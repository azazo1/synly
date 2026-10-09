//! 音频只使用实际已绑定的 LAN 地址, 蓝牙主控制不改变 UDP 的安全域.
use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LanAudioPath { pub(super) binding_id: Uuid, pub(super) peer: SocketAddr }
impl LanAudioPath {
    pub(super) fn accepts(self, binding_id: Uuid, port: u16, channel_id: [u8; 32]) -> bool { self.binding_id == binding_id && !binding_id.is_nil() && port != 0 && channel_id != [0; 32] }
}
pub(super) fn select(primary: TransportKind, primary_peer: Option<SocketAddr>, session_id: Uuid, secondary: Option<&synly_core::transport::logical::SecondaryTunnel>, remote: synly_core::transport::routing::AvailableLinks) -> Option<LanAudioPath> {
    if !remote.lan { return None; }
    let (binding_id, peer) = if primary == TransportKind::Lan { (session_id, primary_peer?) }
        else { let secondary = secondary.filter(|secondary| secondary.transport() == TransportKind::Lan)?; (secondary.binding_id(), secondary.lan_peer()?) };
    if binding_id.is_nil() || peer.port() == 0 || peer.ip().is_unspecified() || peer.ip().is_multicast() { return None; }
    Some(LanAudioPath { binding_id, peer })
}

pub(super) async fn refresh(state: &CapabilityState, runtime: &mut CapabilityTaskRuntime, context: &CapabilityRefreshContext<'_>) -> Result<()> {
    let epoch = state.epoch(); let local = state.effective_local(); let remote = state.effective_remote();
    let audio_plan = state.audio_ready().then(|| resolve_audio_plan(context.session_role, local.audio_mode, remote.audio_mode)).flatten().filter(|_| context.audio_lan.is_some());
    let signature = (epoch, context.audio_lan);
    if runtime.audio_blocked == Some(signature) { return Ok(()); }
    if runtime.audio_epoch == Some(epoch) && runtime.audio_plan == audio_plan && runtime.audio_lan == context.audio_lan { return Ok(()); }
    runtime.stop_audio().await;
    runtime.audio_epoch = Some(epoch); runtime.audio_plan = audio_plan; runtime.audio_lan = context.audio_lan; runtime.audio_blocked = None;
    let Some(plan) = audio_plan else { return Ok(()); }; let path = context.audio_lan.context("音频缺少已经绑定的 LAN 路径")?;
    match plan.role {
        LocalAudioRole::Receive => match audio::bind_and_spawn_receiver_with_config(
            audio::derive_route_secret(context.audio_master_secret, path.binding_id)?, plan.direction, path.peer.ip(),
            audio::CodecConfig { layout: context.audio_layout, ..audio::CodecConfig::default() },
        ) {
            Ok((task, port, channel_id)) => {
                context.tx.send(Frame::Control(ControlMessage::AudioUdpReady { epoch, path_id: path.binding_id, port,
                    layout: match context.audio_layout { audio::AudioLayout::Stereo => ProtocolAudioLayout::Stereo, audio::AudioLayout::Surround51 => ProtocolAudioLayout::Surround51, audio::AudioLayout::Surround71 => ProtocolAudioLayout::Surround71 }, channel_id })).await?;
                runtime.audio_task = Some(task);
            }
            Err(error) => { tracing::warn!(error = %error, "无法准备音频 LAN 接收通道"); runtime.audio_blocked = Some(signature); context.tx.send(Frame::Control(ControlMessage::AudioPathFailed { epoch, path_id: path.binding_id })).await?; }
        },
        LocalAudioRole::Send => {
            runtime.audio_deadline = Some(Instant::now() + Duration::from_secs(10));
            tracing::info!(?epoch, binding = %path.binding_id, "音频发送端等待已绑定 LAN 的 UDP 接收端口");
        }
    }
    Ok(())
}

