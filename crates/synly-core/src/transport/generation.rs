//! 复用通道内按随机代次重建子流, 旧代次数据不进入新的输入运行时.

use super::{mux::FRAGMENT_BYTES, stream::ByteStream};
use anyhow::{Context, Result, bail};
use std::{io, pin::Pin, sync::{Arc, Mutex}, task::{Context as TaskContext, Poll}};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WINDOW: usize = 16;
const DATA: u8 = 0;
const END: u8 = 1;
struct Record { id: Uuid, sequence: u64, data: Vec<u8>, end: bool }
struct Lease {
    id: Uuid,
    received: u64,
    delivery: Option<mpsc::Sender<Vec<u8>>>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Lease {
    fn drop(&mut self) { self.stop.cancel(); for task in &self.tasks { task.abort(); } }
}

#[derive(Clone)]
pub struct GenerationLane {
    active: Arc<Mutex<Option<Lease>>>,
    records: mpsc::Sender<Record>,
    shutdown: CancellationToken,
}
pub struct GenerationGuard {
    lane: GenerationLane,
    tasks: Vec<JoinHandle<()>>,
    failure: watch::Receiver<Option<String>>,
}
impl GenerationGuard {
    pub fn failure(&self) -> watch::Receiver<Option<String>> { self.failure.clone() }
}
impl Drop for GenerationGuard {
    fn drop(&mut self) {
        self.lane.shutdown.cancel();
        self.lane.active.lock().unwrap_or_else(|error| error.into_inner()).take();
        for task in &self.tasks { task.abort(); }
    }
}

pub fn open(stream: ByteStream) -> (GenerationLane, GenerationGuard) {
    let shutdown = CancellationToken::new();
    let active = Arc::new(Mutex::new(None));
    let (records, outgoing) = mpsc::channel(WINDOW);
    let lane = GenerationLane { active: active.clone(), records, shutdown: shutdown.clone() };
    let (reader, writer) = tokio::io::split(stream);
    let (failure_tx, failure) = watch::channel(None);
    let tasks = vec![
        spawn(receive(reader, active), shutdown.clone(), failure_tx.clone()),
        spawn(send(writer, outgoing), shutdown, failure_tx),
    ];
    (lane.clone(), GenerationGuard { lane, tasks, failure })
}
fn spawn(work: impl Future<Output = Result<()>> + Send + 'static, stop: CancellationToken, failure: watch::Sender<Option<String>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = tokio::select! { biased; _ = stop.cancelled() => return, result = work => result };
        if let Err(error) = result {
            failure.send_if_modified(|value| { if value.is_none() { *value = Some(format!("{error:#}")); true } else { false } });
            stop.cancel();
        }
    })
}
impl GenerationLane {
    /// 必须先在已认证控制通道中协商代次. 两端创建相同 UUID 的租约后才可启动输入.
    pub fn lease(&self, id: Uuid) -> Result<ByteStream> {
        if id.is_nil() || self.shutdown.is_cancelled() { bail!("子流代次为空或承载已关闭"); }
        let mut active = self.active.lock().unwrap_or_else(|error| error.into_inner());
        if active.as_ref().is_some_and(|lease| lease.id == id) { bail!("不能重复使用子流代次"); }
        // 先同步撤销旧租约, 已缓存的旧事件也不能继续被读取或发送.
        active.take();
        let (app, tunnel) = tokio::io::duplex(FRAGMENT_BYTES * WINDOW);
        let (mut reader, mut writer) = tokio::io::split(tunnel);
        let (delivery, mut incoming) = mpsc::channel::<Vec<u8>>(WINDOW);
        let stop = self.shutdown.child_token();
        let read_stop = stop.clone();
        let records = self.records.clone();
        let send_task = tokio::spawn(async move {
            let mut sequence = 0u64;
            let mut data = [0; FRAGMENT_BYTES];
            loop {
                let read = tokio::select! { biased; _ = read_stop.cancelled() => break, read = reader.read(&mut data) => read };
                let Ok(size) = read else { break };
                if size == 0 { break; }
                let Some(next) = sequence.checked_add(1) else { break };
                let record = Record { id, sequence: next, data: data[..size].to_vec(), end: false };
                let sent = tokio::select! { biased; _ = read_stop.cancelled() => break, result = records.send(record) => result };
                if sent.is_err() { return; }
                sequence = next;
            }
            if let Some(sequence) = sequence.checked_add(1) {
                let _ = records.send(Record { id, sequence, data: Vec::new(), end: true }).await;
            }
        });
        let deliver_stop = stop.clone();
        let deliver_task = tokio::spawn(async move {
            loop {
                let data = tokio::select! { biased; _ = deliver_stop.cancelled() => return, data = incoming.recv() => data };
                let Some(data) = data else { let _ = writer.shutdown().await; return };
                let result = tokio::select! { biased; _ = deliver_stop.cancelled() => return, result = writer.write_all(&data) => result };
                if result.is_err() { return; }
            }
        });
        *active = Some(Lease { id, received: 0, delivery: Some(delivery), stop: stop.clone(), tasks: vec![send_task, deliver_task] });
        Ok(ByteStream::new(LeaseStream { inner: app, stop, closed: false }))
    }
    pub fn revoke(&self) { self.active.lock().unwrap_or_else(|error| error.into_inner()).take(); }
}

