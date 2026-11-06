//! A bounded first-in, first-out queue for one consumer. When full, the
//! oldest item is dropped, so a stalled worker can't grow memory forever.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

pub struct Queue<T> {
    name: &'static str,
    cap: usize,
    items: Mutex<VecDeque<T>>,
    dropped: AtomicU64,
    ready: Notify,
}

impl<T> Queue<T> {
    pub fn new(name: &'static str, cap: usize) -> Self {
        Self { name, cap, items: Mutex::default(), dropped: AtomicU64::new(0), ready: Notify::new() }
    }

    /// Add an item, dropping the oldest if full. Returns what was dropped.
    pub fn push(&self, item: T) -> Option<T> {
        let dropped = {
            let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
            items.push_back(item);
            (items.len() > self.cap).then(|| items.pop_front()).flatten()
        };
        self.ready.notify_one();
        if dropped.is_some() {
            self.warn_dropped();
        }
        dropped
    }

    /// Wait for the next item.
    pub async fn pop(&self) -> T {
        loop {
            if let Some(x) = self.items.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                return x;
            }
            self.ready.notified().await;
        }
    }

    /// Warn on the 1st, 2nd, 4th, 8th… drop, so a flood doesn't flood the log too.
    fn warn_dropped(&self) {
        let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            tracing::warn!(queue = self.name, dropped = n, cap = self.cap, "queue full; dropped the oldest item");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drops_the_oldest_when_full() {
        let q = Queue::new("test", 2);
        assert_eq!(q.push(1), None);
        assert_eq!(q.push(2), None);
        assert_eq!(q.push(3), Some(1));
        assert_eq!(q.pop().await, 2);
        assert_eq!(q.pop().await, 3);
    }

    #[tokio::test]
    async fn pop_waits_for_a_push() {
        let q = std::sync::Arc::new(Queue::new("test", 4));
        let waiter = tokio::spawn({
            let q = q.clone();
            async move { q.pop().await }
        });
        tokio::task::yield_now().await;
        q.push(7);
        assert_eq!(waiter.await.unwrap(), 7);
    }
}
