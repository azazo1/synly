//! 剪贴板只保留最新未确认载荷, 路径重试保留身份, 仅应用成功推进接收去重状态.
use crate::protocol::{ClipboardPayload, ClipboardTransfer};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardStamp { pub id: Uuid, pub sequence: u64 }
impl ClipboardStamp {
    pub fn validate(self) -> Result<()> { if self.id.is_nil() || self.sequence == 0 { bail!("剪贴板逻辑传输身份为空"); } Ok(()) }
}
struct Pending { stamp: ClipboardStamp, payload: Arc<ClipboardPayload> }
#[derive(Default)]
pub struct ClipboardOutbox { sequence: u64, latest: Option<Pending> }
impl ClipboardOutbox {
    /// 替代尚未确认的旧载荷. 网络写入完成不移除 latest.
    pub fn replace(&mut self, payload: ClipboardPayload) -> Result<ClipboardStamp> {
        self.replace_shared(Arc::new(payload))
    }
    pub fn replace_shared(&mut self, payload: Arc<ClipboardPayload>) -> Result<ClipboardStamp> {
        let sequence = self.sequence.checked_add(1).ok_or_else(|| anyhow::anyhow!("剪贴板消息序号已耗尽"))?;
        let stamp = ClipboardStamp { id: Uuid::new_v4(), sequence }; self.sequence = sequence;
        self.latest = Some(Pending { stamp, payload }); Ok(stamp)
    }
    pub fn stamp(&self) -> Option<ClipboardStamp> { self.latest.as_ref().map(|pending| pending.stamp) }
    pub fn payload(&self) -> Option<Arc<ClipboardPayload>> { self.latest.as_ref().map(|pending| Arc::clone(&pending.payload)) }
    /// 新路径的包代次变化, 同一未确认载荷的 ID/消息序号不变化.
    pub fn on_route(&self, route_epoch: u64) -> Result<Option<ClipboardTransfer>> {
        if route_epoch == 0 { bail!("剪贴板路径代次为空"); }
        Ok(self.latest.as_ref().map(|pending| ClipboardTransfer { stamp: pending.stamp, route_epoch, payload: Arc::clone(&pending.payload) }))
    }
    pub fn applied(&mut self, stamp: ClipboardStamp) -> bool {
        if self.stamp() != Some(stamp) { return false; }
        self.latest = None; true
    }
    pub fn clear(&mut self) { self.latest = None; }
}

