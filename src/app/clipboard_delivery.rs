//! 应用回执独立于主循环轮询, 慢 OS/host 中枢不阻塞控制和输入协商.
use crate::protocol::{ClipboardTransfer, ControlMessage};
use crate::clipboard::ClipboardSync;
use crate::host::clipboard_hub::{ClipboardHubHandle, ClipboardApplyOutcome};
use anyhow::Result;
use std::{future::Future, pin::Pin};
use synly_core::transport::clipboard::{ClipboardInbox, ClipboardStamp, ReceiveAction};
use uuid::Uuid;
type ApplyFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
#[derive(Default)]
pub(super) struct Delivery { inbox: ClipboardInbox, pending: Option<(ClipboardStamp, ApplyFuture)> }
impl Delivery {
    pub(super) fn begin(&mut self, transfer: ClipboardTransfer, can_receive: bool, sync: &ClipboardSync, hub: Option<&ClipboardHubHandle>, peer: Uuid) -> Result<Option<ControlMessage>> {
        transfer.validate()?;
        // 调用方的 ClipboardRoute 已检查实际提交路径与代次.
        let stamp = transfer.stamp;
        if !can_receive { return Ok(Some(ControlMessage::ClipboardRejected { stamp })); }
        match self.inbox.begin(&transfer)? {
            ReceiveAction::Duplicate => return Ok(Some(ControlMessage::ClipboardApplied { stamp })),
            ReceiveAction::InFlight => return Ok(None),
            ReceiveAction::Busy | ReceiveAction::Stale => return Ok(Some(ControlMessage::ClipboardRejected { stamp })),
            ReceiveAction::Apply => {},
        }
        let payload = std::sync::Arc::try_unwrap(transfer.payload).unwrap_or_else(|payload| (*payload).clone());
        let apply: ApplyFuture = if let Some(hub) = hub {
            let receipt = hub.ingest_tracked(peer, payload);
            Box::pin(async move { matches!(receipt.await, Ok(ClipboardApplyOutcome::Applied)) })
        } else {
            let sync = sync.clone();
            Box::pin(async move { match sync.apply_remote_payload_strict(payload).await { Ok(()) => true, Err(error) => { tracing::warn!(error = %error, "可靠剪贴板载荷未应用成功, 不发送成功确认"); false } } })
        };
        self.pending = Some((stamp, apply)); Ok(None)
    }
    pub(super) async fn receipt(&mut self) -> ControlMessage {
        let Some((stamp, application)) = &mut self.pending else { return std::future::pending().await; };
        let success = application.await; let stamp = *stamp; self.pending = None;
        if self.inbox.finish(stamp, success) { ControlMessage::ClipboardApplied { stamp } } else { ControlMessage::ClipboardRejected { stamp } }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::protocol::ClipboardPayload;
    #[tokio::test]
    async fn acknowledgement_waits_for_actual_application_and_failure_remains_retryable() {
        for success in [false, true] {
            let stamp = ClipboardStamp { id: Uuid::new_v4(), sequence: 1 };
            let transfer = ClipboardTransfer { stamp, route_epoch: 1, payload: Arc::new(ClipboardPayload { text: Some("正文".to_owned()), rich_text: None, html: None, image: None, files: vec![] }) };
            let mut delivery = Delivery::default(); assert_eq!(delivery.inbox.begin(&transfer).unwrap(), ReceiveAction::Apply);
            let (tx, rx) = tokio::sync::oneshot::channel(); delivery.pending = Some((stamp, Box::pin(async move { rx.await.unwrap_or(false) })));
            let mut receipt = Box::pin(delivery.receipt());
            assert!(tokio::time::timeout(std::time::Duration::from_millis(10), receipt.as_mut()).await.is_err());
            tx.send(success).unwrap(); let message = receipt.await;
            assert_eq!(matches!(message, ControlMessage::ClipboardApplied { stamp: got } if got == stamp), success);
            assert_eq!(delivery.inbox.begin(&transfer).unwrap(), if success { ReceiveAction::Duplicate } else { ReceiveAction::Apply });
        }
    }
}
