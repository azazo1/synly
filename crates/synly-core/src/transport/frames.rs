//! 控制与剪贴板帧使用独立有界发送队列, 大载荷不阻塞控制写入器.

use crate::protocol::{Frame, FrameReader, FrameWriter, TransferLimits, frame_size_limit_message};
use super::stream::ByteStream;
use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

struct QueuedFrame { frame: Frame, receipt: Option<oneshot::Sender<Result<()>>> }
#[derive(Clone)]
pub struct FrameSender { control: mpsc::Sender<QueuedFrame>, clipboard: Option<mpsc::Sender<QueuedFrame>>, reliable: Option<super::clipboard_sender::ClipboardSender> }
impl FrameSender {
    async fn enqueue(&self, frame: Frame, receipt: Option<oneshot::Sender<Result<()>>>) -> Result<()> {
        let sender = if matches!(frame, Frame::Clipboard(_) | Frame::ClipboardTransfer(_)) { self.clipboard.as_ref().unwrap_or(&self.control) } else { &self.control };
        sender.send(QueuedFrame { frame, receipt }).await.map_err(|_| anyhow::anyhow!("会话帧写入器已经关闭"))
    }
    pub async fn send(&self, frame: Frame) -> Result<()> {
        let frame = match (&self.reliable, frame) { (Some(sender), Frame::Clipboard(payload)) => return sender.publish(payload), (_, frame) => frame };
        self.enqueue(frame, None).await
    }
    pub fn reliable_clipboard(mut self) -> (Self, super::clipboard_sender::ClipboardSenderGuard) {
        self.reliable = None;
        let (sender, guard) = super::clipboard_sender::start(self.clone()); self.reliable = Some(sender); (self, guard)
    }
    pub fn routed_clipboard(mut self) -> (Self, super::clipboard_sender::ClipboardSenderGuard) {
        let (sender, guard) = super::clipboard_sender::start_paused(); self.reliable = Some(sender); (self, guard)
    }
    pub async fn switch_clipboard_route(&self, sender: Option<FrameSender>, epoch: u64) -> Result<()> {
        self.reliable.as_ref().ok_or_else(|| anyhow::anyhow!("未初始化可靠剪贴板发送器"))?.route(sender, epoch).await
    }
    pub fn clipboard_receipt(&self, stamp: super::clipboard::ClipboardStamp, success: bool) { if let Some(sender) = &self.reliable { sender.receipt(stamp, success); } }
    pub fn set_clipboard_enabled(&self, enabled: bool) { if let Some(sender) = &self.reliable { sender.set_enabled(enabled); } }
    /// 等待本 lane 的 write_frame/flush 完成, 不把入队当作发送完成.
    /// 不代表对端应用已经接收或应用, 跨功能路径的提交仍必须等待应用级确认.
    pub async fn send_and_flush(&self, frame: Frame) -> Result<()> {
        let (tx, rx) = oneshot::channel(); self.enqueue(frame, Some(tx)).await?;
        rx.await.map_err(|_| anyhow::anyhow!("业务帧写入在完成确认前已取消"))?
    }
}
pub struct FrameInbox { control: mpsc::Receiver<Result<Frame>>, clipboard: Option<mpsc::Receiver<Result<Frame>>> }
impl FrameInbox {
    pub async fn recv(&mut self) -> Option<Result<Frame>> {
        let Some(clipboard) = &mut self.clipboard else { return self.control.recv().await };
        tokio::select! { biased; frame = self.control.recv() => frame, frame = clipboard.recv() => frame }
    }
}
/// 持有所有任务, 错误和取消时不会留下独立读写任务.
pub struct FrameIoGuard { readers: Vec<JoinHandle<()>>, writers: Vec<JoinHandle<Result<()>>> }
impl Drop for FrameIoGuard {
    fn drop(&mut self) { for task in &self.readers { task.abort(); } for task in &self.writers { task.abort(); } }
}
struct WriterWaitGuard(Vec<tokio::task::AbortHandle>);
impl Drop for WriterWaitGuard { fn drop(&mut self) { for task in &self.0 { task.abort(); } } }
impl FrameIoGuard {
    /// 优雅退出只有限等待控制消息冲刷, 不等待蓝牙大文件无限排空.
    pub async fn finish(mut self) -> Result<()> {
        for task in &self.readers { task.abort(); }
        let writers = std::mem::take(&mut self.writers);
        let _writer_wait = WriterWaitGuard(writers.iter().map(JoinHandle::abort_handle).collect());
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), async move {
            for writer in writers { writer.await??; }
            Result::<()>::Ok(())
        }).await;
        match result { Ok(result) => result, Err(_) => Ok(()) }
    }
}

