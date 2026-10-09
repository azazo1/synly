//! 桌面运行层的蓝牙接入与明确身份授权.

use super::*;
use synly_core::bluetooth::{self as native, session as auth};

pub(super) fn parse_target(query: &str) -> Result<Option<(String, Option<Uuid>)>> {
    let Some(value) = query.strip_prefix("bluetooth:") else { return Ok(None) };
    let (address, id) = value.split_once('/').map_or((value, None), |(address, id)| (address, Some(id)));
    let address = native::normalize_address(address)?;
    let id = id.map(Uuid::parse_str).transpose()?;
    if id.is_some_and(|id| id.is_nil()) { bail!("蓝牙应用身份线索不能是空 UUID"); }
    Ok(Some((address, id)))
}

pub(super) fn auth_config(config: &SynlyConfig, options: &RuntimeOptions) -> auth::AuthConfig {
    auth::AuthConfig { device: config.device.clone(), instance_name: options.instance_name.clone(),
        // 音频只允许 LAN, 输入使用 TLS 内有界独立复用通道.
        capabilities: RuntimeCapabilities { clipboard_mode: options.clipboard_mode, audio_mode: AudioMode::Off, input_mode: options.input_mode },
        policies: Default::default(), trusted_devices: config.trusted_devices.clone(), request_trust: options.pairing.trust_device,
        trusted_only: options.pairing.trusted_only }
}

struct PromptGuard { id: Uuid, control: RuntimeControl }
impl Drop for PromptGuard {
    fn drop(&mut self) { self.control.notify_interaction(InteractionRequest::Clear { request_id: self.id }); }
}
async fn authorize(request: auth::AuthorizationRequest, options: &RuntimeOptions) -> Result<auth::AuthorizationDecision> {
    if options.pairing.headless { bail!("headless 模式不能授权未知蓝牙应用身份"); }
    let id = Uuid::new_v4();
    let _guard = PromptGuard { id, control: options.control.clone() };
    let mut summary = vec![format!("身份指纹: {}", request.fingerprint), format!("系统蓝牙地址仅为发现线索: {}", request.system_peer.address),
        "系统安全 RFCOMM 不代替 Synly 应用授权, 无需第二组应用 PIN".to_owned()];
    if request.changed_identity { summary.insert(0, "警告: 同一设备 ID 的身份公钥已经改变, 必须核实".to_owned()); }
    summary.extend(request.capabilities.summary_lines());
    match options.control.request_interaction(InteractionRequest::AcceptPeer {
        request_id: id, display_name: identity_display_name(&request.peer), device_id: request.peer.device_id,
        summary, default_trust: false,
    }).await? {
        InteractionResponse::Decision { accepted, trust } => Ok(auth::AuthorizationDecision { accepted, remember: accepted && trust }),
        InteractionResponse::Cancel => Ok(auth::AuthorizationDecision::default()),
        _ => bail!("无效的蓝牙身份授权响应"),
    }
}

fn established(authenticated: auth::AuthenticatedBluetooth, config: &mut SynlyConfig, role: SessionRole, profile: SessionCapabilityProfile) -> Result<AuthenticatedSession> {
    let require_existing_session = authenticated.trusted_reconnect && !config.trusted_device(&authenticated.remote.device_id).is_some_and(|known| crypto::public_keys_match(&known.public_key, &authenticated.remote.identity_public_key));
    if authenticated.remember_peer {
        config.remember_trusted_device(authenticated.remote.device_id, authenticated.remote.device_name.clone(), authenticated.remote.identity_public_key.clone(), authenticated.remote.tls_root_certificate.clone());
        config.save_trusted_devices()?;
    } else if config.trusted_device(&authenticated.remote.device_id).is_some_and(|known| crypto::public_keys_match(&known.public_key, &authenticated.remote.identity_public_key)) {
        config.note_trusted_device_session(authenticated.remote.device_id, &authenticated.remote.device_name);
        config.save_trusted_devices()?;
    }
    let keys = SessionKeys::bluetooth(&authenticated.stream, authenticated.session_id, authenticated.link_master_secret)?;
    let logical = keys.logical(device_identity(&config.device, None), authenticated.remote.clone(), TransportKind::Bluetooth)?;
    Ok(AuthenticatedSession { role, require_existing_session, trusted_reconnect: authenticated.trusted_reconnect, stream: ByteStream::new(authenticated.stream), transport: TransportKind::Bluetooth, logical, candidate_exporter: keys.candidate_exporter(), secondary_inbox: None, remote: authenticated.remote,
        remote_capabilities: authenticated.capabilities, remote_socket_addr: None,
        audio_master_secret: authenticated.audio_master_secret, input_master_secret: authenticated.input_master_secret, capability_profile: profile })
}

fn usable_trusted_device(device: &TrustedDeviceConfig) -> bool {
    !device.public_key.trim().is_empty() && !device.tls_root_certificate.trim().is_empty()
}

/// 解析蓝牙目标的长期信任身份.
/// 蓝牙发现只能拿到系统地址, 拿不到对端设备 ID, 因此未指定 ID 时沿用核心客户端的规则:
/// 只保存一条可用信任才按它建立可信连接, 有多条时无法只凭地址选择, 交给交互授权.
pub(super) fn expected_trusted_device(trusted: &[TrustedDeviceConfig], id: Option<Uuid>) -> Option<TrustedDeviceConfig> {
    match id {
        Some(id) => trusted.iter().find(|device| device.device_id == id).filter(|device| usable_trusted_device(device)).cloned(),
        None => {
            let mut usable = trusted.iter().filter(|device| usable_trusted_device(device));
            let only = usable.next()?;
            usable.next().is_none().then(|| only.clone())
        }
    }
}

