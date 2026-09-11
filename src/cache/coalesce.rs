use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;

use crate::cache::metrics::NamespaceMetrics;

type InflightMap = DashMap<u64, Arc<InflightEntry>>;

pub struct CoalesceGuard {
    inflight: Arc<InflightMap>,
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
    map: Arc<InflightMap>,
    finished: bool,
}

impl CoalesceGuard {
    pub fn new() -> Self {
        Self {
            inflight: Arc::new(DashMap::new()),
            wait_timeout: Duration::from_secs(30),
        }
    }

    fn permit(&self, key: u64, writer: bool, inflight: Arc<InflightEntry>) -> CoalescePermit {
        CoalescePermit {
            key,
            writer,
            wait_timeout: self.wait_timeout,
            inflight,
            map: Arc::clone(&self.inflight),
            finished: false,
        }
    }

    pub fn begin(&self, key: u64, metrics: &NamespaceMetrics) -> CoalescePermit {
        loop {
            if let Some(existing) = self.inflight.get(&key)
                && !existing.done.load(Ordering::Acquire)
            {
                metrics.record_coalesced();
                return self.permit(key, false, existing.clone());
            }

            let inflight = Arc::new(InflightEntry {
                notify: Notify::new(),
                done: AtomicBool::new(false),
                success: AtomicBool::new(false),
            });
            match self.inflight.entry(key) {
                dashmap::mapref::entry::Entry::Vacant(vacant) => {
                    vacant.insert(inflight.clone());
                    return self.permit(key, true, inflight);
                }
                dashmap::mapref::entry::Entry::Occupied(occupied) => {
                    if occupied.get().done.load(Ordering::Acquire) {
                        occupied.remove();
                        continue;
                    }
                    metrics.record_coalesced();
                    return self.permit(key, false, occupied.get().clone());
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
        // Remove before notify so a waiter that immediately begins a new fill
        // cannot observe a leftover done entry. remove_if + ptr_eq is
        // idempotent and will not clobber a newer writer for the same key.
        permit.remove_own_entry();
        permit.inflight.notify.notify_waiters();
    }
}

impl CoalescePermit {
    fn remove_own_entry(&self) {
        self.map
            .remove_if(&self.key, |_, current| Arc::ptr_eq(current, &self.inflight));
    }

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

impl Drop for CoalescePermit {
    fn drop(&mut self) {
        // Waiters never own the map slot. The finished writer already removed
        // it in finish(); aborting writers must still unblock waiters and drop
        // the entry so unique-key aborts cannot linger until the next begin().
        if !self.writer || self.finished {
            return;
        }
        if !self.inflight.done.swap(true, Ordering::AcqRel) {
            self.inflight.success.store(false, Ordering::Release);
            self.remove_own_entry();
            self.inflight.notify.notify_waiters();
        } else {
            self.remove_own_entry();
        }
    }
}

#[cfg(test)]
impl CoalesceGuard {
    fn inflight_len(&self) -> usize {
        self.inflight.len()
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
        assert_eq!(guard.inflight_len(), 0);
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
        assert_eq!(guard.inflight_len(), 0);
    }

    #[tokio::test]
    async fn dropped_writer_clears_inflight_on_unique_key_abort() {
        let guard = CoalesceGuard::new();
        let metrics = NamespaceMetrics::default();
        let writer = guard.begin(13, &metrics);
        assert!(writer.is_writer());
        assert_eq!(guard.inflight_len(), 1);
        drop(writer);
        assert_eq!(guard.inflight_len(), 0);

        let next = guard.begin(13, &metrics);
        assert!(next.is_writer());
        assert_eq!(guard.inflight_len(), 1);
        drop(next);
        assert_eq!(guard.inflight_len(), 0);
    }

    #[tokio::test]
    async fn waiter_drop_does_not_remove_a_newer_writer() {
        let guard = CoalesceGuard::new();
        let metrics = NamespaceMetrics::default();
        let writer = guard.begin(17, &metrics);
        let waiter = guard.begin(17, &metrics);
        assert!(!waiter.is_writer());
        guard.finish(writer, true);
        assert_eq!(guard.inflight_len(), 0);

        let next = guard.begin(17, &metrics);
        assert!(next.is_writer());
        assert_eq!(guard.inflight_len(), 1);
        drop(waiter);
        assert_eq!(guard.inflight_len(), 1);
        guard.finish(next, false);
        assert_eq!(guard.inflight_len(), 0);
    }
}