pub(super) struct Ready { pub(super) epoch: CapabilityEpoch, pub(super) path_id: Uuid, pub(super) port: u16, pub(super) layout: ProtocolAudioLayout, pub(super) channel_id: [u8; 32] }
pub(super) fn receive(ready: Ready, state: &CapabilityState, runtime: &mut CapabilityTaskRuntime, master_secret: [u8; 32]) -> Result<()> {
    if !state.current_epoch(ready.epoch) || runtime.audio_epoch != Some(ready.epoch) || runtime.audio_blocked == Some((ready.epoch, runtime.audio_lan)) { return Ok(()); }
    let Some(path) = runtime.audio_lan.filter(|path| path.accepts(ready.path_id, ready.port, ready.channel_id)) else {
        tracing::debug!(path_id = %ready.path_id, "忽略已撤销或无效音频 LAN 路径的准备消息"); return Ok(());
    };
    let Some(AudioPlan { role: LocalAudioRole::Send, direction }) = runtime.audio_plan else { return Ok(()); };
    if runtime.audio_task.is_some() { return Ok(()); }
    let codec = audio::CodecConfig { layout: match ready.layout { ProtocolAudioLayout::Stereo => audio::AudioLayout::Stereo, ProtocolAudioLayout::Surround51 => audio::AudioLayout::Surround51, ProtocolAudioLayout::Surround71 => audio::AudioLayout::Surround71 }, ..audio::CodecConfig::default() };
    match audio::spawn_sender_with_config(audio::derive_route_secret(master_secret, path.binding_id)?, ready.channel_id, direction, SocketAddr::new(path.peer.ip(), ready.port), codec) {
        Ok(task) => { runtime.audio_deadline = None; runtime.audio_task = Some(task); }
        Err(error) => { tracing::warn!(error = %error, "无法启动音频 LAN 发送通道"); runtime.audio_blocked = Some((ready.epoch, runtime.audio_lan)); }
    }
    Ok(())
}
pub(super) async fn finish(task: &mut Option<audio::AudioTaskHandle>) -> Result<()> {
    match task { Some(task) => task.wait_finished().await, None => std::future::pending().await }
}
pub(super) async fn fail(runtime: &mut CapabilityTaskRuntime) -> Option<ControlMessage> {
    let epoch = runtime.audio_epoch; let path = runtime.audio_lan;
    runtime.stop_audio().await;
    if let Some(epoch) = epoch { runtime.audio_blocked = Some((epoch, path)); }
    tracing::warn!("音频 LAN 路径已暂停, 控制, 输入和剪贴板继续运行");
    epoch.zip(path).map(|(epoch, path)| ControlMessage::AudioPathFailed { epoch, path_id: path.binding_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use synly_core::transport::routing::AvailableLinks;
    fn peer(name: &str) -> DeviceIdentity {
        let device = synly_core::identity::generate_device_config(name.to_owned()).unwrap();
        DeviceIdentity { device_id: device.device_id, device_name: name.to_owned(), instance_name: None, identity_public_key: device.identity_public_key().unwrap().to_owned(), tls_root_certificate: synly_core::crypto::device_tls_root_certificate(&device).unwrap() }
    }
    #[tokio::test]
    async fn bluetooth_primary_audio_requires_delivered_lan_metadata_and_peer_availability() {
        let a = peer("a"); let b = peer("b"); let id = Uuid::new_v4(); let links = AvailableLinks { lan: true, bluetooth: true };
        let client = LogicalSession::new(id, a.clone(), b.clone(), TransportKind::Bluetooth, [4; 32]).unwrap();
        let host = LogicalSession::new(id, b.clone(), a.clone(), TransportKind::Bluetooth, [4; 32]).unwrap();
        let _owner_a = client.owner().unwrap(); let _owner_b = host.owner().unwrap();
        assert!(select(TransportKind::Bluetooth, None, id, None, links).is_none());
        let (x, y) = tokio::io::duplex(4096);
        let (x, y) = tokio::join!(client.connect(ByteStream::new(x), &b, TransportKind::Lan, [7; 32]), host.accept(ByteStream::new(y), &a, TransportKind::Lan, [7; 32]));
        let (x, y) = (x.unwrap(), y.unwrap()); assert_eq!(x.binding_id(), y.binding_id()); let binding_id = x.binding_id();
        let mut tunnel = x.multiplex(); assert!(select(TransportKind::Bluetooth, None, id, Some(&tunnel), links).is_none());
        drop(tunnel); drop(y);
        let (x, y) = tokio::io::duplex(4096);
        let (x, y) = tokio::join!(client.connect(ByteStream::new(x), &b, TransportKind::Lan, [8; 32]), host.accept(ByteStream::new(y), &a, TransportKind::Lan, [8; 32]));
        let y = y.unwrap(); assert_ne!(binding_id, y.binding_id());
        tunnel = x.unwrap().with_lan_peer("127.0.0.1:5050".parse().unwrap()).unwrap().multiplex();
        let path = select(TransportKind::Bluetooth, None, id, Some(&tunnel), links).unwrap(); assert_eq!(path.binding_id, y.binding_id());
        assert!(path.accepts(path.binding_id, 48000, [8; 32])); assert!(!path.accepts(binding_id, 48000, [8; 32])); assert!(!path.accepts(path.binding_id, 0, [8; 32]));
        assert!(select(TransportKind::Bluetooth, None, id, Some(&tunnel), AvailableLinks { lan: false, bluetooth: true }).is_none());
        drop(tunnel); drop(y); assert!(client.is_open() && host.is_open()); assert!(!client.has(TransportKind::Lan));
    }
    #[tokio::test]
    async fn audio_loss_and_failure_do_not_restart_without_new_path_or_capability() {
        let caps = RuntimeCapabilities { clipboard_mode: ClipboardMode::Off, audio_mode: AudioMode::Send, input_mode: InputMode::Off };
        let state = CapabilityState::new(true, caps, RuntimeCapabilities { audio_mode: AudioMode::Receive, ..caps });
        let mut runtime = CapabilityTaskRuntime::new(&crate::clipboard::ClipboardRuntimeOptions { path: synly_core::transport::routing::PathPolicy::Auto, max_file_bytes: 1024, max_cache_bytes: None, cache_dir: ".tmp/audio-path-unused".into() }, SessionRole::Host);
        let tuning = InputRuntimeOptions { mode: InputMode::Off, path: synly_core::transport::routing::PathPolicy::Auto, edge: input::ScreenEdge::Left, hotkey: input::Hotkey::DEFAULT.parse().unwrap(), reverse_mouse_wheel: false, reverse_trackpad: false, native_scroll_macos_to_windows: false, native_scroll_windows_to_macos: false, block_switch_on_press: false, filter_app_events: false, key_mapping: input::KeyMappingConfig::default(), cursor_mode: input::CursorMode::Desktop };
        let activity = Arc::new(AtomicBool::new(false)); let (wire, _peer) = tokio::io::duplex(4096);
        let (tx, _inbox, _guard) = synly_core::transport::frames::open_control(ByteStream::new(wire), TransferLimits::default());
        let mut context = CapabilityRefreshContext { session_role: SessionRole::Host, peer_device_id: Uuid::nil(), input_mux: None, input_transport: None, remote_socket_addr: None,
            audio_master_secret: [7; 32], audio_lan: None, audio_layout: audio::AudioLayout::Stereo, input_master_secret: [1; 32], input_options: &tuning,
            input_inbox: None, input_session_id: None, input_socket_tx: None, input_routes: None, input_activity: &activity, clipboard_hub: None, tx: &tx };
        refresh(&state, &mut runtime, &context).await.unwrap(); assert!(runtime.audio_plan.is_none() && runtime.audio_task.is_none());
        let path = LanAudioPath { binding_id: Uuid::new_v4(), peer: "127.0.0.1:5050".parse().unwrap() }; context.audio_lan = Some(path);
        refresh(&state, &mut runtime, &context).await.unwrap(); assert!(runtime.audio_deadline.is_some() && runtime.audio_task.is_none());
        receive(Ready { epoch: state.epoch(), path_id: Uuid::new_v4(), port: 48000, layout: ProtocolAudioLayout::Stereo, channel_id: [8; 32] }, &state, &mut runtime, [7; 32]).unwrap(); assert!(runtime.audio_task.is_none());
        assert!(matches!(fail(&mut runtime).await, Some(ControlMessage::AudioPathFailed { path_id, .. }) if path_id == path.binding_id));
        refresh(&state, &mut runtime, &context).await.unwrap(); assert!(runtime.audio_deadline.is_none());
        context.audio_lan = None; refresh(&state, &mut runtime, &context).await.unwrap(); assert!(runtime.audio_plan.is_none());
        context.audio_lan = Some(LanAudioPath { binding_id: Uuid::new_v4(), ..path });
        refresh(&state, &mut runtime, &context).await.unwrap(); assert!(runtime.audio_deadline.is_some() && runtime.audio_blocked.is_none());
        tx.send(Frame::Control(ControlMessage::Goodbye)).await.unwrap();
    }
}
