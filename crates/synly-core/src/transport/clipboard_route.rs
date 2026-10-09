//! 独立剪贴板路由协商, 子流撤销不关闭控制或输入, 重试保留逻辑载荷身份.
use super::{frames::{self, FrameSender, FrameInbox, FrameIoGuard}, generation::GenerationLane, logical::SecondaryTunnel, routing::*};
use crate::{capabilities::CapabilityState, protocol::{CapabilityEpoch, ClipboardTransfer, ControlMessage, Frame, TransferLimits}};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

pub struct RouteContext<'a> {
    pub capabilities: &'a CapabilityState, pub primary: TransportKind, pub primary_lane: &'a GenerationLane,
    pub secondary: Option<(TransportKind, GenerationLane)>, pub remote_links: AvailableLinks,
    pub policy: PathPolicy, pub remote_policy: PathPolicy, pub tx: &'a FrameSender,
}
impl RouteContext<'_> {
    pub fn secondary(tunnel: &SecondaryTunnel) -> Option<(TransportKind, GenerationLane)> { tunnel.channels.clipboard_route.clone().map(|lane| (tunnel.transport(), lane)) }
    fn shared(&self) -> AvailableLinks {
        let mut local = AvailableLinks { lan: self.primary == TransportKind::Lan, bluetooth: self.primary == TransportKind::Bluetooth };
        if let Some((transport, _)) = &self.secondary { match transport { TransportKind::Lan => local.lan = true, TransportKind::Bluetooth => local.bluetooth = true } }
        AvailableLinks { lan: local.lan && self.remote_links.lan, bluetooth: local.bluetooth && self.remote_links.bluetooth }
    }
    fn lane(&self, transport: TransportKind) -> Option<GenerationLane> {
        if transport == self.primary { Some(self.primary_lane.clone()) }
        else { self.secondary.as_ref().filter(|(kind, _)| *kind == transport).map(|(_, lane)| lane.clone()) }
    }
    fn enabled(&self) -> bool {
        let local = self.capabilities.effective_local().clipboard_mode; let remote = self.capabilities.effective_remote().clipboard_mode;
        local.can_send() && remote.can_receive() || local.can_receive() && remote.can_send()
    }
    fn signature(&self) -> Signature { Signature { epoch: self.capabilities.epoch(), links: self.shared(), policy: self.policy, remote_policy: self.remote_policy } }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Signature { epoch: CapabilityEpoch, links: AvailableLinks, policy: PathPolicy, remote_policy: PathPolicy }