#[derive(Clone, Copy)]
struct Received { stamp: ClipboardStamp, digest: [u8; 32] }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveAction { Apply, Duplicate, InFlight, Stale, Busy }
#[derive(Default)]
pub struct ClipboardInbox { applied: Option<Received>, pending: Option<Received> }
impl ClipboardInbox {
    /// 完整载荷和路径代次已经验证后才能调用. Busy 不会取消正在应用的载荷.
    pub fn begin(&mut self, transfer: &ClipboardTransfer) -> Result<ReceiveAction> {
        transfer.validate()?;
        let current = Received { stamp: transfer.stamp, digest: payload_digest(&transfer.payload) };
        if let Some(applied) = self.applied {
            if current.stamp.sequence < applied.stamp.sequence { return Ok(ReceiveAction::Stale); }
            if current.stamp.sequence == applied.stamp.sequence {
                if !same(applied, current) { bail!("同一剪贴板消息序号收到冲突身份或载荷"); }
                return Ok(ReceiveAction::Duplicate);
            }
            if current.stamp.id == applied.stamp.id { bail!("剪贴板传输 ID 不能用于新消息序号"); }
        }
        if let Some(pending) = self.pending {
            if current.stamp.sequence < pending.stamp.sequence { return Ok(ReceiveAction::Stale); }
            if current.stamp.sequence == pending.stamp.sequence {
                if !same(pending, current) { bail!("正在应用的剪贴板消息收到冲突身份或载荷"); }
                return Ok(ReceiveAction::InFlight);
            }
            if current.stamp.id == pending.stamp.id { bail!("剪贴板传输 ID 被复用"); }
            return Ok(ReceiveAction::Busy);
        }
        self.pending = Some(current); Ok(ReceiveAction::Apply)
    }
    /// 只有真实应用成功才记为已确认. 失败保留重试机会, 迟到回执不能推进新事务.
    pub fn finish(&mut self, stamp: ClipboardStamp, success: bool) -> bool {
        let Some(pending) = self.pending.filter(|pending| pending.stamp == stamp) else { return false; };
        self.pending = None;
        if !success { return false; }
        self.applied = Some(pending); true
    }
    pub fn applied(&self) -> Option<ClipboardStamp> { self.applied.map(|applied| applied.stamp) }
}
fn same(a: Received, b: Received) -> bool { a.stamp == b.stamp && a.digest == b.digest }
// 流式摘要不再次序列化/拷贝图片和文件大体积数据, 明确区分缺失, 空值和字段边界.
fn payload_digest(payload: &ClipboardPayload) -> [u8; 32] {
    fn bytes(hash: &mut Sha256, data: &[u8]) { hash.update((data.len() as u64).to_be_bytes()); hash.update(data); }
    fn optional(hash: &mut Sha256, value: Option<&[u8]>) { match value { Some(data) => { hash.update([1]); bytes(hash, data); }, None => hash.update([0]) } }
    let mut hash = Sha256::new(); hash.update(b"synly/clipboard/payload/v1");
    optional(&mut hash, payload.text.as_deref().map(str::as_bytes)); optional(&mut hash, payload.rich_text.as_deref().map(str::as_bytes)); optional(&mut hash, payload.html.as_deref().map(str::as_bytes));
    optional(&mut hash, payload.image.as_ref().map(|image| image.png_bytes.as_slice()));
    hash.update((payload.files.len() as u64).to_be_bytes());
    for file in &payload.files { bytes(&mut hash, file.name.as_bytes()); bytes(&mut hash, &file.bytes); }
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload(text: &str) -> ClipboardPayload { ClipboardPayload { text: Some(text.to_owned()), rich_text: None, html: None, image: None, files: Vec::new() } }
    #[test]
    fn lost_ack_retries_same_message_on_new_route_without_reapplying() {
        let mut sender = ClipboardOutbox::default(); let mut receiver = ClipboardInbox::default();
        let stamp = sender.replace(payload("latest")).unwrap(); let first = sender.on_route(1).unwrap().unwrap();
        assert_eq!(receiver.begin(&first).unwrap(), ReceiveAction::Apply);
        assert_eq!(receiver.begin(&first).unwrap(), ReceiveAction::InFlight);
        assert!(receiver.finish(stamp, true));
        // 应用后的 ACK 丢失时, 换路径只再次确认, 不再次写系统剪贴板.
        let retry = sender.on_route(2).unwrap().unwrap(); assert_eq!(first.stamp, retry.stamp); assert_ne!(first.route_epoch, retry.route_epoch);
        assert_eq!(receiver.begin(&retry).unwrap(), ReceiveAction::Duplicate);
        assert!(sender.applied(stamp)); assert!(sender.on_route(3).unwrap().is_none());
    }
    #[test]
    fn failure_does_not_ack_and_late_old_ack_cannot_erase_newer_payload() {
        let mut sender = ClipboardOutbox::default(); let mut receiver = ClipboardInbox::default();
        let old = sender.replace(payload("old")).unwrap(); let transfer = sender.on_route(1).unwrap().unwrap();
        assert_eq!(receiver.begin(&transfer).unwrap(), ReceiveAction::Apply); assert!(!receiver.finish(old, false));
        assert!(receiver.applied().is_none()); assert_eq!(receiver.begin(&transfer).unwrap(), ReceiveAction::Apply);
        let new = sender.replace(payload("new")).unwrap(); assert!(!sender.applied(old)); assert_eq!(sender.stamp(), Some(new));
        assert!(!receiver.finish(new, true)); assert!(receiver.applied().is_none());
        assert!(receiver.finish(old, true)); let latest = sender.on_route(2).unwrap().unwrap();
        assert_eq!(receiver.begin(&latest).unwrap(), ReceiveAction::Apply); assert!(receiver.finish(new, true));
        assert_eq!(receiver.begin(&transfer).unwrap(), ReceiveAction::Stale);
        assert!(sender.applied(new));
    }
    #[test]
    fn conflicting_payload_identity_and_reused_ids_are_rejected() {
        let mut sender = ClipboardOutbox::default(); let mut receiver = ClipboardInbox::default();
        let stamp = sender.replace(payload("one")).unwrap(); let mut transfer = sender.on_route(1).unwrap().unwrap();
        assert_eq!(receiver.begin(&transfer).unwrap(), ReceiveAction::Apply); assert!(receiver.finish(stamp, true));
        transfer.payload = Arc::new(payload("changed")); assert!(receiver.begin(&transfer).is_err());
        transfer.payload = Arc::new(payload("one")); transfer.stamp.id = Uuid::new_v4(); assert!(receiver.begin(&transfer).is_err());
        transfer.stamp = ClipboardStamp { sequence: stamp.sequence + 1, ..stamp }; assert!(receiver.begin(&transfer).is_err());
        transfer.stamp.id = Uuid::nil(); assert!(receiver.begin(&transfer).is_err());
    }
    #[test]
    fn busy_apply_keeps_prior_transaction_and_bounded_latest_payload() {
        let mut sender = ClipboardOutbox::default(); let mut receiver = ClipboardInbox::default();
        let old = sender.replace(payload("old")).unwrap(); let old_transfer = sender.on_route(1).unwrap().unwrap(); receiver.begin(&old_transfer).unwrap();
        let newest = sender.replace(payload("new")).unwrap(); let new_transfer = sender.on_route(1).unwrap().unwrap();
        assert_eq!(receiver.begin(&new_transfer).unwrap(), ReceiveAction::Busy); assert_eq!(sender.stamp(), Some(newest));
        assert!(receiver.finish(old, true)); assert_eq!(receiver.begin(&new_transfer).unwrap(), ReceiveAction::Apply);
    }
    #[test]
    fn digest_distinguishes_field_boundaries_missing_fields_and_file_names() {
        let mut a = payload("ab"); a.rich_text = Some("c".to_owned()); let mut b = payload("a"); b.rich_text = Some("bc".to_owned()); assert_ne!(payload_digest(&a), payload_digest(&b));
        b = a.clone(); b.html = Some(String::new()); assert_ne!(payload_digest(&a), payload_digest(&b));
        a.files.push(crate::protocol::ClipboardFile { name: "one".to_owned(), bytes: vec![1, 2, 3] }); b = a.clone(); b.files[0].name = "two".to_owned(); assert_ne!(payload_digest(&a), payload_digest(&b));
    }
}
