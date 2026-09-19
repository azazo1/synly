use super::*;
use crate::audio::config::CodecConfig;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ScriptInput {
    steps: VecDeque<Result<CaptureStatus>>,
    value: f32,
    destroyed: Arc<AtomicUsize>,
}

impl Drop for ScriptInput {
    fn drop(&mut self) { self.destroyed.fetch_add(1, Ordering::SeqCst); }
}

impl AudioInput for ScriptInput {
    fn read_frame(&mut self, frame: &mut [f32], _timeout: Duration) -> Result<CaptureStatus> {
        let result = self.steps.pop_front().expect("读取超出预设步骤");
        frame.fill(if matches!(result, Ok(CaptureStatus::Ok)) { self.value } else { f32::NAN });
        result
    }
}

#[tokio::test]
async fn initial_absence_and_read_failure_recover_without_emitting_partial_frames() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let samples = Arc::new(FrameQueue::new("capture-recovery", 30));
    let worker_samples = Arc::clone(&samples);
    let destroyed = Arc::new(AtomicUsize::new(0));
    let worker_destroyed = Arc::clone(&destroyed);
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker_attempts = Arc::clone(&attempts);
    let task = tokio::task::spawn_blocking(move || run_with_retry_delay(move || {
        let attempt = worker_attempts.fetch_add(1, Ordering::SeqCst);
        match attempt {
            0 => Err(Error::Backend("初始没有设备".into())),
            1 => Ok(Box::new(ScriptInput {
                steps: VecDeque::from([Ok(CaptureStatus::Timeout), Ok(CaptureStatus::Ok), Err(Error::Backend("设备失效".into()))]),
                value: 1.0, destroyed: Arc::clone(&worker_destroyed),
            }) as Box<dyn AudioInput>),
            2 => {
                assert_eq!(worker_destroyed.load(Ordering::SeqCst), 1);
                Ok(Box::new(ScriptInput {
                    steps: VecDeque::from([Ok(CaptureStatus::Ok), Err(Error::InvalidConfig("结束测试"))]),
                    value: 2.0, destroyed: Arc::clone(&worker_destroyed),
                }))
            }
            _ => panic!("永久错误不应重试"),
        }
    }, worker_samples, CancellationToken::new(), &stream, Duration::from_millis(10)));
    let result = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(destroyed.load(Ordering::SeqCst), 2);
    assert_eq!(samples.len(), 2);
    for value in [1.0, 2.0] {
        assert!(samples.pop_blocking().unwrap().iter().all(|sample| *sample == value));
    }
}

#[tokio::test]
async fn cancellation_interrupts_the_five_second_retry_wait() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let samples = Arc::new(FrameQueue::new("cancel-capture-retry", 30));
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker_attempts = Arc::clone(&attempts);
    let task = tokio::task::spawn_blocking(move || run(move || {
        worker_attempts.fetch_add(1, Ordering::SeqCst);
        entered.send(()).unwrap();
        Err(Error::Io(std::io::Error::other("捕获设备暂时不可用")))
    }, samples, worker_stop, &stream));
    tokio::time::timeout(Duration::from_secs(2), entered_rx.recv()).await.unwrap().unwrap();
    stop.cancel();
    tokio::time::timeout(Duration::from_millis(500), task).await.unwrap().unwrap().unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_open_releases_device_without_reading() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let samples = Arc::new(FrameQueue::new("cancel-capture-open", 30));
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let destroyed = Arc::new(AtomicUsize::new(0));
    let worker_destroyed = Arc::clone(&destroyed);
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut entered = Some(entered);
    let task = tokio::task::spawn_blocking(move || run(move || {
        entered.take().unwrap().send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        Ok(Box::new(ScriptInput { steps: VecDeque::new(), value: 0.0, destroyed: Arc::clone(&worker_destroyed) }))
    }, samples, worker_stop, &stream));
    tokio::time::timeout(Duration::from_secs(2), entered_rx).await.unwrap().unwrap();
    stop.cancel();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

struct DelayedInput {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl AudioInput for DelayedInput {
    fn read_frame(&mut self, frame: &mut [f32], _timeout: Duration) -> Result<CaptureStatus> {
        self.entered.take().unwrap().send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(2)).unwrap();
        frame.fill(9.0);
        Ok(CaptureStatus::Ok)
    }
}

#[tokio::test]
async fn frame_completed_after_cancellation_is_not_encoded() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let samples = Arc::new(FrameQueue::new("cancel-capture-read", 30));
    let worker_samples = Arc::clone(&samples);
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut input = Some(DelayedInput { entered: Some(entered), release: release_rx });
    let task = tokio::task::spawn_blocking(move || run(move || Ok(Box::new(input.take().unwrap())), worker_samples, worker_stop, &stream));
    tokio::time::timeout(Duration::from_secs(2), entered_rx).await.unwrap().unwrap();
    stop.cancel();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert_eq!(samples.len(), 0);
}
