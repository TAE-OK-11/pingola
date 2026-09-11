use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;

use crate::cache::metrics::NamespaceMetrics;

pub struct CoalesceGuard {
    inflight: DashMap<u64, Arc<InflightEntry>>,
    wait_timeout: Duration,
}

struct InflightEntry {
    notify: Notify,
    done: AtomicBool,
    success: AtomicBool,
}

pub struct CoalescePermit {
    key: u64,
    writer: bool,
    wait_timeout: Duration,
    inflight: Arc<InflightEntry>,
    finished: bool,
}

impl CoalesceGuard {
    pub fn new() -> Self {
        Self {
            inflight: DashMap::new(),
            wait_timeout: Duration::from_secs(30),
        }
    }

    pub fn begin(&self, key: u64, metrics: &NamespaceMetrics) -> CoalescePermit {
        loop {
            if let Some(existing) = self.inflight.get(&key)
                && !existing.done.load(Ordering::Acquire)
            {
                metrics.record_coalesced();
                return CoalescePermit {
                    key,
                    writer: false,
                    wait_timeout: self.wait_timeout,
                    inflight: existing.clone(),
                    finished: false,
                };
            }

            let inflight = Arc::new(InflightEntry {
                notify: Notify::new(),
                done: AtomicBool::new(false),
                success: AtomicBool::new(false),
            });
            match self.inflight.entry(key) {
                dashmap::mapref::entry::Entry::Vacant(vacant) => {
                    vacant.insert(inflight.clone());
                    return CoalescePermit {
                        key,
                        writer: true,
                        wait_timeout: self.wait_timeout,
                        inflight,
                        finished: false,
                    };
                }
                dashmap::mapref::entry::Entry::Occupied(occupied) => {
                    if occupied.get().done.load(Ordering::Acquire) {
                        occupied.remove();
                        continue;
                    }
                    metrics.record_coalesced();
                    return CoalescePermit {
                        key,
                        writer: false,
                        wait_timeout: self.wait_timeout,
                        inflight: occupied.get().clone(),
                        finished: false,
                    };
                }
            }
        }
    }

    pub fn finish(&self, mut permit: CoalescePermit, inserted: bool) {
        if permit.finished {
            return;
        }
        permit.finished = true;
        // Only the writer owns completion. Waiters may hold a permit in request
        // context and later drop/finish it from logging; they must not overwrite
        // the writer's success bit or remove the inflight entry early.
        if !permit.writer {
            return;
        }
        permit.inflight.success.store(inserted, Ordering::Release);
        permit.inflight.done.store(true, Ordering::Release);
        permit.inflight.notify.notify_waiters();
        self.inflight.remove(&permit.key);
    }
}

impl Drop for CoalescePermit {
    fn drop(&mut self) {
        // Writers that abort before finish_fill must unblock coalesced waiters.
        if self.writer && !self.finished && !self.inflight.done.swap(true, Ordering::AcqRel) {
            self.inflight.success.store(false, Ordering::Release);
            self.inflight.notify.notify_waiters();
        }
    }
}

impl CoalescePermit {
    pub fn is_writer(&self) -> bool {
        self.writer
    }

    pub async fn wait_for_writer(&self) -> bool {
        if self.writer {
            return true;
        }
        let deadline = tokio::time::Instant::now() + self.wait_timeout;
        loop {
            if self.inflight.done.load(Ordering::Acquire) {
                return self.inflight.success.load(Ordering::Acquire);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            tokio::select! {
                _ = self.inflight.notify.notified() => {}
                _ = tokio::time::sleep(remaining) => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn coalesced_waiters_observe_writer_result() {
        let guard = Arc::new(CoalesceGuard::new());
        let metrics = NamespaceMetrics::default();
        let writer = guard.begin(7, &metrics);
        assert!(writer.is_writer());
        let waiter = {
            let guard = guard.clone();
            tokio::spawn(async move {
                let waiter = guard.begin(7, &NamespaceMetrics::default());
                assert!(!waiter.is_writer());
                waiter.wait_for_writer().await
            })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        guard.finish(writer, true);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn dropped_writer_unblocks_waiters_as_failure() {
        let guard = Arc::new(CoalesceGuard::new());
        let metrics = NamespaceMetrics::default();
        let writer = guard.begin(11, &metrics);
        assert!(writer.is_writer());
        let waiter = {
            let guard = guard.clone();
            tokio::spawn(async move {
                let waiter = guard.begin(11, &NamespaceMetrics::default());
                assert!(!waiter.is_writer());
                waiter.wait_for_writer().await
            })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        drop(writer);
        assert!(!waiter.await.unwrap());
    }
}
