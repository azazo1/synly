//! 最新载荷有界保留, 网络完成与对端应用确认分开处理.
use super::{clipboard::{ClipboardOutbox, ClipboardStamp}, frames::FrameSender};
use crate::protocol::{ClipboardPayload, Frame};
use anyhow::{Result, anyhow};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{sync::{mpsc, watch, oneshot}, task::JoinHandle, time::Instant};
const ACK_WAIT: Duration = Duration::from_secs(15);
const MAX_ATTEMPTS: u8 = 4;
#[derive(Clone)]
pub struct ClipboardSender {
    latest: watch::Sender<Option<Arc<ClipboardPayload>>>,
    enabled: watch::Sender<(bool, u64)>,
    receipts: mpsc::Sender<(ClipboardStamp, bool)>,
    routes: mpsc::Sender<RouteUpdate>,
}
pub struct ClipboardSenderGuard(JoinHandle<()>);
impl Drop for ClipboardSenderGuard { fn drop(&mut self) { self.0.abort(); } }
impl ClipboardSender {
    pub fn publish(&self, payload: ClipboardPayload) -> Result<()> {
        if payload.is_empty() || !self.enabled.borrow().0 { return Ok(()); }
        self.latest.send(Some(Arc::new(payload))).map_err(|_| anyhow!("剪贴板交付任务已经关闭"))
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.send_if_modified(|current| { if current.0 == enabled { false } else { current.0 = enabled; current.1 = current.1.saturating_add(1); true } });
        if !enabled { self.latest.send_replace(None); }
    }
    /// 调用方先撤销旧 lease/IO, 本任务才可丢弃旧帧等待并在新流上重发原 ID.
    pub async fn route(&self, sender: Option<FrameSender>, epoch: u64) -> Result<()> {
        if sender.is_some() && epoch == 0 { return Err(anyhow!("剪贴板发送路径代次为空")); }
        let (done, receipt) = oneshot::channel();
        self.routes.send(RouteUpdate { sender, epoch, done }).await.map_err(|_| anyhow!("剪贴板交付任务已经关闭"))?;
        receipt.await.map_err(|_| anyhow!("剪贴板发送路径切换已取消"))
    }
    pub fn receipt(&self, stamp: ClipboardStamp, success: bool) {
        if let Err(error) = self.receipts.try_send((stamp, success)) { tracing::debug!(error = %error, "剪贴板应用回执邮箱已满或关闭, 等待下一次确认"); }
    }
}
struct RouteUpdate { sender: Option<FrameSender>, epoch: u64, done: oneshot::Sender<()> }
type WriteFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
struct Sending { stamp: ClipboardStamp, write: WriteFuture }
struct Worker {
    latest: watch::Receiver<Option<Arc<ClipboardPayload>>>, latest_slot: watch::Sender<Option<Arc<ClipboardPayload>>>, enabled: watch::Receiver<(bool, u64)>, receipts: mpsc::Receiver<(ClipboardStamp, bool)>,
    sender: Option<FrameSender>, route_epoch: u64, routes: mpsc::Receiver<RouteUpdate>, outbox: ClipboardOutbox, sending: Option<Sending>, deadline: Option<Instant>, attempts: u8, ack_wait: Duration,
}
pub fn start(sender: FrameSender) -> (ClipboardSender, ClipboardSenderGuard) { start_with_wait(sender, ACK_WAIT) }
pub fn start_paused() -> (ClipboardSender, ClipboardSenderGuard) { start_inner(None, ACK_WAIT) }
fn start_with_wait(sender: FrameSender, ack_wait: Duration) -> (ClipboardSender, ClipboardSenderGuard) { start_inner(Some(sender), ack_wait) }
fn start_inner(sender: Option<FrameSender>, ack_wait: Duration) -> (ClipboardSender, ClipboardSenderGuard) {
    let (latest_tx, latest) = watch::channel(None); let (enabled_tx, enabled) = watch::channel((true, 0)); let (receipts_tx, receipts) = mpsc::channel(32);
    let (routes_tx, routes) = mpsc::channel(4);
    let task = tokio::spawn(Worker { latest, latest_slot: latest_tx.clone(), enabled, receipts, sender, route_epoch: 1, routes, outbox: ClipboardOutbox::default(), sending: None, deadline: None, attempts: 0, ack_wait }.run());
    (ClipboardSender { latest: latest_tx, enabled: enabled_tx, receipts: receipts_tx, routes: routes_tx }, ClipboardSenderGuard(task))
}
impl Worker {
    fn start_write(&mut self) -> Result<()> {
        if self.sending.is_some() || !self.enabled.borrow().0 || self.outbox.stamp().is_none() { return Ok(()); }
        if self.attempts >= MAX_ATTEMPTS { self.deadline = None; return Ok(()); }
        let Some(sender) = self.sender.clone() else { self.deadline = None; return Ok(()); };
        let transfer = self.outbox.on_route(self.route_epoch)?.expect("已检查最新载荷"); let stamp = transfer.stamp;
        self.attempts += 1; self.deadline = None;
        self.sending = Some(Sending { stamp, write: Box::pin(async move { sender.send_and_flush(Frame::ClipboardTransfer(transfer)).await }) });
        Ok(())
    }
    async fn run(mut self) {
        loop {
            // 保持同一路径上的当前帧完整. 新载荷/禁用不取消正在写入的半帧.
            tokio::select! { biased;
                update = self.routes.recv() => {
                    let Some(update) = update else { return; };
                    self.sending = None; self.sender = update.sender; self.route_epoch = update.epoch;
                    self.deadline = None; self.attempts = 0;
                    // 路由命令优先于模式通知, 仍须先处理被合并的权限撤销, 防止复活旧载荷.
                    if self.enabled.has_changed().unwrap_or(true) { self.enabled.borrow_and_update(); self.outbox.clear(); }
                    let result = self.start_write(); let _ = update.done.send(());
                    if let Err(error) = result { tracing::warn!(error = %error, "剪贴板新发送路径初始化失败"); return; }
                }
                changed = self.enabled.changed() => {
                    if changed.is_err() { return; }
                    self.enabled.borrow_and_update();
                    // 即便 false -> true 被 watch 合并, 模式代次也必须清除旧事务.
                    self.outbox.clear(); self.deadline = None; self.attempts = 0;
                }
                receipt = self.receipts.recv() => {
                    let Some((stamp, success)) = receipt else { return; };
                    if self.outbox.stamp() == Some(stamp) {
                        if success { self.outbox.applied(stamp); self.deadline = None; }
                        else if self.sending.is_none() { self.schedule(stamp); }
                    }
                }
                changed = self.latest.changed() => {
                    if changed.is_err() { return; }
                    let payload = self.latest.borrow_and_update().clone();
                    if self.enabled.borrow().0 {
                        if let Some(payload) = payload {
                            // 清理已经取走的单槽引用, 不能清除并发发布的更晚载荷.
                            self.latest_slot.send_if_modified(|slot| { if slot.as_ref().is_some_and(|value| Arc::ptr_eq(value, &payload)) { *slot = None; true } else { false } });
                            if let Err(error) = self.outbox.replace_shared(payload) { tracing::error!(error = %error, "剪贴板消息身份生成失败"); return; }
                            self.attempts = 0; self.deadline = None;
                            if let Err(error) = self.start_write() { tracing::warn!(error = %error, "无法开始剪贴板交付"); return; }
                        }
                    } else { self.outbox.clear(); self.deadline = None; }
                }
                result = async { match &mut self.sending { Some(sending) => sending.write.as_mut().await, None => std::future::pending().await } } => {
                    let stamp = self.sending.take().expect("写入完成须存在当前帧").stamp;
                    if let Err(error) = result { tracing::warn!(error = %error, %stamp.id, "剪贴板帧未完成写入, 等待有限重试"); }
                    if self.outbox.stamp() == Some(stamp) { self.schedule(stamp); }
                    else if let Err(error) = self.start_write() { tracing::warn!(error = %error, "无法开始最新剪贴板交付"); return; }
                }
                _ = async { match self.deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {
                    if let Err(error) = self.start_write() { tracing::warn!(error = %error, "无法重试剪贴板交付"); return; }
                }
            }
        }
    }
    fn schedule(&mut self, stamp: ClipboardStamp) {
        if self.attempts >= MAX_ATTEMPTS { self.deadline = None; tracing::warn!(%stamp.id, "剪贴板应用未确认, 已暂停当前载荷的自动重试"); }
        else { self.deadline = Some(Instant::now() + self.ack_wait.saturating_mul(1 << self.attempts.saturating_sub(1))); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::{ClipboardFile, ClipboardTransfer, ControlMessage, FrameReader, TransferLimits}, transport::{frames, stream::ByteStream}};
    fn payload(text: &str, size: usize) -> ClipboardPayload { ClipboardPayload { text: Some(text.to_owned()), rich_text: None, html: None, image: None, files: if size == 0 { vec![] } else { vec![ClipboardFile { name: "data.bin".to_owned(), bytes: vec![0x55; size] }] } } }
    async fn transfer<R: tokio::io::AsyncRead + Unpin>(reader: &mut FrameReader<R>) -> ClipboardTransfer {
        let Frame::ClipboardTransfer(transfer) = tokio::time::timeout(Duration::from_secs(2), reader.read_frame()).await.unwrap().unwrap() else { panic!("应发送可靠剪贴板帧") }; transfer
    }
    #[tokio::test]
    async fn missing_ack_retries_same_identity_and_stops_after_bounded_attempts() {
        let (stream, peer) = tokio::io::duplex(4096); let (raw, _inbox, _io) = frames::open(ByteStream::new(stream), None, TransferLimits::default());
        let (sender, _guard) = start_with_wait(raw, Duration::from_millis(5)); let mut reader = FrameReader::new(peer);
        sender.publish(payload("正文", 0)).unwrap(); let first = transfer(&mut reader).await;
        for _ in 1..MAX_ATTEMPTS { let retry = transfer(&mut reader).await; assert_eq!(retry.stamp, first.stamp); assert_eq!(retry.payload, first.payload); }
        assert!(tokio::time::timeout(Duration::from_millis(60), reader.read_frame()).await.is_err());
        sender.publish(payload("新内容", 0)).unwrap(); let next = transfer(&mut reader).await; assert!(next.stamp.sequence > first.stamp.sequence);
        sender.receipt(first.stamp, true); let retry = transfer(&mut reader).await; assert_eq!(retry.stamp, next.stamp);
        sender.receipt(next.stamp, true); assert!(tokio::time::timeout(Duration::from_millis(50), reader.read_frame()).await.is_err());
    }
    #[tokio::test]
    async fn latest_coalesces_while_large_write_blocks_and_control_keeps_progressing() {
        let (control, control_peer) = tokio::io::duplex(1024); let (clipboard, clipboard_peer) = tokio::io::duplex(128);
        let (raw, _inbox, _io) = frames::open(ByteStream::new(control), Some(ByteStream::new(clipboard)), TransferLimits::default());
        let (sender, _guard) = start_with_wait(raw.clone(), Duration::from_secs(1));
        let mut reader = FrameReader::new(clipboard_peer); sender.publish(payload("old", 256 * 1024)).unwrap();
        let mut first = Box::pin(reader.read_frame()); assert!(tokio::time::timeout(Duration::from_millis(10), first.as_mut()).await.is_err());
        for index in 0..50 { sender.publish(payload(&format!("latest-{index}"), 0)).unwrap(); }
        raw.send(Frame::Control(ControlMessage::Goodbye)).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), FrameReader::new(control_peer).read_frame()).await.unwrap().unwrap(), Frame::Control(ControlMessage::Goodbye)));
        let Frame::ClipboardTransfer(old) = first.await.unwrap() else { panic!("旧帧应完整写出") }; assert_eq!(old.payload.text.as_deref(), Some("old"));
        let latest = transfer(&mut reader).await; assert_eq!(latest.payload.text.as_deref(), Some("latest-49")); assert!(latest.stamp.sequence > old.stamp.sequence);
        sender.receipt(old.stamp, true); sender.receipt(latest.stamp, true);
        assert!(tokio::time::timeout(Duration::from_millis(30), reader.read_frame()).await.is_err());
    }
    #[tokio::test]
    async fn routing_after_coalesced_disable_enable_never_resurrects_old_payload() {
        let (a, b) = tokio::io::duplex(1024); let (raw, _inbox, _io) = frames::open_clipboard(ByteStream::new(a), TransferLimits::default());
        let (sender, _worker) = start_with_wait(raw, Duration::from_secs(30)); let mut initial = FrameReader::new(b);
        sender.publish(payload("权限撤销前", 0)).unwrap(); let _first = transfer(&mut initial).await;
        sender.set_enabled(false); sender.set_enabled(true);
        let (a, b) = tokio::io::duplex(1024); let (next, _inbox, _io) = frames::open_clipboard(ByteStream::new(a), TransferLimits::default());
        sender.route(Some(next), 2).await.unwrap(); let mut reader = FrameReader::new(b);
        assert!(tokio::time::timeout(Duration::from_millis(20), reader.read_frame()).await.is_err());
        sender.publish(payload("新的本机更新", 0)).unwrap(); assert_eq!(transfer(&mut reader).await.payload.text.as_deref(), Some("新的本机更新"));
    }
    #[tokio::test]
    async fn interrupted_frame_is_discarded_with_old_stream_and_same_stamp_retries_on_new_path() {
        let (a, b) = tokio::io::duplex(1024); let (raw, _inbox, _io) = frames::open_clipboard(ByteStream::new(a), TransferLimits::default());
        let (sender, _worker) = start_with_wait(raw, Duration::from_secs(30)); let mut initial = FrameReader::new(b);
        sender.publish(payload("保持原身份", 256 * 1024)).unwrap(); let first = transfer(&mut initial).await;
        let (a, b) = tokio::io::duplex(128); let (blocked, _inbox, blocked_io) = frames::open_clipboard(ByteStream::new(a), TransferLimits::default());
        sender.route(Some(blocked), 2).await.unwrap(); let mut old = FrameReader::new(b);
        assert!(tokio::time::timeout(Duration::from_millis(10), old.read_frame()).await.is_err());
        // 取消读写只发生在被丢弃的旧流, 新流从完整头开始.
        drop(blocked_io); sender.route(None, 0).await.unwrap();
        let (a, b) = tokio::io::duplex(1024); let (next, _inbox, _next_io) = frames::open_clipboard(ByteStream::new(a), TransferLimits::default());
        sender.route(Some(next), 3).await.unwrap(); let retry = transfer(&mut FrameReader::new(b)).await;
        assert_eq!(retry.stamp, first.stamp); assert_eq!(retry.payload, first.payload); assert_eq!(retry.route_epoch, 3);
        sender.receipt(retry.stamp, true);
    }
    #[tokio::test]
    async fn disable_then_enable_does_not_replay_old_transaction_or_truncate_current_frame() {
        let (stream, peer) = tokio::io::duplex(128); let (raw, _inbox, _io) = frames::open(ByteStream::new(stream), None, TransferLimits::default());
        let (sender, _guard) = start_with_wait(raw, Duration::from_millis(5)); let mut reader = FrameReader::new(peer);
        sender.publish(payload("old", 128 * 1024)).unwrap();
        let mut first = Box::pin(reader.read_frame()); assert!(tokio::time::timeout(Duration::from_millis(10), first.as_mut()).await.is_err());
        sender.set_enabled(false); sender.set_enabled(true);
        assert!(matches!(first.await.unwrap(), Frame::ClipboardTransfer(_)));
        assert!(tokio::time::timeout(Duration::from_millis(30), reader.read_frame()).await.is_err());
        sender.publish(payload("new", 0)).unwrap(); let next = transfer(&mut reader).await; assert_eq!(next.payload.text.as_deref(), Some("new"));
        sender.receipt(next.stamp, true);
    }
}