struct Path { lane: GenerationLane, tx: FrameSender, inbox: FrameInbox, _guard: FrameIoGuard }
impl Drop for Path { fn drop(&mut self) { self.lane.revoke(); } }
pub struct ClipboardRoute {
    host: bool, route: ChannelRoute, generation: Option<Uuid>, epoch: Option<CapabilityEpoch>,
    target: Option<TransportKind>, path: Option<Path>, deadline: Option<Instant>, blocked: Option<Signature>, limits: TransferLimits,
}
impl ClipboardRoute {
    pub fn new(host: bool, limits: TransferLimits) -> Self {
        Self { host, route: ChannelRoute::new(FunctionalChannel::Clipboard, host, PathPolicy::Auto), generation: None, epoch: None, target: None, path: None, deadline: None, blocked: None, limits }
    }
    pub fn choice(&self, context: RouteContext<'_>) -> RouteChoice { choose_route(FunctionalChannel::Clipboard, context.policy, context.remote_policy, context.shared(), self.target) }
    pub fn transport(&self) -> Option<TransportKind> { self.route.sending_route().and_then(|route| route.transport) }
    pub fn route_epoch(&self) -> Option<u64> { self.route.receiving_route().map(|route| route.epoch) }
    pub fn switching(&self) -> bool { self.route.is_switching() }
    pub fn failed(&self) -> bool { self.blocked.is_some() }
    async fn stop(&mut self, tx: &FrameSender) -> Result<()> {
        // 撤销代次并丢弃旧 IO 后, 才能取消发送等待. 半帧不会进入后续 lease.
        self.path = None; self.route.cancel(); self.deadline = None; self.target = None; self.generation = None; self.epoch = None;
        tx.switch_clipboard_route(None, 0).await
    }
    fn prepare(&mut self, lane: GenerationLane, generation: Uuid) -> Result<()> {
        let stream = lane.lease(generation)?; let (tx, inbox, guard) = frames::open_clipboard(stream, self.limits);
        self.path = Some(Path { lane, tx, inbox, _guard: guard }); Ok(())
    }
    pub async fn reconcile(&mut self, context: RouteContext<'_>) -> Result<()> {
        let signature = context.signature();
        if self.blocked.is_some_and(|blocked| blocked != signature) { self.blocked = None; }
        let choice = choose_route(FunctionalChannel::Clipboard, context.policy, context.remote_policy, signature.links, self.target);
        let target = choice.transport();
        let invalid = self.route.update_policy(context.policy) || self.epoch.is_some_and(|epoch| epoch != signature.epoch) || self.target.is_some_and(|current| Some(current) != target);
        if invalid || !context.capabilities.is_local_acknowledged() || !context.enabled() { self.stop(context.tx).await?; }
        if !context.enabled() || !self.host || !context.capabilities.is_local_acknowledged() || self.route.is_switching() || self.transport() == target && self.path.is_some() || self.blocked == Some(signature) { return Ok(()); }
        let Some(target) = target else { return Ok(()); };
        let Some(lane) = context.lane(target) else { return Ok(()); };
        self.stop(context.tx).await?; self.blocked = None;
        let route_epoch = self.route.begin(Some(target))?; let generation = Uuid::new_v4();
        self.generation = Some(generation); self.epoch = Some(signature.epoch); self.target = Some(target); self.deadline = Some(Instant::now() + Duration::from_secs(10));
        if let Err(error) = self.prepare(lane, generation) { tracing::warn!(error = %error, "剪贴板路径准备失败"); return self.fail(context, true).await; }
        let message = self.route.quiesced(route_epoch)?;
        context.tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch: signature.epoch, generation, message })).await
    }
    pub async fn receive(&mut self, epoch: CapabilityEpoch, generation: Uuid, message: RouteMessage, context: RouteContext<'_>) -> Result<()> {
        if !context.capabilities.current_epoch(epoch) || !context.capabilities.is_local_acknowledged() { return Ok(()); }
        if generation.is_nil() { bail!("剪贴板路径 lease 为空"); }
        if let RouteMessage::Offer(offer) = message {
            if self.host || offer.channel != FunctionalChannel::Clipboard || offer.transport.is_none() { bail!("剪贴板路径方案角色或功能无效"); }
            if offer.epoch <= self.route.latest_epoch() { return Ok(()); }
            let transport = offer.transport.expect("已检查路径类型"); let shared = context.shared();
            let offered = AvailableLinks { lan: shared.lan && transport == TransportKind::Lan, bluetooth: shared.bluetooth && transport == TransportKind::Bluetooth };
            let allowed = context.enabled() && choose_route(FunctionalChannel::Clipboard, context.policy, context.remote_policy, offered, None).transport() == Some(transport);
            let lane = context.lane(transport).filter(|_| allowed);
            let Some(lane) = lane else { context.tx.send(Frame::Control(ControlMessage::ClipboardPathFailed { epoch, generation })).await?; return Ok(()); };
            self.stop(context.tx).await?; self.route.update_policy(context.policy); self.blocked = None;
            self.route.receive_message(message)?;
            self.epoch = Some(epoch); self.generation = Some(generation); self.target = Some(transport); self.deadline = Some(Instant::now() + Duration::from_secs(10));
            if let Err(error) = self.prepare(lane, generation) { tracing::warn!(error = %error, "剪贴板对端路径准备失败"); return self.fail(context, true).await; }
            let ready = self.route.quiesced(offer.epoch)?;
            context.tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: ready })).await?;
            return Ok(());
        }
        if !self.route.is_switching() || self.epoch != Some(epoch) || self.generation != Some(generation) || message.epoch() != self.route.latest_epoch() { return Ok(()); }
        let effect = self.route.receive_message(message)?;
        if let Some(outbound) = effect.outbound {
            context.tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: outbound })).await?;
            if matches!(outbound, RouteMessage::Committed { .. }) { self.route.reply_sent(outbound)?; }
        }
        if !self.route.is_switching() {
            self.deadline = None;
            let sender = self.path.as_ref().context("剪贴板路径提交缺少已准备 IO")?.tx.clone();
            context.tx.switch_clipboard_route(Some(sender), self.route.latest_epoch()).await?;
        }
        Ok(())
    }
    pub async fn incoming(&mut self) -> Result<ClipboardTransfer> {
        loop {
            let Some(path) = &mut self.path else { return std::future::pending().await; };
            let frame = path.inbox.recv().await.context("剪贴板路径已关闭")??;
            let Frame::ClipboardTransfer(transfer) = frame else { bail!("剪贴板路径收到缺少可靠身份的帧"); };
            // 接收端在 Commit 前已准备 IO. 发送方只能在最终确认之后发送.
            if self.route.receiving_route().is_some_and(|route| transfer.route_epoch == route.epoch) { return Ok(transfer); }
            tracing::debug!(route_epoch = transfer.route_epoch, "丢弃未提交或旧代次的剪贴板帧");
        }
    }
    pub fn deadline(&self) -> Option<Instant> { self.deadline }
    pub async fn fail(&mut self, context: RouteContext<'_>, notify: bool) -> Result<()> {
        let generation = self.generation; let epoch = self.epoch;
        self.blocked = Some(context.signature()); self.stop(context.tx).await?;
        if notify && let (Some(epoch), Some(generation)) = (epoch, generation) { context.tx.send(Frame::Control(ControlMessage::ClipboardPathFailed { epoch, generation })).await?; }
        tracing::warn!("剪贴板路径已暂停, 未确认载荷保留, 控制和输入继续运行"); Ok(())
    }
    pub async fn remote_failed(&mut self, epoch: CapabilityEpoch, generation: Uuid, context: RouteContext<'_>) -> Result<()> {
        if self.epoch == Some(epoch) && self.generation == Some(generation) { self.fail(context, false).await?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::{ClipboardPayload, RuntimeCapabilities}, settings::{AudioMode, ClipboardMode}, input::InputMode};
    use super::super::{bluetooth::{self, BluetoothChannels}, clipboard::{ClipboardInbox, ReceiveAction}, clipboard_sender::ClipboardSenderGuard, stream::ByteStream};
    struct Data { caps: CapabilityState, lane: GenerationLane, secondary: Option<(TransportKind, GenerationLane)>, links: AvailableLinks, policy: PathPolicy, remote_policy: PathPolicy, tx: FrameSender }
    impl Data { fn context(&self) -> RouteContext<'_> { RouteContext { capabilities: &self.caps, primary: TransportKind::Lan, primary_lane: &self.lane, secondary: self.secondary.clone(), remote_links: self.links, policy: self.policy, remote_policy: self.remote_policy, tx: &self.tx } } }
    struct Side { route: ClipboardRoute, data: Data, inbox: FrameInbox, _channels: (BluetoothChannels, BluetoothChannels), _io: FrameIoGuard, _sender: ClipboardSenderGuard }
    fn side(host: bool, main: ByteStream, secondary: ByteStream) -> Side {
        let (control, mut channels) = bluetooth::open(main); let lane = channels.enable_clipboard_routes().unwrap();
        let (_control, mut other_channels) = bluetooth::open(secondary); let other = other_channels.enable_clipboard_routes().unwrap();
        let (tx, inbox, io) = frames::open_control(control, TransferLimits::default()); let (tx, sender) = tx.routed_clipboard();
        let caps = RuntimeCapabilities { clipboard_mode: ClipboardMode::Both, audio_mode: AudioMode::Off, input_mode: InputMode::Off };
        Side { route: ClipboardRoute::new(host, TransferLimits::default()), data: Data { caps: CapabilityState::new(host, caps, caps), lane, secondary: Some((TransportKind::Bluetooth, other)), links: AvailableLinks { lan: true, bluetooth: true }, policy: PathPolicy::Auto, remote_policy: PathPolicy::Auto, tx }, inbox, _channels: (channels, other_channels), _io: io, _sender: sender }
    }
    fn pair() -> (Side, Side) {
        let (a, b) = tokio::io::duplex(2048); let (c, d) = tokio::io::duplex(2048);
        (side(true, ByteStream::new(a), ByteStream::new(c)), side(false, ByteStream::new(b), ByteStream::new(d)))
    }
    async fn relay(to: &mut Side) -> (CapabilityEpoch, Uuid, RouteMessage) {
        let frame = tokio::time::timeout(Duration::from_secs(2), to.inbox.recv()).await.unwrap().unwrap().unwrap();
        let Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message }) = frame else { panic!("应收到路径握手") };
        to.route.receive(epoch, generation, message, to.data.context()).await.unwrap(); (epoch, generation, message)
    }
    async fn commit(host: &mut Side, client: &mut Side) -> (CapabilityEpoch, Uuid, RouteMessage) {
        host.route.reconcile(host.data.context()).await.unwrap();
        let offer = relay(client).await;
        assert!(host.route.transport().is_none() && client.route.transport().is_none());
        relay(host).await;
        assert!(host.route.transport().is_none() && client.route.transport().is_none());
        relay(client).await;
        assert!(client.route.transport().is_some() && host.route.transport().is_none());
        relay(host).await;
        assert_eq!(host.route.transport(), client.route.transport());
        offer
    }
    fn payload(text: &str) -> ClipboardPayload { ClipboardPayload { text: Some(text.to_owned()), rich_text: None, html: None, image: None, files: vec![] } }
    async fn incoming(side: &mut Side) -> ClipboardTransfer { tokio::time::timeout(Duration::from_secs(2), side.route.incoming()).await.unwrap().unwrap() }
    #[tokio::test]
    async fn lost_ack_switches_both_directions_without_duplicate_application_and_conflict_pauses() {
        let (mut host, mut client) = pair(); let old_offer = commit(&mut host, &mut client).await;
        assert_eq!(host.route.transport(), Some(TransportKind::Lan));
        host.data.tx.send(Frame::Clipboard(payload("原载荷"))).await.unwrap();
        let first = incoming(&mut client).await; let mut applied = ClipboardInbox::default();
        assert_eq!(applied.begin(&first).unwrap(), ReceiveAction::Apply); assert!(applied.finish(first.stamp, true));
        host.data.policy = PathPolicy::PreferBluetooth; host.data.remote_policy = PathPolicy::PreferBluetooth;
        client.data.policy = PathPolicy::PreferBluetooth; client.data.remote_policy = PathPolicy::PreferBluetooth;
        commit(&mut host, &mut client).await;
        let retry = incoming(&mut client).await; assert_eq!(retry.stamp, first.stamp); assert_eq!(retry.payload, first.payload); assert!(retry.route_epoch > first.route_epoch);
        assert_eq!(applied.begin(&retry).unwrap(), ReceiveAction::Duplicate);
        host.data.tx.clipboard_receipt(retry.stamp, true);
        client.route.receive(old_offer.0, old_offer.1, old_offer.2, client.data.context()).await.unwrap();
        assert_eq!(client.route.transport(), Some(TransportKind::Bluetooth));
        // 副承载断开后重新准备 LAN lease, 没有确认的反向载荷保持原身份.
        client.data.tx.send(Frame::Clipboard(payload("反向载荷"))).await.unwrap(); let reverse = incoming(&mut host).await;
        host.data.secondary = None; host.data.links.bluetooth = false; client.data.secondary = None; client.data.links.bluetooth = false;
        client.route.reconcile(client.data.context()).await.unwrap(); commit(&mut host, &mut client).await;
        let reverse_retry = incoming(&mut host).await; assert_eq!(reverse_retry.stamp, reverse.stamp); assert_eq!(reverse_retry.payload, reverse.payload);
        client.data.tx.clipboard_receipt(reverse_retry.stamp, true);
        assert_eq!(host.route.transport(), Some(TransportKind::Lan));
        host.data.policy = PathPolicy::LanOnly; host.data.remote_policy = PathPolicy::BluetoothOnly;
        client.data.policy = PathPolicy::BluetoothOnly; client.data.remote_policy = PathPolicy::LanOnly;
        host.route.reconcile(host.data.context()).await.unwrap(); client.route.reconcile(client.data.context()).await.unwrap();
        assert!(host.route.transport().is_none() && client.route.transport().is_none());
        host.data.tx.send(Frame::Clipboard(payload("冲突期间的最新载荷"))).await.unwrap();
        host.data.policy = PathPolicy::Auto; host.data.remote_policy = PathPolicy::Auto; client.data.policy = PathPolicy::Auto; client.data.remote_policy = PathPolicy::Auto;
        commit(&mut host, &mut client).await;
        let latest = incoming(&mut client).await; assert!(latest.stamp.sequence > first.stamp.sequence); assert_eq!(*latest.payload, payload("冲突期间的最新载荷"));
    }
    #[tokio::test]
    async fn missing_commit_and_invalid_offer_preserve_other_channels_and_block_retry_storms() {
        let (mut host, mut client) = pair(); host.route.reconcile(host.data.context()).await.unwrap();
        let offer = relay(&mut client).await; relay(&mut host).await;
        // 丢弃 Commit, 两端仍不允许发送. 失败不会终止独立主控制通道.
        let _commit = client.inbox.recv().await.unwrap().unwrap();
        assert!(host.route.transport().is_none() && client.route.transport().is_none());
        host.route.fail(host.data.context(), false).await.unwrap(); client.route.fail(client.data.context(), false).await.unwrap();
        host.route.reconcile(host.data.context()).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), client.inbox.recv()).await.is_err());
        host.data.tx.send(Frame::Control(ControlMessage::CapabilitiesAck { generation: 42 })).await.unwrap();
        assert!(matches!(client.inbox.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::CapabilitiesAck { generation: 42 })));
        host.data.links.bluetooth = false; client.data.links.bluetooth = false;
        commit(&mut host, &mut client).await;
        client.data.policy = PathPolicy::LanOnly;
        let invalid = RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Clipboard, epoch: 100, transport: Some(TransportKind::Bluetooth) });
        client.route.receive(offer.0, Uuid::new_v4(), invalid, client.data.context()).await.unwrap();
        assert_eq!(client.route.transport(), Some(TransportKind::Lan));
        assert!(matches!(host.inbox.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardPathFailed { .. })));
    }
}
