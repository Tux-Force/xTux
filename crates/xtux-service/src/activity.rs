//! Tracks open connections and running jobs, so the daemon can exit when it has been idle.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct Activity {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    busy: AtomicUsize,
    idle_since: Mutex<Option<Instant>>,
}

/// Held while a connection is open or a job runs.
pub struct Busy {
    inner: Arc<Inner>,
}

impl Activity {
    pub fn new() -> Self {
        let activity = Self::default();
        *activity.inner.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        activity
    }

    pub fn busy(&self) -> Busy {
        self.inner.busy.fetch_add(1, Ordering::SeqCst);
        *self.inner.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Busy {
            inner: self.inner.clone(),
        }
    }

    /// How long nothing has been going on, `None` while busy.
    pub fn idle_for(&self) -> Option<Duration> {
        self.inner
            .idle_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|since| since.elapsed())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if self.inner.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
            *self.inner.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_only_without_busy_guards() {
        let activity = Activity::new();
        assert!(activity.idle_for().is_some());
        let a = activity.busy();
        let b = activity.busy();
        assert!(activity.idle_for().is_none());
        drop(a);
        assert!(activity.idle_for().is_none());
        drop(b);
        assert!(activity.idle_for().is_some());
    }
}