pub fn open(control: ByteStream, clipboard: Option<ByteStream>, limits: TransferLimits) -> (FrameSender, FrameInbox, FrameIoGuard) { open_inner(control, clipboard, limits, false) }
pub fn open_control(control: ByteStream, limits: TransferLimits) -> (FrameSender, FrameInbox, FrameIoGuard) { open_inner(control, None, limits, true) }
pub fn open_clipboard(stream: ByteStream, limits: TransferLimits) -> (FrameSender, FrameInbox, FrameIoGuard) {
    let (reader, writer) = tokio::io::split(stream); let (tx, outgoing) = mpsc::channel(2); let (incoming, rx) = mpsc::channel(2);
    let read_task = spawn_reader(reader, incoming.clone(), limits, Some(true)); let write_task = spawn_writer(writer, outgoing, incoming, limits);
    (FrameSender { control: tx.clone(), clipboard: Some(tx), reliable: None }, FrameInbox { control: rx, clipboard: None }, FrameIoGuard { readers: vec![read_task], writers: vec![write_task] })
}
fn open_inner(control: ByteStream, clipboard: Option<ByteStream>, limits: TransferLimits, control_only: bool) -> (FrameSender, FrameInbox, FrameIoGuard) {
    let mut readers = Vec::new();
    let mut writers = Vec::new();
    let split = clipboard.is_some();
    let mut control_limits = limits;
    if split || control_only { control_limits.max_meta_len = control_limits.max_meta_len.min(16 * 1024); control_limits.max_frame_data_len = 0; }
    let (reader, writer) = tokio::io::split(control);
    let (control_tx, outgoing) = mpsc::channel(64);
    let (incoming, control_rx) = mpsc::channel(8);
    readers.push(spawn_reader(reader, incoming.clone(), control_limits, if split || control_only { Some(false) } else { None }));
    writers.push(spawn_writer(writer, outgoing, incoming, control_limits));
    let (clipboard_tx, clipboard_rx) = if let Some(clipboard) = clipboard {
        let (reader, writer) = tokio::io::split(clipboard);
        let (tx, outgoing) = mpsc::channel(2);
        let (incoming, rx) = mpsc::channel(2);
        readers.push(spawn_reader(reader, incoming.clone(), limits, Some(true)));
        writers.push(spawn_writer(writer, outgoing, incoming, limits));
        (Some(tx), Some(rx))
    } else { (None, None) };
    (FrameSender { control: control_tx, clipboard: clipboard_tx, reliable: None }, FrameInbox { control: control_rx, clipboard: clipboard_rx }, FrameIoGuard { readers, writers })
}
fn spawn_reader(reader: impl AsyncRead + Unpin + Send + 'static, incoming: mpsc::Sender<Result<Frame>>, limits: TransferLimits, clipboard: Option<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = FrameReader::with_limits(reader, limits);
        loop {
            let result = match clipboard { Some(clipboard) => reader.read_frame_on_lane(clipboard).await, None => reader.read_frame().await };
            let failed = result.is_err();
            if incoming.send(result).await.is_err() || failed { return; }
        }
    })
}
fn spawn_writer(writer: impl AsyncWrite + Unpin + Send + 'static, mut outgoing: mpsc::Receiver<QueuedFrame>, incoming: mpsc::Sender<Result<Frame>>, limits: TransferLimits) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut writer = FrameWriter::with_limits(writer, limits);
        while let Some(QueuedFrame { frame, receipt }) = outgoing.recv().await {
            if let Err(error) = writer.write_frame(frame).await {
                // 必须先完成写入回执, 不在有界错误 inbox 上阻塞等待回执的调用方.
                if let Some(receipt) = receipt { let _ = receipt.send(Err(anyhow::anyhow!("业务帧写入失败: {error:#}"))); }
                if let Some(message) = frame_size_limit_message(&error) { tracing::warn!(%message, "跳过超过限制的业务帧"); continue; }
                let _ = incoming.send(Err(anyhow::anyhow!("业务帧写入失败: {error:#}"))).await;
                return Err(error);
            }
            if let Some(receipt) = receipt { let _ = receipt.send(Ok(())); }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClipboardFile, ClipboardPayload, ControlMessage};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn clipboard(size: usize) -> ClipboardPayload {
        ClipboardPayload { text: Some("正文".to_owned()), rich_text: None, html: None, image: None, files: vec![ClipboardFile { name: "payload.bin".to_owned(), bytes: vec![0x52; size] }] }
    }
    #[tokio::test]
    async fn write_confirmation_waits_for_backpressure_and_reports_cancellation() {
        for cancelled in [false, true] {
            let (stream, peer) = tokio::io::duplex(1);
            let limits = TransferLimits::default();
            let (sender, _inbox, guard) = open(ByteStream::new(stream), None, limits);
            let mut confirmation = Box::pin(sender.send_and_flush(Frame::Control(ControlMessage::Goodbye)));
            assert!(tokio::time::timeout(Duration::from_millis(20), confirmation.as_mut()).await.is_err());
            if cancelled {
                drop(guard);
                assert!(tokio::time::timeout(Duration::from_secs(1), confirmation).await.unwrap().is_err());
            } else {
                let mut reader = FrameReader::with_limits(peer, limits);
                let (receipt, frame) = tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(confirmation.as_mut(), reader.read_frame()) }).await.unwrap();
                receipt.unwrap(); assert!(matches!(frame.unwrap(), Frame::Control(ControlMessage::Goodbye)));
            }
        }
    }
    #[tokio::test]
    async fn rejected_oversized_payload_completes_receipt_and_keeps_control_writer_alive() {
        let (stream, peer) = tokio::io::duplex(1024);
        let limits = TransferLimits { max_clipboard_binary_len: 8, ..TransferLimits::default() };
        let (sender, _inbox, _guard) = open(ByteStream::new(stream), None, limits);
        assert!(tokio::time::timeout(Duration::from_secs(1), sender.send_and_flush(Frame::Clipboard(clipboard(32)))).await.unwrap().is_err());
        sender.send_and_flush(Frame::Control(ControlMessage::Goodbye)).await.unwrap();
        let mut reader = FrameReader::with_limits(peer, limits);
        assert!(matches!(reader.read_frame().await.unwrap(), Frame::Control(ControlMessage::Goodbye)));
    }
    #[tokio::test]
    async fn stalled_bulk_writer_does_not_block_control_or_input() {
        let (a, b) = tokio::io::duplex(2048);
        let (a, mut a_channels) = super::super::bluetooth::open(ByteStream::new(a));
        let (b, mut b_channels) = super::super::bluetooth::open(ByteStream::new(b));
        let limits = TransferLimits::default();
        let (sender, _inbox, _io) = open(a, a_channels.clipboard.take(), limits);
        let (sender, _delivery) = sender.reliable_clipboard();
        sender.send(Frame::Clipboard(clipboard(512 * 1024))).await.unwrap();
        sender.send(Frame::Control(ControlMessage::CapabilitiesAck { generation: 17 })).await.unwrap();
        let id = uuid::Uuid::new_v4();
        let mut a_input = a_channels.input.lease(id).unwrap();
        let mut b_input = b_channels.input.lease(id).unwrap();
        a_input.write_all(&[1, 2, 3]).await.unwrap();
        let mut reader = FrameReader::with_limits(b, limits);
        let (frame, input) = tokio::time::timeout(Duration::from_secs(1), async {
            let frame = reader.read_frame_on_lane(false).await.unwrap();
            let mut data = [0; 3]; b_input.read_exact(&mut data).await.unwrap();
            (frame, data)
        }).await.unwrap();
        assert!(matches!(frame, Frame::Control(ControlMessage::CapabilitiesAck { generation: 17 })));
        assert_eq!(input, [1, 2, 3]);
        // 原始剪贴板 lane 之前一直没有读取, 优先消息已到达后再恢复完整文件读取.
        let mut bulk = FrameReader::with_limits(b_channels.clipboard.take().unwrap(), limits);
        let payload = tokio::time::timeout(Duration::from_secs(3), bulk.read_frame_on_lane(true)).await.unwrap().unwrap();
        let Frame::ClipboardTransfer(transfer) = payload else { panic!("应保留可靠传输身份") }; assert_eq!(*transfer.payload, clipboard(512 * 1024));
        sender.clipboard_receipt(transfer.stamp, true);
    }
    #[tokio::test]
    async fn wrong_lane_is_rejected_before_waiting_for_payload_body() {
        let (stream, mut peer) = tokio::io::duplex(1024);
        let limits = TransferLimits::default();
        let mut reader = FrameReader::with_limits(stream, limits);
        // 从有效剪贴板帧提取类型字节, 不提供其长度或帧体.
        let (wire, captured) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move { FrameWriter::with_limits(wire, limits).write_frame(Frame::Clipboard(clipboard(1024))).await });
        let mut captured = captured; let kind = captured.read_u8().await.unwrap();
        task.abort(); let _ = task.await;
        peer.write_all(&[kind]).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), reader.read_frame_on_lane(false)).await.unwrap().is_err());
    }
    #[tokio::test]
    async fn cancelling_graceful_finish_still_aborts_owned_writers() {
        let (stream, mut peer) = tokio::io::duplex(256);
        let (sender, _inbox, guard) = open(ByteStream::new(stream), None, TransferLimits::default());
        sender.send(Frame::Clipboard(clipboard(512 * 1024))).await.unwrap();
        let mut finish = Box::pin(guard.finish());
        std::future::poll_fn(|cx| {
            assert!(finish.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(finish); drop(sender);
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes)).await.unwrap().unwrap();
        assert!(bytes.len() <= 256);
    }
    #[tokio::test]
    async fn dropping_io_guard_closes_readers_and_blocked_writers() {
        let (stream, mut peer) = tokio::io::duplex(256);
        let (sender, _inbox, guard) = open(ByteStream::new(stream), None, TransferLimits::default());
        sender.send(Frame::Clipboard(clipboard(512 * 1024))).await.unwrap();
        drop(guard); drop(sender);
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes)).await.unwrap().unwrap();
        assert!(bytes.len() <= 256);
    }
}
