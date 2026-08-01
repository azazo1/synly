use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl ReconnectPolicy {
    pub fn new(base_delay: Duration, max_delay: Duration) -> Self {
        Self { base_delay, max_delay }
    }
}

#[derive(Debug)]
pub enum AttemptVerdict {
    /// 连接失败, 应增长退避后重试.
    Failed,
    /// 会话已建立并结束, 应重置退避后重试.
    Disconnected,
    /// 终止性失败(如配对终止), 驱动直接返回该错误.
    Terminal(anyhow::Error),
}

/// 一次连接加会话尝试, 由调用方实现.
pub trait ReconnectAttempt {
    fn attempt(&mut self) -> Pin<Box<dyn Future<Output = AttemptVerdict> + Send + '_>>;
}

/// 通用自动重连循环: 反复执行 attempt, 按判定结果增长或重置退避,
/// 等待期间可被 shutdown 取消, 取消时静默退出.
pub async fn run_auto_reconnect<A>(
    policy: ReconnectPolicy,
    shutdown: CancellationToken,
    attempt: &mut A,
) -> Result<()>
where
    A: ReconnectAttempt + ?Sized,
{
    let mut delay = policy.base_delay;
    loop {
        let reset = match attempt.attempt().await {
            AttemptVerdict::Terminal(err) => return Err(err),
            AttemptVerdict::Failed => false,
            AttemptVerdict::Disconnected => true,
        };
        if !wait_for_retry(delay, &shutdown).await {
            return Ok(());
        }
        delay = if reset {
            policy.base_delay
        } else {
            next_delay(delay, policy)
        };
    }
}

async fn wait_for_retry(delay: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

fn next_delay(current: Duration, policy: ReconnectPolicy) -> Duration {
    let next = current.as_secs().saturating_mul(2);
    Duration::from_secs(next.min(policy.max_delay.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::new(Duration::from_secs(2), Duration::from_secs(20))
    }

    struct FixedAttempt(AttemptVerdict);

    impl ReconnectAttempt for FixedAttempt {
        fn attempt(&mut self) -> Pin<Box<dyn Future<Output = AttemptVerdict> + Send + '_>> {
            let verdict = match &self.0 {
                AttemptVerdict::Terminal(_) => {
                    AttemptVerdict::Terminal(anyhow::anyhow!("配对终止"))
                }
                AttemptVerdict::Failed => AttemptVerdict::Failed,
                AttemptVerdict::Disconnected => AttemptVerdict::Disconnected,
            };
            Box::pin(async move { verdict })
        }
    }

    struct CountingAttempt {
        calls: usize,
    }

    impl ReconnectAttempt for CountingAttempt {
        fn attempt(&mut self) -> Pin<Box<dyn Future<Output = AttemptVerdict> + Send + '_>> {
            self.calls += 1;
            Box::pin(async move { AttemptVerdict::Failed })
        }
    }

    #[test]
    fn failed_attempt_doubles_delay_up_to_cap() {
        let mut delay = Duration::from_secs(2);
        assert_eq!(next_delay(delay, policy()), Duration::from_secs(4));
        delay = next_delay(delay, policy());
        assert_eq!(delay, Duration::from_secs(4));
        delay = next_delay(delay, policy());
        assert_eq!(delay, Duration::from_secs(8));
        delay = next_delay(delay, policy());
        assert_eq!(delay, Duration::from_secs(16));
        delay = next_delay(delay, policy());
        assert_eq!(delay, Duration::from_secs(20));
        delay = next_delay(delay, policy());
        assert_eq!(delay, Duration::from_secs(20));
    }

    #[tokio::test]
    async fn terminal_verdict_returns_error_immediately() {
        let mut attempt = FixedAttempt(AttemptVerdict::Terminal(anyhow::anyhow!("配对终止")));
        let result =
            run_auto_reconnect(policy(), CancellationToken::new(), &mut attempt).await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("配对终止"));
    }

    #[tokio::test]
    async fn cancel_stops_loop_without_more_attempts() {
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let mut attempt = CountingAttempt { calls: 0 };
        let task = tokio::spawn(async move {
            let result = run_auto_reconnect(
                ReconnectPolicy::new(Duration::from_secs(60), Duration::from_secs(120)),
                shutdown,
                &mut attempt,
            )
            .await;
            (result, attempt.calls)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_clone.cancel();
        let (result, calls) = task.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(calls, 1);
    }
}
