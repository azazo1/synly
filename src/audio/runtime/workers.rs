use super::queue::{FrameQueue, StopQueue};
use anyhow::{Context, Result};
use std::future::Future;
use std::sync::Arc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

/// 一条音频链路的所有工作任务共同停止, 并在会话返回前回收.
pub(super) struct Workers {
    stop: CancellationToken,
    tasks: JoinSet<Result<()>>,
    queues: Vec<Arc<dyn StopQueue>>,
}

impl Workers {
    pub fn new(stop: CancellationToken) -> Self {
        Self { stop, tasks: JoinSet::new(), queues: Vec::new() }
    }

    pub fn queue<T: Send + 'static>(&mut self, name: &'static str, capacity: usize) -> Arc<FrameQueue<T>> {
        let queue = Arc::new(FrameQueue::new(name, capacity));
        self.queues.push(Arc::clone(&queue) as Arc<dyn StopQueue>);
        queue
    }

    pub fn spawn(&mut self, task: impl Future<Output = Result<()>> + Send + 'static) {
        self.tasks.spawn(task);
    }

    pub fn spawn_blocking(&mut self, task: impl FnOnce() -> Result<()> + Send + 'static) {
        self.tasks.spawn_blocking(task);
    }

    fn shutdown(&self) {
        self.stop.cancel();
        for queue in &self.queues {
            queue.close();
        }
    }

    pub async fn finish(mut self) -> Result<()> {
        // 任一阶段错误, panic 或提前结束都停止其他阶段, 不留孤立的采集线程.
        let mut result = tokio::select! {
            _ = self.stop.cancelled() => Ok(()),
            task = self.tasks.join_next() => task.map(join_result).unwrap_or(Ok(())),
        };
        self.shutdown();
        while let Some(task) = self.tasks.join_next().await {
            if let Err(error) = join_result(task) {
                if result.is_ok() {
                    result = Err(error);
                } else {
                    tracing::debug!(%error, "音频停止期间工作任务退出失败");
                }
            }
        }
        result
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        // 即使外层异步任务被取消, 阻塞线程也会被队列关闭和停止信号唤醒.
        self.shutdown();
    }
}

fn join_result(task: std::result::Result<Result<()>, JoinError>) -> Result<()> {
    task.context("音频工作任务异常退出")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn worker_failure_stops_and_joins_blocked_sibling() {
        let stop = CancellationToken::new();
        let mut workers = Workers::new(stop.clone());
        let queue = workers.queue::<u8>("test", 30);
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_worker = Arc::clone(&stopped);
        workers.spawn_blocking(move || {
            assert!(queue.pop_blocking().is_none());
            stopped_worker.store(true, Ordering::Release);
            Ok(())
        });
        workers.spawn(async { anyhow::bail!("测试失败") });
        assert!(tokio::time::timeout(Duration::from_secs(1), workers.finish()).await.unwrap().is_err());
        assert!(stop.is_cancelled());
        assert!(stopped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn worker_panic_wakes_async_queue_consumer() {
        let stop = CancellationToken::new();
        let mut workers = Workers::new(stop.clone());
        let queue = workers.queue::<u8>("test", 30);
        workers.spawn(async move {
            assert!(queue.pop().await.is_none());
            Ok(())
        });
        workers.spawn_blocking(|| panic!("测试工作线程 panic"));
        assert!(tokio::time::timeout(Duration::from_secs(1), workers.finish()).await.unwrap().is_err());
        assert!(stop.is_cancelled());
    }

    #[tokio::test]
    async fn external_cancellation_wakes_empty_queue() {
        let stop = CancellationToken::new();
        let mut workers = Workers::new(stop.clone());
        let queue = workers.queue::<u8>("test", 30);
        workers.spawn_blocking(move || {
            assert!(queue.pop_blocking().is_none());
            Ok(())
        });
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), workers.finish()).await.unwrap().unwrap();
    }
}