pub(super) async fn connect(address: &str, id: Option<Uuid>, config: &mut SynlyConfig, options: &RuntimeOptions) -> Result<AuthenticatedSession> {
    if !options.bluetooth_enabled { return Err(anyhow!(PairingTerminal(anyhow!("蓝牙接入未启用")))); }
    let expected = expected_trusted_device(&config.trusted_devices, id);
    // 仅可信策略下交互授权已被禁止, 先在桌面侧给出可操作的说明, 不再打开一条注定被拒的链路.
    if expected.is_none() && options.pairing.trusted_only {
        bail!("仅允许可信设备, 但蓝牙目标 {address} 没有可对应的已信任身份; 请取消该限制, 或在设置中重新完成一次配对");
    }
    let connection = native::connect(address).await?;
    let auth_config = auth_config(config, options);
    let authenticated = auth::connect(connection, &auth_config, expected.as_ref(), |request| async move {
        if id.is_some_and(|id| id != request.peer.device_id) { bail!("蓝牙身份与目标设备 ID 不一致"); }
        authorize(request, options).await
    }).await.map_err(|error| anyhow!(PairingTerminal(error)))?;
    if id.is_some_and(|id| id != authenticated.remote.device_id) { bail!("蓝牙身份与目标设备 ID 不一致"); }
    established(authenticated, config, SessionRole::Client, SessionCapabilityProfile::Full)
}

pub(crate) async fn accept(connection: native::BluetoothConnection, config: &mut SynlyConfig, options: &RuntimeOptions, reserver: &ActiveSlotReserver, active_peers: &[DeviceIdentity]) -> Result<(AuthenticatedSession, SlotReservation)> {
    let mut auth_config = auth_config(config, options);
    // 当前会话授权仅用于建立候选 TLS, 不写入持久信任. 最终加入还必须通过主会话证明.
    for peer in active_peers {
        auth_config.trusted_devices.retain(|trusted| trusted.device_id != peer.device_id);
        auth_config.trusted_devices.push(TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name.clone(), public_key: peer.identity_public_key.clone(), tls_root_certificate: peer.tls_root_certificate.clone(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 });
    }
    let authenticated = auth::accept(connection, &auth_config, |request| authorize(request, options)).await?;
    let reservation = reserver.reserve(authenticated.remote.device_id);
    let profile = reservation.profile();
    let session = established(authenticated, config, SessionRole::Host, profile)?;
    Ok((session, reservation))
}

pub(super) fn spawn_input(stream: ByteStream, role: LocalInputRole, options: InputRuntimeOptions, master_secret: [u8; 32], activity: Arc<std::sync::atomic::AtomicBool>, epoch: CapabilityEpoch, require_manual: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = input::run_input_session_with_gate(InputSessionContext::Multiplexed { stream }, master_secret, role, options, Some(activity), require_manual).await {
            tracing::warn!(error = %error, ?epoch, "蓝牙输入子流已经停止, 需要重新建立代次并手动激活");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bluetooth_target_is_explicit_and_normalized_without_guessing_lan_identity() {
        let id = Uuid::new_v4();
        let target = parse_target(&format!("bluetooth:11-22-33-44-55-66/{id}")).unwrap().unwrap();
        assert_eq!(target, ("11:22:33:44:55:66".to_owned(), Some(id)));
        assert!(parse_target("192.168.0.2:5000").unwrap().is_none());
        assert!(parse_target("bluetooth:11:22:33:44:55:66").unwrap().unwrap().1.is_none());
        for value in ["bluetooth:foo", "bluetooth:11:22:33:44:55:66/foo", "bluetooth:11:22:33:44:55:66/00000000-0000-0000-0000-000000000000"] { assert!(parse_target(value).is_err()); }
    }
    #[test]
    fn trusted_bluetooth_identity_reuses_the_only_usable_record() {
        fn device(public_key: &str, certificate: &str) -> TrustedDeviceConfig {
            TrustedDeviceConfig { device_id: Uuid::new_v4(), device_name: "peer".to_owned(), public_key: public_key.to_owned(), tls_root_certificate: certificate.to_owned(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
        }
        let only = device("key", "cert");
        // 设备列表按钮只能给出蓝牙地址, 此时必须复用唯一一条可用信任, 否则仅可信策略会拒绝每次连接.
        assert_eq!(expected_trusted_device(std::slice::from_ref(&only), None).map(|device| device.device_id), Some(only.device_id));
        // 多条信任无法只凭地址选择, 不能随便挑一条.
        let second = device("key-b", "cert-b");
        assert!(expected_trusted_device(&[only.clone(), second.clone()], None).is_none());
        // 空公钥或空根证书的记录无法建立可信 mTLS, 不参与复用.
        assert!(expected_trusted_device(&[device("key-c", "")], None).is_none());
        assert!(expected_trusted_device(&[device("", "cert-d")], None).is_none());
        // 显式指定设备 ID 时只按该身份解析, 不受记录条数影响.
        assert_eq!(expected_trusted_device(&[only.clone(), second.clone()], Some(second.device_id)).map(|device| device.device_id), Some(second.device_id));
        assert!(expected_trusted_device(&[only], Some(Uuid::new_v4())).is_none());
    }
}
