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
                    };
                }
            }
        }
    }

    pub fn finish(&self, permit: CoalescePermit, inserted: bool) {
        permit.inflight.success.store(inserted, Ordering::Release);
        permit.inflight.done.store(true, Ordering::Release);
        permit.inflight.notify.notify_waiters();
        if permit.writer {
            self.inflight.remove(&permit.key);
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
}
