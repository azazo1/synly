use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use tokio::sync::Notify;

/// Sunshine queue_t 与 Moonlight queuePacketToLbq 的有界积压策略.
/// 满队列清空旧帧后接受最新帧, 生产者从不等待播放或编码消费.
/// 每个队列只有一个消费者, 可以使用阻塞或异步接口.
pub(super) struct FrameQueue<T> {
    name: &'static str,
    capacity: usize,
    state: Mutex<State<T>>,
    available: Condvar,
    ready: Notify,
}

struct State<T> {
    items: VecDeque<T>,
    closed: bool,
    dropped: u64,
    high_water: usize,
}

pub(super) trait StopQueue: Send + Sync {
    fn close(&self);
}

impl<T> FrameQueue<T> {
    pub fn new(name: &'static str, capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            name,
            capacity,
            state: Mutex::new(State {
                items: VecDeque::with_capacity(capacity),
                closed: false,
                dropped: 0,
                high_water: 0,
            }),
            available: Condvar::new(),
            ready: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn push(&self, item: T) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        if state.items.len() == self.capacity {
            state.dropped += state.items.len() as u64;
            state.items.clear();
        }
        state.items.push_back(item);
        state.high_water = state.high_water.max(state.items.len());
        drop(state);
        self.available.notify_one();
        self.ready.notify_one();
        true
    }

    pub fn pop_blocking(&self) -> Option<T> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(item) = state.items.pop_front() {
                return Some(item);
            }
            state = self.available.wait(state).unwrap_or_else(|error| error.into_inner());
        }
    }

    pub async fn pop(&self) -> Option<T> {
        loop {
            // notify_one 保留许可, 使检查队列与进入 await 之间的通知不会丢失.
            let notified = self.ready.notified();
            {
                let mut state = self.lock();
                if state.closed {
                    return None;
                }
                if let Some(item) = state.items.pop_front() {
                    return Some(item);
                }
            }
            notified.await;
        }
    }

    // 设备恢复时持续丢弃旧音频, 等待固定截止时间或监督器关闭队列.
    // 使用同一把锁清空队列并进入条件变量等待, 不丢失取消通知.
    pub fn discard_until(&self, deadline: std::time::Instant) -> bool {
        let mut state = self.lock();
        loop {
            state.dropped = state.dropped.saturating_add(state.items.len() as u64);
            state.items.clear();
            if state.closed { return false; }
            let now = std::time::Instant::now();
            if now >= deadline { return true; }
            let (next, _) = self.available.wait_timeout(state, deadline - now)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
        }
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

impl<T: Send> StopQueue for FrameQueue<T> {
    fn close(&self) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        state.items.clear();
        tracing::debug!(
            queue = self.name,
            dropped_frames = state.dropped,
            high_water = state.high_water,
            capacity = self.capacity,
            "音频队列已停止"
        );
        drop(state);
        self.available.notify_all();
        self.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn overflow_keeps_only_the_latest_frame() {
        let queue = FrameQueue::new("test", 30);
        for value in 0..30 {
            assert!(queue.push(value));
        }
        assert_eq!(queue.len(), 30);
        assert!(queue.push(30));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop().await, Some(30));
        assert_eq!(queue.lock().dropped, 30);
    }

    #[tokio::test]
    async fn close_wakes_async_consumer_and_discards_pending_frames() {
        let queue = Arc::new(FrameQueue::<u8>::new("test", 30));
        let consumer_queue = Arc::clone(&queue);
        let consumer = tokio::spawn(async move { consumer_queue.pop().await });
        tokio::task::yield_now().await;
        queue.close();
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), consumer).await.unwrap().unwrap(), None);
        assert!(!queue.push(1));
        let queue = FrameQueue::new("pending", 30);
        queue.push(1);
        queue.close();
        assert_eq!(queue.pop().await, None);
    }

    #[test]
    fn expired_recovery_window_flushes_backlog_but_accepts_future_frames() {
        let queue = FrameQueue::new("recovery", 30);
        queue.push(1);
        queue.push(2);
        assert!(queue.discard_until(std::time::Instant::now()));
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.dropped(), 2);
        queue.push(3);
        assert_eq!(queue.pop_blocking(), Some(3));
    }

    #[tokio::test]
    async fn recovery_window_ends_on_time_without_any_network_packets() {
        let queue = Arc::new(FrameQueue::<u8>::new("recovery", 30));
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(30);
        let waiting = tokio::task::spawn_blocking(move || queue.discard_until(deadline));
        assert!(tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap());
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn closing_interrupts_a_long_recovery_drop_window() {
        let queue = Arc::new(FrameQueue::<u8>::new("recovery", 30));
        let worker_queue = Arc::clone(&queue);
        let waiting = tokio::task::spawn_blocking(move || worker_queue.discard_until(std::time::Instant::now() + Duration::from_secs(60)));
        queue.close();
        assert!(!tokio::time::timeout(Duration::from_millis(500), waiting).await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn close_wakes_blocking_consumer() {
        let queue = Arc::new(FrameQueue::<u8>::new("test", 30));
        let consumer_queue = Arc::clone(&queue);
        let consumer = tokio::task::spawn_blocking(move || consumer_queue.pop_blocking());
        queue.close();
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), consumer).await.unwrap().unwrap(), None);
    }
}
