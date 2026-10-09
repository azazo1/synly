//! 每个功能独立协商路径, 切换期间暂停发送并拒绝旧代次.

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind { Lan, Bluetooth }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum PathPolicy { #[default] Auto, PreferBluetooth, LanOnly, BluetoothOnly }

impl PathPolicy {
    fn allowed(self) -> u8 {
        match self { Self::LanOnly => 1, Self::BluetoothOnly => 2, Self::Auto | Self::PreferBluetooth => 3 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionalChannel { Input, Clipboard, Audio }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPolicies {
    pub input: PathPolicy,
    pub clipboard: PathPolicy,
    pub audio: PathPolicy,
}
impl Default for ChannelPolicies {
    fn default() -> Self { Self { input: PathPolicy::PreferBluetooth, clipboard: PathPolicy::Auto, audio: PathPolicy::LanOnly } }
}
impl ChannelPolicies {
    pub fn for_channel(self, channel: FunctionalChannel) -> PathPolicy {
        match channel { FunctionalChannel::Input => self.input, FunctionalChannel::Clipboard => self.clipboard, FunctionalChannel::Audio => self.audio }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableLinks { pub lan: bool, pub bluetooth: bool }
impl AvailableLinks {
    pub fn contains(self, transport: TransportKind) -> bool {
        match transport { TransportKind::Lan => self.lan, TransportKind::Bluetooth => self.bluetooth }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason { PolicyConflict, UnsupportedChannel, TransportUnavailable }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteChoice { Selected(TransportKind), Paused(PauseReason) }
impl RouteChoice {
    pub fn transport(self) -> Option<TransportKind> {
        match self { Self::Selected(kind) => Some(kind), Self::Paused(_) => None }
    }
}

pub fn choose_route(channel: FunctionalChannel, local: PathPolicy, remote: PathPolicy, available: AvailableLinks, current: Option<TransportKind>) -> RouteChoice {
    let allowed = local.allowed() & remote.allowed();
    if allowed == 0 { return RouteChoice::Paused(PauseReason::PolicyConflict); }
    let supported = if channel == FunctionalChannel::Audio { 1 } else { 3 };
    let allowed = allowed & supported;
    if allowed == 0 { return RouteChoice::Paused(PauseReason::UnsupportedChannel); }
    let usable = |kind| available.contains(kind) && allowed & match kind { TransportKind::Lan => 1, TransportKind::Bluetooth => 2 } != 0;
    let prefer_bluetooth = local == PathPolicy::PreferBluetooth || remote == PathPolicy::PreferBluetooth;
    if prefer_bluetooth && usable(TransportKind::Bluetooth) { return RouteChoice::Selected(TransportKind::Bluetooth); }
    if let Some(current) = current.filter(|kind| usable(*kind)) { return RouteChoice::Selected(current); }
    for kind in [TransportKind::Lan, TransportKind::Bluetooth] {
        if usable(kind) { return RouteChoice::Selected(kind); }
    }
    RouteChoice::Paused(PauseReason::TransportUnavailable)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteOffer {
    pub channel: FunctionalChannel,
    pub epoch: u64,
    pub transport: Option<TransportKind>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteMessage {
    Offer(RouteOffer),
    Ready { channel: FunctionalChannel, epoch: u64 },
    Commit { channel: FunctionalChannel, epoch: u64 },
    Committed { channel: FunctionalChannel, epoch: u64 },
}
impl RouteMessage {
    fn channel(self) -> FunctionalChannel {
        match self { Self::Offer(offer) => offer.channel, Self::Ready { channel, .. } | Self::Commit { channel, .. } | Self::Committed { channel, .. } => channel }
    }
    pub fn epoch(self) -> u64 {
        match self { Self::Offer(offer) => offer.epoch, Self::Ready { epoch, .. } | Self::Commit { epoch, .. } | Self::Committed { epoch, .. } => epoch }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase { Quiescing, AwaitReady, AwaitCommit, AwaitCommitted, CommittedReply }
#[derive(Clone, Copy, Debug)]
struct Pending { offer: RouteOffer, phase: Phase }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteEffect {
    /// 为 true 时, 输入接收者必须先释放按键/按钮, 发送者停止捕获, 才能调用 quiesced.
    pub quiesce: bool,
    pub outbound: Option<RouteMessage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteStamp { pub epoch: u64, pub transport: TransportKind, pub sequence: u64 }

/// 每个功能各持有一个状态机, 修改剪贴板路径不会暂停输入.
#[derive(Debug)]
pub struct ChannelRoute {
    channel: FunctionalChannel,
    host: bool,
    policy: PathPolicy,
    highest_epoch: u64,
    pending: Option<Pending>,
    receive: Option<RouteOffer>,
    send: Option<RouteOffer>,
    last_sequence: u64,
}

impl ChannelRoute {
    pub fn new(channel: FunctionalChannel, host: bool, policy: PathPolicy) -> Self {
        Self { channel, host, policy, highest_epoch: 0, pending: None, receive: None, send: None, last_sequence: 0 }
    }

    fn validate_transport(&self, transport: Option<TransportKind>) -> Result<()> {
        if let Some(kind) = transport {
            let bit = match kind { TransportKind::Lan => 1, TransportKind::Bluetooth => 2 };
            if self.policy.allowed() & bit == 0 { bail!("路径方案违反本机功能策略"); }
        }
        if self.channel == FunctionalChannel::Audio && transport == Some(TransportKind::Bluetooth) { bail!("音频不能通过蓝牙路径传输"); }
        Ok(())
    }

    fn pause(&mut self) {
        self.receive = None;
        self.send = None;
        self.last_sequence = 0;
    }

    /// host 开始切换, 此时尚不发送 offer. 调用方完成状态释放后调用 quiesced.
    pub fn begin(&mut self, transport: Option<TransportKind>) -> Result<u64> {
        if !self.host { bail!("只有 host 可以提出路径方案"); }
        if self.pending.is_some() { bail!("已有路径切换正在进行"); }
        self.validate_transport(transport)?;
        let epoch = self.highest_epoch.checked_add(1).ok_or_else(|| anyhow::anyhow!("路径代次已耗尽"))?;
        self.highest_epoch = epoch;
        self.pause();
        self.pending = Some(Pending { offer: RouteOffer { channel: self.channel, epoch, transport }, phase: Phase::Quiescing });
        tracing::info!(channel = ?self.channel, epoch, ?transport, "功能路径开始切换, 等待状态释放");
        Ok(epoch)
    }

    pub fn quiesced(&mut self, epoch: u64) -> Result<RouteMessage> {
        let Some(pending) = self.pending.as_mut() else { bail!("没有正在进行的路径切换"); };
        if pending.offer.epoch != epoch || pending.phase != Phase::Quiescing { bail!("路径状态释放确认不匹配当前切换"); }
        if self.host {
            pending.phase = Phase::AwaitReady;
            Ok(RouteMessage::Offer(pending.offer))
        } else {
            pending.phase = Phase::AwaitCommit;
            Ok(RouteMessage::Ready { channel: self.channel, epoch })
        }
    }

    pub fn receive_message(&mut self, message: RouteMessage) -> Result<RouteEffect> {
        if message.channel() != self.channel { bail!("路径消息属于其他功能"); }
        let epoch = message.epoch();
        if let RouteMessage::Offer(offer) = message {
            if self.host { bail!("client 不能向 host 提出路径方案"); }
            if epoch == 0 || epoch <= self.highest_epoch { bail!("路径方案代次过期或重复"); }
            self.validate_transport(offer.transport)?;
            self.highest_epoch = epoch;
            self.pause();
            self.pending = Some(Pending { offer, phase: Phase::Quiescing });
            return Ok(RouteEffect { quiesce: true, outbound: None });
        }
        let Some(pending) = self.pending else { bail!("没有匹配的路径切换"); };
        if epoch != pending.offer.epoch { bail!("路径确认代次不匹配"); }
        let outbound = match (self.host, pending.phase, message) {
            (true, Phase::AwaitReady, RouteMessage::Ready { .. }) => {
                // 在 commit 发出前接收端已准备好, 避免另一条更快链路的数据超越 Committed.
                self.receive = Some(pending.offer);
                self.last_sequence = 0;
                self.pending.as_mut().expect("已核对 pending").phase = Phase::AwaitCommitted;
                Some(RouteMessage::Commit { channel: self.channel, epoch })
            }
            (false, Phase::AwaitCommit, RouteMessage::Commit { .. }) => {
                self.receive = Some(pending.offer);
                self.last_sequence = 0;
                self.pending.as_mut().expect("已核对 pending").phase = Phase::CommittedReply;
                Some(RouteMessage::Committed { channel: self.channel, epoch })
            }
            (true, Phase::AwaitCommitted, RouteMessage::Committed { .. }) => {
                self.send = Some(pending.offer);
                self.pending = None;
                tracing::info!(channel = ?self.channel, epoch, transport = ?pending.offer.transport, "功能路径提交已确认");
                None
            }
            _ => bail!("路径消息与当前协商阶段不符"),
        };
        Ok(RouteEffect { quiesce: false, outbound })
    }

    /// client 必须先发送并 flush Committed, 之后才开启新路径发送.
    pub fn reply_sent(&mut self, message: RouteMessage) -> Result<()> {
        let Some(pending) = self.pending else { bail!("没有匹配的路径切换"); };
        if self.host || pending.phase != Phase::CommittedReply || message != (RouteMessage::Committed { channel: self.channel, epoch: pending.offer.epoch }) {
            bail!("路径提交发送确认不匹配");
        }
        self.send = Some(pending.offer);
        self.pending = None;
        tracing::info!(channel = ?self.channel, epoch = pending.offer.epoch, transport = ?pending.offer.transport, "功能路径提交确认已发送");
        Ok(())
    }

    /// 收紧策略时立即暂停不再允许的路径, 返回 true 要求调用方释放本机状态.
    pub fn update_policy(&mut self, policy: PathPolicy) -> bool {
        self.policy = policy;
        let disallowed = [self.send, self.receive, self.pending.map(|pending| pending.offer)]
            .into_iter().flatten().any(|route| self.validate_transport(route.transport).is_err());
        if disallowed { self.cancel(); }
        disallowed
    }

    pub fn latest_epoch(&self) -> u64 { self.highest_epoch }
    pub fn sending_route(&self) -> Option<RouteOffer> { self.send }
    pub fn receiving_route(&self) -> Option<RouteOffer> { self.receive }
    pub fn is_switching(&self) -> bool { self.pending.is_some() }

    pub fn accept(&mut self, stamp: RouteStamp) -> bool {
        let Some(route) = self.receive else { return false; };
        if route.transport != Some(stamp.transport) || route.epoch != stamp.epoch || stamp.sequence == 0 || stamp.sequence <= self.last_sequence { return false; }
        self.last_sequence = stamp.sequence;
        true
    }

    /// 中断时不恢复已释放的旧输入状态, 重新协商并由用户重新激活.
    pub fn cancel(&mut self) { self.pause(); self.pending = None; }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(host: &mut ChannelRoute, client: &mut ChannelRoute, transport: TransportKind) -> u64 {
        let epoch = host.begin(Some(transport)).unwrap();
        let offer = host.quiesced(epoch).unwrap();
        assert!(client.receive_message(offer).unwrap().quiesce);
        let ready = client.quiesced(epoch).unwrap();
        let commit = host.receive_message(ready).unwrap().outbound.unwrap();
        let committed = client.receive_message(commit).unwrap().outbound.unwrap();
        client.reply_sent(committed).unwrap();
        host.receive_message(committed).unwrap();
        epoch
    }

    #[test]
    fn policy_intersection_never_bypasses_only_constraints_or_audio_support() {
        let links = AvailableLinks { lan: true, bluetooth: true };
        assert_eq!(choose_route(FunctionalChannel::Input, PathPolicy::BluetoothOnly, PathPolicy::LanOnly, links, None), RouteChoice::Paused(PauseReason::PolicyConflict));
        assert_eq!(choose_route(FunctionalChannel::Clipboard, PathPolicy::BluetoothOnly, PathPolicy::Auto, AvailableLinks { lan: true, bluetooth: false }, None), RouteChoice::Paused(PauseReason::TransportUnavailable));
        assert_eq!(choose_route(FunctionalChannel::Audio, PathPolicy::BluetoothOnly, PathPolicy::Auto, links, None), RouteChoice::Paused(PauseReason::UnsupportedChannel));
        assert_eq!(choose_route(FunctionalChannel::Clipboard, PathPolicy::Auto, PathPolicy::Auto, links, Some(TransportKind::Bluetooth)), RouteChoice::Selected(TransportKind::Bluetooth));
        assert_eq!(choose_route(FunctionalChannel::Input, PathPolicy::Auto, PathPolicy::PreferBluetooth, links, Some(TransportKind::Lan)), RouteChoice::Selected(TransportKind::Bluetooth));
    }

    #[test]
    fn data_cannot_overtake_the_committed_ack_on_another_link() {
        let mut host = ChannelRoute::new(FunctionalChannel::Input, true, PathPolicy::PreferBluetooth);
        let mut client = ChannelRoute::new(FunctionalChannel::Input, false, PathPolicy::PreferBluetooth);
        let epoch = host.begin(Some(TransportKind::Bluetooth)).unwrap();
        assert!(host.sending_route().is_none());
        client.receive_message(host.quiesced(epoch).unwrap()).unwrap();
        assert!(client.sending_route().is_none());
        let commit = host.receive_message(client.quiesced(epoch).unwrap()).unwrap().outbound.unwrap();
        let ack = client.receive_message(commit).unwrap().outbound.unwrap();
        assert!(client.sending_route().is_none());
        client.reply_sent(ack).unwrap();
        let stamp = RouteStamp { epoch, transport: TransportKind::Bluetooth, sequence: 1 };
        assert!(host.accept(stamp));
        assert!(host.sending_route().is_none());
        host.receive_message(ack).unwrap();
        assert!(host.sending_route().is_some());
    }

    #[test]
    fn switching_rejects_old_epoch_duplicates_wrong_link_and_stale_acks() {
        let mut host = ChannelRoute::new(FunctionalChannel::Input, true, PathPolicy::PreferBluetooth);
        let mut client = ChannelRoute::new(FunctionalChannel::Input, false, PathPolicy::PreferBluetooth);
        let old = commit(&mut host, &mut client, TransportKind::Lan);
        let packet = RouteStamp { epoch: old, transport: TransportKind::Lan, sequence: 1 };
        assert!(client.accept(packet));
        assert!(!client.accept(packet));
        let new = host.begin(Some(TransportKind::Bluetooth)).unwrap();
        assert!(!host.accept(packet));
        assert!(host.receive_message(RouteMessage::Ready { channel: FunctionalChannel::Input, epoch: old }).is_err());
        client.receive_message(host.quiesced(new).unwrap()).unwrap();
        assert!(!client.accept(packet));
        let commit = host.receive_message(client.quiesced(new).unwrap()).unwrap().outbound.unwrap();
        let ack = client.receive_message(commit).unwrap().outbound.unwrap();
        client.reply_sent(ack).unwrap(); host.receive_message(ack).unwrap();
        assert!(!client.accept(RouteStamp { epoch: new, ..packet }));
        assert!(client.accept(RouteStamp { epoch: new, transport: TransportKind::Bluetooth, sequence: 1 }));
        client.cancel();
        assert!(!client.accept(RouteStamp { epoch: new, transport: TransportKind::Bluetooth, sequence: 2 }));
        assert!(client.receive_message(RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Input, epoch: new, transport: None })).is_err());
    }

    #[test]
    fn channel_switches_are_independent_and_unquiesced_ready_is_rejected() {
        let mut input_host = ChannelRoute::new(FunctionalChannel::Input, true, PathPolicy::PreferBluetooth);
        let mut input_client = ChannelRoute::new(FunctionalChannel::Input, false, PathPolicy::PreferBluetooth);
        commit(&mut input_host, &mut input_client, TransportKind::Bluetooth);
        let mut clipboard = ChannelRoute::new(FunctionalChannel::Clipboard, true, PathPolicy::Auto);
        let epoch = clipboard.begin(Some(TransportKind::Lan)).unwrap();
        assert!(clipboard.receive_message(RouteMessage::Ready { channel: FunctionalChannel::Clipboard, epoch }).is_err());
        assert_eq!(input_host.sending_route().unwrap().transport, Some(TransportKind::Bluetooth));
        assert!(input_host.receive_message(RouteMessage::Ready { channel: FunctionalChannel::Clipboard, epoch }).is_err());
        assert!(ChannelRoute::new(FunctionalChannel::Audio, true, PathPolicy::LanOnly).begin(Some(TransportKind::Bluetooth)).is_err());
        let mut restricted = ChannelRoute::new(FunctionalChannel::Input, false, PathPolicy::LanOnly);
        assert!(restricted.receive_message(RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Input, epoch: 1, transport: Some(TransportKind::Bluetooth) })).is_err());
        assert!(input_client.update_policy(PathPolicy::LanOnly));
        assert!(input_client.sending_route().is_none());
        assert!(input_client.receiving_route().is_none());
        assert!(!input_client.accept(RouteStamp { epoch: 1, transport: TransportKind::Bluetooth, sequence: 1 }));
    }
}
