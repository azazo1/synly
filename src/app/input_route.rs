//! 输入路径握手与副承载选择, 主控制连接始终保持不变.
use super::*;
use synly_core::transport::routing::{AvailableLinks, RouteMessage, FunctionalChannel};
use synly_core::transport::{logical::SecondaryTunnel, generation::GenerationLane};

pub(super) fn available(primary: TransportKind, secondary: Option<&SecondaryTunnel>) -> AvailableLinks {
    let mut links = AvailableLinks { lan: primary == TransportKind::Lan, bluetooth: primary == TransportKind::Bluetooth };
    if let Some(secondary) = secondary { match secondary.transport() { TransportKind::Lan => links.lan = true, TransportKind::Bluetooth => links.bluetooth = true } }
    links
}
pub(super) fn select(primary: TransportKind, primary_lane: Option<&GenerationLane>, secondary: Option<&SecondaryTunnel>, remote: AvailableLinks, local_policy: synly_core::transport::routing::PathPolicy, remote_policy: synly_core::transport::routing::PathPolicy, current: Option<TransportKind>) -> (synly_core::transport::routing::RouteChoice, Option<GenerationLane>) {
    let local = available(primary, secondary);
    let shared = AvailableLinks { lan: local.lan && remote.lan, bluetooth: local.bluetooth && remote.bluetooth };
    let choice = synly_core::transport::routing::choose_route(FunctionalChannel::Input, local_policy, remote_policy, shared, current);
    let lane = match choice.transport() {
        Some(kind) if kind == primary => primary_lane.cloned(),
        Some(kind) => secondary.filter(|secondary| secondary.transport() == kind).map(|secondary| secondary.channels.input.clone()),
        None => None,
    };
    (choice, lane)
}
pub(super) async fn task_finished(task: &mut Option<tokio::task::JoinHandle<()>>) {
    match task { Some(handle) => { let _ = handle.await; task.take(); }, None => std::future::pending().await }
}
pub(super) async fn fail(runtime: &mut CapabilityTaskRuntime, state: &CapabilityState, context: CapabilityRefreshContext<'_>, notify: bool) -> Result<()> {
    let epoch = state.epoch(); let generation = runtime.input_generation;
    runtime.input_requires_manual = true;
    runtime.stop_input(context.input_session_id, context.input_routes).await;
    context.input_activity.store(false, Ordering::Release);
    runtime.input_blocked = Some((epoch, context.input_transport));
    if notify && let Some(generation) = generation { context.tx.send(Frame::Control(ControlMessage::InputPathFailed { epoch, generation })).await?; }
    tracing::warn!(?epoch, "输入路径已暂停, 不影响主控制与剪贴板");
    Ok(())
}
pub(super) async fn receive(epoch: CapabilityEpoch, generation: Uuid, message: RouteMessage, state: &CapabilityState, runtime: &mut CapabilityTaskRuntime, tasks: &mut SessionTaskAbortGuard, context: CapabilityRefreshContext<'_>) -> Result<()> {
    if !state.current_epoch(epoch) || !state.is_local_acknowledged() { return Ok(()); }
    if generation.is_nil() { bail!("输入路径代次为空"); }
    let local = state.effective_local(); let remote = state.effective_remote();
    let role = negotiate_input(local.input_mode, remote.input_mode).context("未协商输入角色却收到路径消息")?;
    if let RouteMessage::Offer(offer) = message {
        if !matches!(context.session_role, SessionRole::Client) || offer.channel != FunctionalChannel::Input || offer.transport.is_none() { bail!("输入路径方案角色或功能类型无效"); }
        // 已完成的同一 lease 不重新开启, 不能把重复消息用于复活旧输入.
        if runtime.input_generation == Some(generation) || offer.epoch <= runtime.input_route.latest_epoch() { return Ok(()); }
        let Some(lane) = context.input_mux.filter(|_| context.input_transport == offer.transport) else {
            context.tx.send(Frame::Control(ControlMessage::InputPathFailed { epoch, generation })).await?;
            return Ok(());
        };
        runtime.stop_input(context.input_session_id, context.input_routes).await;
        context.input_activity.store(false, Ordering::Release);
        runtime.input_route.receive_message(message)?;
        let stream = lane.lease(generation)?;
        runtime.input_generation = Some(generation); runtime.input_epoch = Some(epoch); runtime.input_role = Some(role); runtime.input_uses_mux = true; runtime.input_transport = context.input_transport;
        runtime.pending_mux_input = Some((generation, stream, Instant::now() + Duration::from_secs(10)));
        let ready = runtime.input_route.quiesced(offer.epoch)?;
        context.tx.send(Frame::Control(ControlMessage::InputPath { epoch, generation, message: ready })).await?;
        return Ok(());
    }
    // 接入/失败消息可能已经在主控制队列中, 迟到的旧 lease 不影响新路径.
    if runtime.input_generation != Some(generation) || runtime.input_epoch != Some(epoch) { return Ok(()); }
    let effect = runtime.input_route.receive_message(message)?;
    if let Some(outbound) = effect.outbound {
        context.tx.send_and_flush(Frame::Control(ControlMessage::InputPath { epoch, generation, message: outbound })).await?;
        if matches!(outbound, RouteMessage::Committed { .. }) { runtime.input_route.reply_sent(outbound)?; }
    }
    if runtime.input_route.is_switching() { return Ok(()); }
    let (_, stream, _) = runtime.pending_mux_input.take().context("路径提交缺少已准备的输入租约")?;
    let mut tuning = context.input_options.clone(); tuning.mode = local.input_mode;
    let task = bluetooth::spawn_input(stream, role, tuning, context.input_master_secret, Arc::clone(context.input_activity), epoch, runtime.input_requires_manual);
    tasks.track(&task); runtime.input_task = Some(task);
    tracing::info!(?epoch, transport = ?runtime.input_transport, route_epoch = message.epoch(), manual = runtime.input_requires_manual, "输入复用路径已完成四阶段提交");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use synly_core::transport::{stream::ByteStream, routing::RouteOffer};
    fn options() -> InputRuntimeOptions {
        InputRuntimeOptions { mode: InputMode::Send, path: synly_core::transport::routing::PathPolicy::PreferBluetooth, edge: input::ScreenEdge::Left, hotkey: input::Hotkey::DEFAULT.parse().unwrap(), reverse_mouse_wheel: false, reverse_trackpad: false, native_scroll_macos_to_windows: false, native_scroll_windows_to_macos: false, block_switch_on_press: false, filter_app_events: false, key_mapping: input::KeyMappingConfig::default(), cursor_mode: input::CursorMode::Desktop }
    }
    fn runtime(role: SessionRole) -> CapabilityTaskRuntime {
        CapabilityTaskRuntime::new(&crate::clipboard::ClipboardRuntimeOptions { path: synly_core::transport::routing::PathPolicy::Auto, max_file_bytes: 1024, max_cache_bytes: None, cache_dir: PathBuf::from(".tmp/input-route-unused") }, role)
    }
    fn context<'a>(role: SessionRole, lane: &'a GenerationLane, options: &'a InputRuntimeOptions, activity: &'a Arc<AtomicBool>, tx: &'a FrameSender) -> CapabilityRefreshContext<'a> {
        CapabilityRefreshContext { session_role: role, peer_device_id: Uuid::nil(), input_mux: Some(lane), input_transport: Some(TransportKind::Bluetooth), remote_socket_addr: None,
            audio_master_secret: [0; 32], audio_lan: None, audio_layout: audio::AudioLayout::Stereo, input_master_secret: [1; 32], input_options: options,
            input_inbox: None, input_session_id: None, input_socket_tx: None, input_routes: None, input_activity: activity, clipboard_hub: None, tx }
    }
    #[tokio::test]
    async fn client_offer_prepares_lease_and_ready_without_starting_capture() {
        let (a, _b) = tokio::io::duplex(4096); let (_control, channels) = synly_core::transport::bluetooth::open(ByteStream::new(a));
        let (wire, peer) = tokio::io::duplex(4096); let (tx, _inbox, _guard) = synly_core::transport::frames::open(ByteStream::new(wire), None, TransferLimits::default());
        let caps = RuntimeCapabilities { clipboard_mode: ClipboardMode::Off, audio_mode: AudioMode::Off, input_mode: InputMode::Send };
        let state = CapabilityState::new(false, caps, RuntimeCapabilities { input_mode: InputMode::Receive, ..caps });
        let mut runtime = runtime(SessionRole::Client); let mut tasks = SessionTaskAbortGuard::default();
        let tuning = options(); let activity = Arc::new(AtomicBool::new(false)); let generation = Uuid::new_v4();
        let message = RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Input, epoch: 1, transport: Some(TransportKind::Bluetooth) });
        receive(state.epoch(), generation, message, &state, &mut runtime, &mut tasks, context(SessionRole::Client, &channels.input, &tuning, &activity, &tx)).await.unwrap();
        assert!(runtime.input_task.is_none() && runtime.pending_mux_input.is_some());
        assert!(runtime.input_route.sending_route().is_none());
        let mut reader = FrameReader::with_limits(peer, TransferLimits::default());
        assert!(matches!(reader.read_frame().await.unwrap(), Frame::Control(ControlMessage::InputPath { generation: actual, message: RouteMessage::Ready { epoch: 1, .. }, .. }) if actual == generation));
        // 旧路径的重复 offer 不能释放已经准备的新 lease 或重置人工激活标记.
        let stale_generation = Uuid::new_v4();
        receive(state.epoch(), stale_generation, message, &state, &mut runtime, &mut tasks, context(SessionRole::Client, &channels.input, &tuning, &activity, &tx)).await.unwrap();
        assert_eq!(runtime.input_generation, Some(generation));
        assert!(!runtime.input_requires_manual);
        fail(&mut runtime, &state, context(SessionRole::Client, &channels.input, &tuning, &activity, &tx), false).await.unwrap();
        assert!(runtime.pending_mux_input.is_none() && runtime.input_requires_manual);
    }
    #[tokio::test]
    async fn host_ready_sends_commit_but_missing_committed_never_starts_input() {
        let (a, _b) = tokio::io::duplex(4096); let (_control, channels) = synly_core::transport::bluetooth::open(ByteStream::new(a));
        let (wire, peer) = tokio::io::duplex(4096); let (tx, _inbox, _guard) = synly_core::transport::frames::open(ByteStream::new(wire), None, TransferLimits::default());
        let caps = RuntimeCapabilities { clipboard_mode: ClipboardMode::Off, audio_mode: AudioMode::Off, input_mode: InputMode::Send };
        let state = CapabilityState::new(true, caps, RuntimeCapabilities { input_mode: InputMode::Receive, ..caps });
        let mut runtime = runtime(SessionRole::Host); let mut tasks = SessionTaskAbortGuard::default();
        let tuning = options(); let activity = Arc::new(AtomicBool::new(false)); let generation = Uuid::new_v4();
        let epoch = runtime.input_route.begin(Some(TransportKind::Bluetooth)).unwrap(); runtime.input_route.quiesced(epoch).unwrap();
        runtime.input_generation = Some(generation); runtime.input_epoch = Some(state.epoch());
        runtime.pending_mux_input = Some((generation, channels.input.lease(generation).unwrap(), Instant::now() + Duration::from_secs(10)));
        receive(state.epoch(), generation, RouteMessage::Ready { channel: FunctionalChannel::Input, epoch }, &state, &mut runtime, &mut tasks, context(SessionRole::Host, &channels.input, &tuning, &activity, &tx)).await.unwrap();
        assert!(runtime.input_task.is_none() && runtime.pending_mux_input.is_some());
        assert!(runtime.input_route.sending_route().is_none());
        let mut reader = FrameReader::with_limits(peer, TransferLimits::default());
        assert!(matches!(reader.read_frame().await.unwrap(), Frame::Control(ControlMessage::InputPath { generation: actual, message: RouteMessage::Commit { .. }, .. }) if actual == generation));
        fail(&mut runtime, &state, context(SessionRole::Host, &channels.input, &tuning, &activity, &tx), true).await.unwrap();
        assert!(runtime.input_requires_manual && runtime.input_route.receiving_route().is_none());
        assert_eq!(runtime.input_blocked, Some((state.epoch(), Some(TransportKind::Bluetooth))));
    }
    #[test]
    fn selection_respects_policies_and_never_invents_a_secondary() {
        use synly_core::transport::routing::{PathPolicy, PauseReason, RouteChoice};
        let remote = AvailableLinks { lan: true, bluetooth: true };
        assert_eq!(select(TransportKind::Lan, None, None, remote, PathPolicy::BluetoothOnly, PathPolicy::Auto, None).0, RouteChoice::Paused(PauseReason::TransportUnavailable));
        assert_eq!(select(TransportKind::Lan, None, None, remote, PathPolicy::BluetoothOnly, PathPolicy::LanOnly, None).0, RouteChoice::Paused(PauseReason::PolicyConflict));
        assert_eq!(select(TransportKind::Lan, None, None, remote, PathPolicy::PreferBluetooth, PathPolicy::Auto, None).0.transport(), Some(TransportKind::Lan));
        assert_eq!(select(TransportKind::Bluetooth, None, None, remote, PathPolicy::LanOnly, PathPolicy::Auto, None).0.transport(), None);
        assert_eq!(select(TransportKind::Bluetooth, None, None, remote, PathPolicy::Auto, PathPolicy::Auto, Some(TransportKind::Bluetooth)).0.transport(), Some(TransportKind::Bluetooth));
    }
    #[tokio::test]
    async fn completed_task_is_consumed_before_state_release() {
        let mut runtime = runtime(SessionRole::Client); runtime.input_task = Some(tokio::spawn(async {}));
        task_finished(&mut runtime.input_task).await; assert!(runtime.input_task.is_none());
        runtime.stop_input(None, None).await;
    }
}
