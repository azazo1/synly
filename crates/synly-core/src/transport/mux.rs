//! TLS 内的小包复用, 各通道独立接收窗口, 输入优先但大载荷不永久饥饿.

use super::stream::ByteStream;
use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const FRAGMENT_BYTES: usize = 1024;

const LANES: usize = 3;
const WINDOW_PACKETS: usize = 16;
const STREAM_BUFFER_BYTES: usize = FRAGMENT_BYTES * WINDOW_PACKETS;
const PRIORITY_BURST: usize = 8;
const DATA: u8 = 0;
const CREDIT: u8 = 1;
const FINISH: u8 = 2;

type ReceiveWindows = Arc<[AtomicUsize; LANES]>;

pub struct MuxChannels {
    pub control: ByteStream,
    pub input: ByteStream,
    pub clipboard: ByteStream,
}

/// 必须与通道一起持有. 丢弃时取消整个承载连接, 不留下后台 IO 或按键通道.
pub struct MuxGuard {
    shutdown: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    failure: watch::Receiver<Option<String>>,
}

impl MuxGuard {
    pub fn failure(&self) -> watch::Receiver<Option<String>> {
        self.failure.clone()
    }
}

impl Drop for MuxGuard {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

enum WindowEvent {
    GrantPeer(usize),
    PeerCredit(usize),
}

/// 只能传入已经认证并加密的连接. 所有帧在进入 TLS 写入器前被拆成小片段.
pub fn open(stream: ByteStream) -> (MuxChannels, MuxGuard) {
    let shutdown = CancellationToken::new();
    let (failure_tx, failure) = watch::channel(None);
    let (events_tx, events_rx) = mpsc::channel(WINDOW_PACKETS * LANES * 2);
    let windows = Arc::new(std::array::from_fn(|_| AtomicUsize::new(WINDOW_PACKETS)));
    let (reader, writer) = tokio::io::split(stream);
    let mut tasks = Vec::with_capacity(5);
    let pairs: [(DuplexStream, DuplexStream); LANES] =
        std::array::from_fn(|_| tokio::io::duplex(STREAM_BUFFER_BYTES));
    let mut apps = Vec::with_capacity(LANES);
    let mut lane_readers = Vec::with_capacity(LANES);
    let mut deliveries = Vec::with_capacity(LANES);
    for (lane, (app, tunnel)) in pairs.into_iter().enumerate() {
        apps.push(ByteStream::new(app));
        let (lane_reader, lane_writer) = tokio::io::split(tunnel);
        lane_readers.push(lane_reader);
        let (delivery_tx, delivery_rx) = mpsc::channel(WINDOW_PACKETS);
        deliveries.push(Some(delivery_tx));
        tasks.push(spawn_task(
            deliver(lane, lane_writer, delivery_rx, events_tx.clone()),
            shutdown.clone(),
            failure_tx.clone(),
        ));
    }
    let lane_readers = lane_readers.try_into().unwrap_or_else(|_| unreachable!());
    let deliveries = deliveries.try_into().unwrap_or_else(|_| unreachable!());
    tasks.push(spawn_task(
        receive(reader, deliveries, events_tx, windows.clone()),
        shutdown.clone(),
        failure_tx.clone(),
    ));
    tasks.push(spawn_task(
        schedule(writer, lane_readers, events_rx, windows),
        shutdown.clone(),
        failure_tx,
    ));
    let [control, input, clipboard] = apps.try_into().unwrap_or_else(|_| unreachable!());
    (
        MuxChannels { control, input, clipboard },
        MuxGuard { shutdown, tasks, failure },
    )
}

fn spawn_task(
    work: impl Future<Output = Result<()>> + Send + 'static,
    shutdown: CancellationToken,
    failure: watch::Sender<Option<String>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            result = work => result,
        };
        if let Err(error) = result {
            tracing::debug!(%error, "承载复用连接已关闭");
            failure.send_if_modified(|value| {
                if value.is_none() {
                    *value = Some(format!("{error:#}"));
                    true
                } else {
                    false
                }
            });
            shutdown.cancel();
        }
    })
}

async fn deliver(
    lane: usize,
    mut writer: WriteHalf<DuplexStream>,
    mut data: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<WindowEvent>,
) -> Result<()> {
    while let Some(fragment) = data.recv().await {
        writer.write_all(&fragment).await?;
        events.send(WindowEvent::GrantPeer(lane)).await?;
    }
    writer.shutdown().await?;
    Ok(())
}

async fn receive(
    mut reader: ReadHalf<ByteStream>,
    mut deliveries: [Option<mpsc::Sender<Vec<u8>>>; LANES],
    events: mpsc::Sender<WindowEvent>,
    windows: ReceiveWindows,
) -> Result<()> {
    loop {
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).await.context("承载连接断开")?;
        let lane = usize::from(header[0]);
        if lane >= LANES {
            bail!("无效的复用通道");
        }
        let size = usize::from(u16::from_be_bytes([header[2], header[3]]));
        match (header[1], size) {
            (DATA, 1..=FRAGMENT_BYTES) => {
                let delivery = deliveries[lane].as_ref().context("已结束的复用通道收到数据")?;
                // 只有此读取任务消耗窗口, 写入任务只归还信用.
                if windows[lane].load(Ordering::SeqCst) == 0 {
                    bail!("复用通道超出接收窗口");
                }
                windows[lane].fetch_sub(1, Ordering::SeqCst);
                let mut fragment = vec![0; size];
                reader.read_exact(&mut fragment).await?;
                delivery.try_send(fragment).context("复用通道接收队列已满或已关闭")?;
            }
            (CREDIT, 0) => events.send(WindowEvent::PeerCredit(lane)).await?,
            (FINISH, 0) => {
                if deliveries[lane].take().is_none() {
                    bail!("复用通道重复结束");
                }
            }
            _ => bail!("无效的复用帧类型或长度"),
        }
    }
}