struct LeaseStream { inner: DuplexStream, stop: CancellationToken, closed: bool }
impl Drop for LeaseStream { fn drop(&mut self) { if !self.closed { self.stop.cancel(); } } }
impl AsyncRead for LeaseStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.stop.is_cancelled() { Poll::Ready(Ok(())) } else { Pin::new(&mut self.inner).poll_read(cx, buf) }
    }
}
impl AsyncWrite for LeaseStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.stop.is_cancelled() { Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())) } else { Pin::new(&mut self.inner).poll_write(cx, buf) }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> { Pin::new(&mut self.inner).poll_flush(cx) }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(result, Poll::Ready(Ok(()))) { self.closed = true; }
        result
    }
}
async fn send(mut writer: impl AsyncWrite + Unpin, mut records: mpsc::Receiver<Record>) -> Result<()> {
    while let Some(record) = records.recv().await {
        let mut packet = Vec::with_capacity(27 + record.data.len());
        packet.extend_from_slice(record.id.as_bytes());
        packet.extend_from_slice(&record.sequence.to_be_bytes());
        packet.push(if record.end { END } else { DATA });
        packet.extend_from_slice(&(record.data.len() as u16).to_be_bytes());
        packet.extend_from_slice(&record.data);
        writer.write_all(&packet).await?;
        writer.flush().await?;
    }
    Ok(())
}
async fn receive(mut reader: impl AsyncRead + Unpin, active: Arc<Mutex<Option<Lease>>>) -> Result<()> {
    loop {
        let mut header = [0; 27];
        reader.read_exact(&mut header).await.context("子流承载断开")?;
        let id = Uuid::from_slice(&header[..16])?;
        let sequence = u64::from_be_bytes(header[16..24].try_into().unwrap());
        let end = match header[24] { DATA => false, END => true, _ => bail!("无效的子流帧类型") };
        let size = usize::from(u16::from_be_bytes(header[25..27].try_into().unwrap()));
        if size > FRAGMENT_BYTES || end != (size == 0) || id.is_nil() || sequence == 0 { bail!("无效的子流帧长度或代次"); }
        let mut data = vec![0; size];
        reader.read_exact(&mut data).await?;
        let target = {
            let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
            let Some(lease) = active.as_mut().filter(|lease| lease.id == id && !lease.stop.is_cancelled()) else { continue };
            if lease.received.checked_add(1) != Some(sequence) { bail!("子流序号重复或乱序"); }
            lease.received = sequence;
            if end { if lease.delivery.take().is_none() { bail!("子流重复结束"); } None }
            else { Some((lease.delivery.as_ref().context("已结束的子流收到数据")?.clone(), lease.stop.clone())) }
        };
        if let Some((delivery, stop)) = target {
            // 只反压此输入 lane, 不阻塞其他 lane. 撤销旧代次会立即打断等待.
            tokio::select! { biased; _ = stop.cancelled() => {}, _ = delivery.send(data) => {} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    async fn record(peer: &mut DuplexStream, id: Uuid, sequence: u64, data: &[u8]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(id.as_bytes()); bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.push(DATA); bytes.extend_from_slice(&(data.len() as u16).to_be_bytes()); bytes.extend_from_slice(data);
        peer.write_all(&bytes).await.unwrap();
    }
    #[tokio::test]
    async fn replacing_lease_discards_cached_and_late_motion() {
        let (stream, mut peer) = tokio::io::duplex(2048);
        let (lane, guard) = open(ByteStream::new(stream));
        let first = Uuid::new_v4(); let second = Uuid::new_v4();
        let mut old = lane.lease(first).unwrap();
        record(&mut peer, first, 1, &[1, 2, 3, 4]).await;
        let mut byte = [0]; old.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [1]);
        let mut current = lane.lease(second).unwrap();
        assert_eq!(old.read(&mut byte).await.unwrap(), 0);
        assert!(old.write_all(&[5]).await.is_err());
        record(&mut peer, first, 2, &[9, 9]).await;
        record(&mut peer, second, 1, &[7, 8]).await;
        let mut data = [0; 2];
        tokio::time::timeout(Duration::from_secs(1), current.read_exact(&mut data)).await.unwrap().unwrap();
        assert_eq!(data, [7, 8]);
        assert!(guard.failure().borrow().is_none());
    }
    #[tokio::test]
    async fn replay_sequence_closes_current_input_stream() {
        let (stream, mut peer) = tokio::io::duplex(2048);
        let (lane, guard) = open(ByteStream::new(stream));
        let id = Uuid::new_v4(); let mut app = lane.lease(id).unwrap();
        let mut failure = guard.failure();
        record(&mut peer, id, 1, &[1]).await;
        let mut byte = [0]; app.read_exact(&mut byte).await.unwrap();
        record(&mut peer, id, 1, &[2]).await;
        tokio::time::timeout(Duration::from_secs(1), failure.changed()).await.unwrap().unwrap();
        assert!(failure.borrow().is_some());
        assert_eq!(app.read(&mut byte).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn slow_consumer_applies_backpressure_without_closing_lane() {
        let (a, b) = tokio::io::duplex(2048);
        let (a, _a_guard) = open(ByteStream::new(a)); let (b, b_guard) = open(ByteStream::new(b));
        let id = Uuid::new_v4(); let mut sender = a.lease(id).unwrap(); let mut receiver = b.lease(id).unwrap();
        let payload = vec![0x45; 256 * 1024]; let expected = payload.clone();
        let task = tokio::spawn(async move { sender.write_all(&payload).await.unwrap(); sender.shutdown().await.unwrap(); });
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), receiver.read_to_end(&mut actual)).await.unwrap().unwrap();
        task.await.unwrap(); assert_eq!(actual, expected); assert!(b_guard.failure().borrow().is_none());
    }
}