async fn schedule(
    mut writer: WriteHalf<ByteStream>,
    readers: [ReadHalf<DuplexStream>; LANES],
    mut events: mpsc::Receiver<WindowEvent>,
    windows: ReceiveWindows,
) -> Result<()> {
    let [mut control, mut input, mut clipboard] = readers;
    let mut credits = [WINDOW_PACKETS; LANES];
    let mut open = [true; LANES];
    let mut burst = 0usize;
    let mut control_data = [0u8; FRAGMENT_BYTES];
    let mut input_data = [0u8; FRAGMENT_BYTES];
    let mut clipboard_data = [0u8; FRAGMENT_BYTES];
    loop {
        enum Ready {
            Event(WindowEvent),
            Lane(usize, std::io::Result<usize>),
        }
        let ready = if burst >= PRIORITY_BURST {
            tokio::select! {
                biased;
                Some(event) = events.recv() => Ready::Event(event),
                n = clipboard.read(&mut clipboard_data), if open[2] && credits[2] > 0 => Ready::Lane(2, n),
                n = control.read(&mut control_data), if open[0] && credits[0] > 0 => Ready::Lane(0, n),
                n = input.read(&mut input_data), if open[1] && credits[1] > 0 => Ready::Lane(1, n),
            }
        } else {
            tokio::select! {
                biased;
                Some(event) = events.recv() => Ready::Event(event),
                n = control.read(&mut control_data), if open[0] && credits[0] > 0 => Ready::Lane(0, n),
                n = input.read(&mut input_data), if open[1] && credits[1] > 0 => Ready::Lane(1, n),
                n = clipboard.read(&mut clipboard_data), if open[2] && credits[2] > 0 => Ready::Lane(2, n),
            }
        };
        match ready {
            Ready::Event(WindowEvent::PeerCredit(lane)) => {
                if credits[lane] >= WINDOW_PACKETS {
                    bail!("对端发送了超出窗口的信用确认");
                }
                credits[lane] += 1;
            }
            Ready::Event(WindowEvent::GrantPeer(lane)) => {
                let previous = windows[lane].fetch_add(1, Ordering::SeqCst);
                if previous >= WINDOW_PACKETS {
                    bail!("接收窗口信用重复释放");
                }
                packet(&mut writer, lane, CREDIT, &[]).await?;
            }
            Ready::Lane(lane, n) => {
                let n = n?;
                if n == 0 {
                    open[lane] = false;
                    packet(&mut writer, lane, FINISH, &[]).await?;
                    continue;
                }
                credits[lane] -= 1;
                let data = match lane {
                    0 => &control_data[..n],
                    1 => &input_data[..n],
                    _ => &clipboard_data[..n],
                };
                packet(&mut writer, lane, DATA, data).await?;
                if lane == 2 {
                    burst = 0;
                } else {
                    burst = burst.saturating_add(1);
                }
            }
        }
    }
}

async fn packet(writer: &mut WriteHalf<ByteStream>, lane: usize, kind: u8, data: &[u8]) -> Result<()> {
    let mut packet = Vec::with_capacity(4 + data.len());
    packet.extend_from_slice(&[lane as u8, kind]);
    packet.extend_from_slice(&(data.len() as u16).to_be_bytes());
    packet.extend_from_slice(data);
    writer.write_all(&packet).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stalled_clipboard_does_not_block_input_or_control() {
        let (a, b) = tokio::io::duplex(2048);
        let (mut a, _a_guard) = open(ByteStream::new(a));
        let (mut b, _b_guard) = open(ByteStream::new(b));
        let payload = vec![0xa7; 512 * 1024];
        let expected = payload.clone();
        let bulk = tokio::spawn(async move {
            a.clipboard.write_all(&payload).await.unwrap();
            a.clipboard.shutdown().await.unwrap();
        });
        a.input.write_all(&[1, 2, 3, 4]).await.unwrap();
        a.control.write_all(&[9, 8, 7]).await.unwrap();
        let mut input = [0; 4];
        let mut control = [0; 3];
        tokio::time::timeout(Duration::from_secs(1), async {
            b.input.read_exact(&mut input).await.unwrap();
            b.control.read_exact(&mut control).await.unwrap();
        }).await.unwrap();
        assert_eq!(input, [1, 2, 3, 4]);
        assert_eq!(control, [9, 8, 7]);
        assert!(!bulk.is_finished());
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), b.clipboard.read_to_end(&mut received)).await.unwrap().unwrap();
        bulk.await.unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn forged_window_credit_closes_all_channels() {
        let (a, mut peer) = tokio::io::duplex(2048);
        let (mut channels, guard) = open(ByteStream::new(a));
        let mut failure = guard.failure();
        peer.write_all(&[0, CREDIT, 0, 0]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), failure.changed()).await.unwrap().unwrap();
        assert!(failure.borrow().is_some());
        let mut byte = [0];
        assert_eq!(channels.input.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dropping_guard_releases_channel_readers() {
        let (a, _peer) = tokio::io::duplex(2048);
        let (mut channels, guard) = open(ByteStream::new(a));
        drop(guard);
        let mut byte = [0];
        let n = tokio::time::timeout(Duration::from_secs(1), channels.input.read(&mut byte)).await.unwrap().unwrap();
        assert_eq!(n, 0);
    }
}
