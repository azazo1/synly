//! 系统安全 RFCOMM 上的 Synly 应用身份授权与认证.

use super::{BluetoothConnection, BluetoothPeer};
use crate::{crypto, device::{DeviceConfig, TrustedDeviceConfig}, protocol::{DeviceIdentity, PROTOCOL_VERSION, RuntimeCapabilities, decode_payload, encode_payload}, transport::{binding, routing::ChannelPolicies, stream::ByteStream}};
use anyhow::{Result, bail};
use crypto::bluetooth::Purpose;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{future::Future, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsStream;
use uuid::Uuid;

const AUTH_TIMEOUT: Duration = Duration::from_secs(90);
const WIRE_LIMIT: usize = 16 * 1024;
const PREAMBLE: &[u8; 8] = b"SYNLYBT1";

#[derive(Clone)]
pub struct AuthConfig {
    pub device: DeviceConfig,
    pub instance_name: Option<String>,
    pub capabilities: RuntimeCapabilities,
    pub policies: ChannelPolicies,
    pub trusted_devices: Vec<TrustedDeviceConfig>,
    pub request_trust: bool,
    pub trusted_only: bool,
}

#[derive(Clone, Debug)]
pub struct AuthorizationRequest {
    pub peer: DeviceIdentity,
    pub system_peer: BluetoothPeer,
    pub capabilities: RuntimeCapabilities,
    pub fingerprint: String,
    pub changed_identity: bool,
    pub request_trust: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorizationDecision { pub accepted: bool, pub remember: bool }

/// stream 只在双方应用授权成功后返回. 密钥不进入 Debug 或网络元数据.
pub struct AuthenticatedBluetooth {
    pub stream: TlsStream<ByteStream>,
    pub remote: DeviceIdentity,
    pub capabilities: RuntimeCapabilities,
    pub policies: ChannelPolicies,
    pub system_peer: BluetoothPeer,
    pub session_id: Uuid,
    pub link_master_secret: [u8; 32],
    pub audio_master_secret: [u8; 32],
    pub input_master_secret: [u8; 32],
    pub trusted_reconnect: bool,
    pub remember_peer: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Mode { Trusted, Authorize }
#[derive(Serialize, Deserialize)]
struct Hello { version: u16, request_id: Uuid, mode: Mode, ephemeral_key: Option<String> }
#[derive(Serialize, Deserialize)]
struct Challenge { request_id: Uuid, ephemeral_key: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PeerOffer { identity: DeviceIdentity, capabilities: RuntimeCapabilities, policies: ChannelPolicies, request_trust: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Request { request_id: Uuid, peer: PeerOffer }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Decision { request: Request, server: PeerOffer, accepted: bool, remember: bool }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Confirmation { decision: Decision, accepted: bool, remember: bool }
#[derive(Serialize, Deserialize)]
struct Signed<T> { payload: T, signature: String }

fn offer(config: &AuthConfig) -> Result<PeerOffer> {
    Ok(PeerOffer { identity: DeviceIdentity {
        device_id: config.device.device_id, device_name: config.device.device_name.clone(), instance_name: config.instance_name.clone(),
        identity_public_key: config.device.identity_public_key()?.to_owned(), tls_root_certificate: crypto::device_tls_root_certificate(&config.device)?,
    }, capabilities: config.capabilities, policies: config.policies, request_trust: config.request_trust })
}
fn trusted<'a>(config: &'a AuthConfig, peer: &DeviceIdentity) -> Option<&'a TrustedDeviceConfig> {
    config.trusted_devices.iter().find(|entry| entry.device_id == peer.device_id)
}
fn check_trusted(peer: &DeviceIdentity, expected: &TrustedDeviceConfig) -> Result<()> {
    if peer.device_id != expected.device_id { bail!("蓝牙对端应用设备 ID 与已信任身份不一致"); }
    crypto::verify_device_identity(peer, &expected.public_key)
}

/// 地址目标在握手前无法确定对端设备 ID, 所以只能先接受已保存信任中的任意一条,
/// 握手完成后再核对对端确实是其中的一条, 不凭地址建立新信任.
fn check_trusted_any(peer: &DeviceIdentity, candidates: &[TrustedDeviceConfig]) -> Result<()> {
    let matched = candidates.iter().any(|candidate| {
        candidate.device_id == peer.device_id
            && crypto::verify_device_identity(peer, &candidate.public_key).is_ok()
    });
    if !matched { bail!("蓝牙对端应用身份不在本机已保存的信任中"); }
    Ok(())
}

/// 客户端对蓝牙对端长期身份的预期.
/// 蓝牙发现只能得到系统地址, 拿不到对端应用设备 ID, 因此除了"已确定具体身份"还有"任一已保存信任".
#[derive(Clone, Copy)]
pub enum TrustedExpectation<'a> {
    /// 没有可复用的信任, 需要交互授权.
    Interactive,
    /// 已经确定是这一条信任.
    One(&'a TrustedDeviceConfig),
    /// 只知道系统地址: 先按任一条已保存信任完成 mTLS, 再核对对端身份.
    AnyOf(&'a [TrustedDeviceConfig]),
}

impl TrustedExpectation<'_> {
    fn is_trusted(self) -> bool { !matches!(self, Self::Interactive) }
}
fn prompt(config: &AuthConfig, peer: &PeerOffer, system_peer: &BluetoothPeer) -> Result<AuthorizationRequest> {
    Ok(AuthorizationRequest {
        peer: peer.identity.clone(), system_peer: system_peer.clone(), capabilities: peer.capabilities,
        fingerprint: crypto::short_identity_fingerprint(&peer.identity.identity_public_key)?,
        changed_identity: trusted(config, &peer.identity).is_some_and(|known| !crypto::public_keys_match(&known.public_key, &peer.identity.identity_public_key)),
        request_trust: peer.request_trust,
    })
}

async fn write<W: AsyncWrite + Unpin, T: Serialize>(stream: &mut W, value: &T) -> Result<()> {
    let bytes = encode_payload(value)?;
    if bytes.len() > WIRE_LIMIT { bail!("蓝牙授权消息过大"); }
    stream.write_u16(bytes.len() as u16).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(stream: &mut R) -> Result<T> {
    let size = stream.read_u16().await? as usize;
    if size == 0 || size > WIRE_LIMIT { bail!("蓝牙授权消息大小无效"); }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    decode_payload(&bytes, "蓝牙授权消息")
}
fn signed<T: Serialize>(payload: T, purpose: Purpose, config: &AuthConfig, exporter: &[u8; 32], id: Uuid) -> Result<Signed<T>> {
    let signature = crypto::bluetooth::sign(purpose, &config.device, exporter, &id.to_string(), &payload)?;
    Ok(Signed { payload, signature })
}

pub async fn connect<F, Fut>(connection: BluetoothConnection, config: &AuthConfig, expected: TrustedExpectation<'_>, authorize: F) -> Result<AuthenticatedBluetooth>
where F: FnOnce(AuthorizationRequest) -> Fut, Fut: Future<Output = Result<AuthorizationDecision>> {
    let started = std::time::Instant::now();
    tracing::info!(trusted = expected.is_trusted(), "开始蓝牙应用身份认证");
    let result = tokio::time::timeout(AUTH_TIMEOUT, connect_inner(connection, config, expected, authorize)).await.map_err(|_| anyhow::anyhow!("蓝牙应用授权超时"))?;
    match &result {
        Ok(_) => tracing::info!(elapsed_ms = started.elapsed().as_millis(), "蓝牙客户端授权完成"),
        Err(error) => tracing::warn!(elapsed_ms = started.elapsed().as_millis(), error = %error, "蓝牙客户端授权失败"),
    }
    result
}

async fn connect_inner<F, Fut>(mut connection: BluetoothConnection, config: &AuthConfig, expected: TrustedExpectation<'_>, authorize: F) -> Result<AuthenticatedBluetooth>
where F: FnOnce(AuthorizationRequest) -> Fut, Fut: Future<Output = Result<AuthorizationDecision>> {
    let id = Uuid::new_v4();
    let trusted = expected.is_trusted();
    let mode = if trusted { Mode::Trusted } else { Mode::Authorize };
    if config.trusted_only && !trusted { bail!("当前策略只允许已信任应用身份"); }
    let key = (mode == Mode::Authorize).then(crypto::generate_bootstrap_key_material).transpose()?;
    let hello = Hello { version: PROTOCOL_VERSION, request_id: id, mode, ephemeral_key: key.as_ref().map(crypto::BootstrapKeyMaterial::public_key_encoded) };
    connection.stream.write_all(PREAMBLE).await?;
    write(&mut connection.stream, &hello).await?;
    let connector = match expected {
        TrustedExpectation::One(device) => crypto::build_client_connector(&config.device, &device.tls_root_certificate)?,
        TrustedExpectation::AnyOf(devices) => crypto::build_client_connector_for_trusted_devices(&config.device, devices)?,
        TrustedExpectation::Interactive => {
            let challenge: Challenge = read(&mut connection.stream).await?;
            if challenge.request_id != id { bail!("蓝牙临时 TLS 挑战标识不匹配"); }
            crypto::bluetooth::client_connector(&connection, &id.to_string(), key.ok_or_else(|| anyhow::anyhow!("缺少临时 TLS 密钥"))?, &challenge.ephemeral_key)?
        }
    };
    let (stream, system_peer) = connection.into_parts();
    let mut stream = connector.connect(crypto::server_name()?, stream).await?;
    let exporter = crypto::export_keying_material_from_client(&stream, &id.to_string())?;
    let local = Request { request_id: id, peer: offer(config)? };
    write(&mut stream, &signed(local.clone(), Purpose::Request, config, &exporter, id)?).await?;
    let reply: Signed<Decision> = read(&mut stream).await?;
    if reply.payload.request != local { bail!("蓝牙授权决定未绑定本机身份请求"); }
    crypto::bluetooth::verify(Purpose::Decision, &reply.payload.server.identity, &exporter, &id.to_string(), &reply.payload, &reply.signature)?;
    if !reply.payload.accepted { bail!("对端拒绝了 Synly 蓝牙应用授权"); }
    let decision = match expected {
        TrustedExpectation::One(device) => {
            check_trusted(&reply.payload.server.identity, device)?;
            AuthorizationDecision { accepted: true, remember: false }
        }
        TrustedExpectation::AnyOf(devices) => {
            check_trusted_any(&reply.payload.server.identity, devices)?;
            AuthorizationDecision { accepted: true, remember: false }
        }
        TrustedExpectation::Interactive => authorize(prompt(config, &reply.payload.server, &system_peer)?).await?,
    };
    let confirmation = Confirmation { decision: reply.payload, accepted: decision.accepted, remember: decision.accepted && decision.remember };
    write(&mut stream, &signed(confirmation.clone(), Purpose::Confirmation, config, &exporter, id)?).await?;
    if !decision.accepted { bail!("本机拒绝了 Synly 蓝牙应用授权"); }
    let ready: Signed<Confirmation> = read(&mut stream).await?;
    if ready.payload != confirmation { bail!("蓝牙授权最终确认不匹配"); }
    crypto::bluetooth::verify(Purpose::Ready, &confirmation.decision.server.identity, &exporter, &id.to_string(), &ready.payload, &ready.signature)?;
    let link_master_secret = binding::export_link_master_from_client(&stream, id)?;
    let audio_master_secret = crypto::export_audio_master_secret_from_client(&stream, &id.to_string())?;
    let input_master_secret = crypto::export_input_master_secret_from_client(&stream, &id.to_string())?;
    tracing::info!(peer = %confirmation.decision.server.identity.device_id, "蓝牙应用身份授权完成");
    Ok(AuthenticatedBluetooth { stream: stream.into(), remote: confirmation.decision.server.identity, capabilities: confirmation.decision.server.capabilities, policies: confirmation.decision.server.policies,
        system_peer, session_id: id, link_master_secret, audio_master_secret, input_master_secret, trusted_reconnect: trusted, remember_peer: confirmation.remember })
}

pub async fn accept<F, Fut>(connection: BluetoothConnection, config: &AuthConfig, authorize: F) -> Result<AuthenticatedBluetooth>
where F: FnOnce(AuthorizationRequest) -> Fut, Fut: Future<Output = Result<AuthorizationDecision>> {
    let started = std::time::Instant::now();
    tracing::info!("等待蓝牙应用身份请求");
    let result = tokio::time::timeout(AUTH_TIMEOUT, accept_inner(connection, config, authorize)).await.map_err(|_| anyhow::anyhow!("蓝牙应用授权超时"))?;
    match &result {
        Ok(_) => tracing::info!(elapsed_ms = started.elapsed().as_millis(), "蓝牙服务端授权完成"),
        Err(error) => tracing::warn!(elapsed_ms = started.elapsed().as_millis(), error = %error, "蓝牙服务端授权失败"),
    }
    result
}

async fn accept_inner<F, Fut>(mut connection: BluetoothConnection, config: &AuthConfig, authorize: F) -> Result<AuthenticatedBluetooth>
where F: FnOnce(AuthorizationRequest) -> Fut, Fut: Future<Output = Result<AuthorizationDecision>> {
    let mut magic = [0; 8]; connection.stream.read_exact(&mut magic).await?;
    if &magic != PREAMBLE { bail!("非 Synly 蓝牙协议连接"); }
    let hello: Hello = read(&mut connection.stream).await?;
    if hello.version != PROTOCOL_VERSION || hello.request_id.is_nil() { bail!("蓝牙授权协议版本或请求 ID 无效"); }
    let id = hello.request_id;
    let acceptor = match hello.mode {
        Mode::Trusted => {
            if hello.ephemeral_key.is_some() { bail!("可信蓝牙 TLS 不能携带临时密钥"); }
            crypto::build_server_acceptor(&config.device, &config.trusted_devices)?
        }
        Mode::Authorize => {
            if config.trusted_only { bail!("当前策略只允许已信任应用身份"); }
            let public = hello.ephemeral_key.ok_or_else(|| anyhow::anyhow!("缺少蓝牙临时 TLS 密钥"))?;
            let key = crypto::generate_bootstrap_key_material()?;
            write(&mut connection.stream, &Challenge { request_id: id, ephemeral_key: key.public_key_encoded() }).await?;
            crypto::bluetooth::server_acceptor(&connection, &id.to_string(), key, &public)?
        }
    };
    let (stream, system_peer) = connection.into_parts();
    let mut stream = acceptor.accept(stream).await?;
    let exporter = crypto::export_keying_material_from_server(&stream, &id.to_string())?;
    let request: Signed<Request> = read(&mut stream).await?;
    if request.payload.request_id != id { bail!("蓝牙身份请求标识不匹配"); }
    crypto::bluetooth::verify(Purpose::Request, &request.payload.peer.identity, &exporter, &id.to_string(), &request.payload, &request.signature)?;
    let decision = if hello.mode == Mode::Trusted {
        let expected = trusted(config, &request.payload.peer.identity).ok_or_else(|| anyhow::anyhow!("蓝牙应用身份未处于可信状态"))?;
        check_trusted(&request.payload.peer.identity, expected)?;
        AuthorizationDecision { accepted: true, remember: false }
    } else { authorize(prompt(config, &request.payload.peer, &system_peer)?).await? };
    let payload = Decision { request: request.payload, server: offer(config)?, accepted: decision.accepted, remember: decision.accepted && decision.remember };
    write(&mut stream, &signed(payload.clone(), Purpose::Decision, config, &exporter, id)?).await?;
    if !decision.accepted { bail!("本机拒绝了 Synly 蓝牙应用授权"); }
    let confirmation: Signed<Confirmation> = read(&mut stream).await?;
    if confirmation.payload.decision != payload { bail!("蓝牙授权确认未绑定本机决定"); }
    crypto::bluetooth::verify(Purpose::Confirmation, &payload.request.peer.identity, &exporter, &id.to_string(), &confirmation.payload, &confirmation.signature)?;
    if !confirmation.payload.accepted { bail!("对端拒绝了 Synly 蓝牙应用授权"); }
    write(&mut stream, &signed(confirmation.payload.clone(), Purpose::Ready, config, &exporter, id)?).await?;
    let link_master_secret = binding::export_link_master_from_server(&stream, id)?;
    let audio_master_secret = crypto::export_audio_master_secret_from_server(&stream, &id.to_string())?;
    let input_master_secret = crypto::export_input_master_secret_from_server(&stream, &id.to_string())?;
    tracing::info!(peer = %payload.request.peer.identity.device_id, "蓝牙应用身份授权完成");
    Ok(AuthenticatedBluetooth { stream: stream.into(), remote: payload.request.peer.identity, capabilities: payload.request.peer.capabilities, policies: payload.request.peer.policies,
        system_peer, session_id: id, link_master_secret, audio_master_secret, input_master_secret, trusted_reconnect: hello.mode == Mode::Trusted, remember_peer: payload.remember })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{identity, input::InputMode, settings::{AudioMode, ClipboardMode}};
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    fn config(name: &str) -> AuthConfig {
        AuthConfig { device: identity::generate_device_config(name.to_owned()).unwrap(), instance_name: None,
            capabilities: RuntimeCapabilities { clipboard_mode: ClipboardMode::Both, audio_mode: AudioMode::Off, input_mode: InputMode::Off },
            policies: ChannelPolicies::default(), trusted_devices: Vec::new(), request_trust: true, trusted_only: false }
    }
    fn connections() -> (BluetoothConnection, BluetoothConnection) {
        let (client, server) = tokio::io::duplex(32 * 1024);
        let peer = BluetoothPeer { address: "11:22:33:44:55:66".to_owned(), name: "系统发现线索".to_owned() };
        // 假连接只测试应用协议, 不表示实际无线加密或互通已经验证.
        (BluetoothConnection::authenticated(ByteStream::new(client), peer.clone()), BluetoothConnection::authenticated(ByteStream::new(server), peer))
    }
    fn trust(config: &AuthConfig) -> TrustedDeviceConfig {
        TrustedDeviceConfig { device_id: config.device.device_id, device_name: config.device.device_name.clone(), public_key: config.device.identity_public_key.clone(),
            tls_root_certificate: crypto::device_tls_root_certificate(&config.device).unwrap(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
    }

    #[tokio::test]
    async fn first_authorization_requires_both_decisions_and_binds_exported_secrets() {
        let client_config = config("客户端"); let server_config = config("服务端");
        let client_id = client_config.device.device_id; let server_id = server_config.device.device_id;
        let (client, server) = connections();
        let prompts = Arc::new(AtomicUsize::new(0)); let observed = prompts.clone(); let final_prompts = prompts.clone();
        let host = tokio::spawn(async move { accept(server, &server_config, |request| async move {
            assert_eq!(request.peer.device_id, client_id); assert!(!request.fingerprint.is_empty());
            observed.fetch_add(1, Ordering::AcqRel);
            Ok(AuthorizationDecision { accepted: true, remember: true })
        }).await.unwrap() });
        let mut client = connect(client, &client_config, TrustedExpectation::Interactive, |request| async move {
            assert_eq!(request.peer.device_id, server_id); assert!(!request.changed_identity);
            prompts.fetch_add(1, Ordering::AcqRel);
            Ok(AuthorizationDecision { accepted: true, remember: true })
        }).await.unwrap();
        let mut server = host.await.unwrap();
        assert_eq!(final_prompts.load(Ordering::Acquire), 2);
        assert_eq!(client.session_id, server.session_id);
        assert_eq!(client.link_master_secret, server.link_master_secret);
        assert_eq!(client.input_master_secret, server.input_master_secret);
        assert_ne!(client.link_master_secret, client.input_master_secret);
        assert_eq!(client.audio_master_secret, server.audio_master_secret);
        assert!(client.remember_peer && server.remember_peer);
        assert!(!client.trusted_reconnect && !server.trusted_reconnect);
        client.stream.write_all(b"clipboard").await.unwrap(); client.stream.flush().await.unwrap();
        let mut bytes = [0; 9]; server.stream.read_exact(&mut bytes).await.unwrap(); assert_eq!(&bytes, b"clipboard");
        // 应用 TLS 之后接入真实业务帧复用, 假连接仍不验证无线链路或原生输入注入.
        let (client_control, mut client_channels) = crate::transport::bluetooth::open(ByteStream::new(client.stream));
        let (server_control, mut server_channels) = crate::transport::bluetooth::open(ByteStream::new(server.stream));
        let limits = crate::protocol::TransferLimits::default();
        let (client_tx, mut client_inbox, _client_io) = crate::transport::frames::open(client_control, client_channels.clipboard.take(), limits);
        let (server_tx, mut server_inbox, _server_io) = crate::transport::frames::open(server_control, server_channels.clipboard.take(), limits);
        let payload = crate::protocol::ClipboardPayload { text: Some("蓝牙业务帧".to_owned()), rich_text: None, html: None, image: None, files: vec![crate::protocol::ClipboardFile { name: "binary.bin".to_owned(), bytes: vec![0x39; 256 * 1024] }] };
        client_tx.send(crate::protocol::Frame::Clipboard(payload.clone())).await.unwrap();
        server_tx.send(crate::protocol::Frame::Control(crate::protocol::ControlMessage::CapabilitiesAck { generation: 7 })).await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(3), server_inbox.recv()).await.unwrap().unwrap().unwrap();
        assert!(matches!(received, crate::protocol::Frame::Clipboard(received) if received == payload));
        assert!(matches!(client_inbox.recv().await.unwrap().unwrap(), crate::protocol::Frame::Control(crate::protocol::ControlMessage::CapabilitiesAck { generation: 7 })));
        let generation = Uuid::new_v4();
        let mut client_input = client_channels.input.lease(generation).unwrap();
        let mut server_input = server_channels.input.lease(generation).unwrap();
        client_input.write_all(&[1, 2, 3]).await.unwrap();
        let mut motion = [0; 3]; server_input.read_exact(&mut motion).await.unwrap(); assert_eq!(motion, [1, 2, 3]);
    }

    #[tokio::test]
    async fn client_rejection_never_returns_an_authorized_server_session() {
        let client_config = config("客户端"); let server_config = config("服务端");
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { Ok(AuthorizationDecision { accepted: true, remember: true }) }).await });
        assert!(connect(client, &client_config, TrustedExpectation::Interactive, |_| async { Ok(AuthorizationDecision::default()) }).await.is_err());
        assert!(host.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn server_rejection_does_not_prompt_the_client() {
        let client_config = config("客户端"); let server_config = config("服务端");
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { Ok(AuthorizationDecision::default()) }).await });
        assert!(connect(client, &client_config, TrustedExpectation::Interactive, |_| async { panic!("服务端拒绝时不能询问客户端授权") }).await.is_err());
        assert!(host.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn trusted_only_rejects_first_authorization_and_oversized_preamble_is_bounded() {
        let client_config = config("客户端"); let mut server_config = config("服务端"); server_config.trusted_only = true;
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { panic!("仅信任策略不能询问未知身份") }).await });
        assert!(connect(client, &client_config, TrustedExpectation::Interactive, |_| async { panic!("仅信任策略不能询问未知身份") }).await.is_err());
        assert!(host.await.unwrap().is_err());
        let server_config = config("服务端"); let (mut client, server) = connections();
        client.stream.write_all(PREAMBLE).await.unwrap(); client.stream.write_u16((WIRE_LIMIT + 1) as u16).await.unwrap();
        assert!(accept(server, &server_config, |_| async { panic!("过大消息不能进入授权") }).await.is_err());
    }

    #[tokio::test]
    async fn trusted_reconnect_uses_long_term_mtls_without_authorization_prompt() {
        let mut client_config = config("客户端"); let mut server_config = config("服务端");
        let expected = trust(&server_config);
        server_config.trusted_devices.push(trust(&client_config)); client_config.trusted_devices.push(expected.clone());
        client_config.trusted_only = true; server_config.trusted_only = true;
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { panic!("可信连接不能重新询问授权") }).await.unwrap() });
        let client = connect(client, &client_config, TrustedExpectation::One(&expected), |_| async { panic!("可信连接不能重新询问授权") }).await.unwrap();
        let server = host.await.unwrap();
        assert_eq!(client.session_id, server.session_id);
        assert_eq!(client.remote.device_id, expected.device_id);
        assert!(!client.remember_peer && !server.remember_peer);
        assert!(client.trusted_reconnect && server.trusted_reconnect);
    }

    #[tokio::test]
    async fn address_only_target_reuses_any_saved_trust_and_verifies_the_peer() {
        // 蓝牙发现只能拿到系统地址, 因此保存多条信任时客户端用 AnyOf, 由完成 mTLS 的对端自证身份.
        let mut client_config = config("客户端"); let mut server_config = config("服务端");
        let server_trust = trust(&server_config);
        let stranger = trust(&config("无关设备"));
        server_config.trusted_devices.push(trust(&client_config));
        client_config.trusted_devices.push(server_trust.clone());
        client_config.trusted_devices.push(stranger);
        client_config.trusted_only = true; server_config.trusted_only = true;
        let candidates = client_config.trusted_devices.clone();
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { panic!("可信连接不能重新询问授权") }).await.unwrap() });
        let client = connect(client, &client_config, TrustedExpectation::AnyOf(&candidates), |_| async { panic!("可信连接不能重新询问授权") }).await.unwrap();
        let server = host.await.unwrap();
        assert_eq!(client.session_id, server.session_id);
        assert_eq!(client.remote.device_id, server_trust.device_id);
        assert!(client.trusted_reconnect && !client.remember_peer);

        // 对端身份不在已保存信任中时必须拒绝, 不能因为"接受任一条根证书"就放行.
        let mut unknown_client = config("冒名设备");
        unknown_client.trusted_devices.push(trust(&server_config));
        let candidates = unknown_client.trusted_devices.clone();
        let (client, server) = connections();
        let host = tokio::spawn(async move { accept(server, &server_config, |_| async { Ok(AuthorizationDecision { accepted: true, remember: false }) }).await });
        assert!(connect(client, &unknown_client, TrustedExpectation::AnyOf(&candidates), |_| async { panic!("可信连接不能重新询问授权") }).await.is_err());
        assert!(host.await.unwrap().is_err());
    }

    #[test]
    fn signatures_reject_changed_payload_exporter_and_cross_purpose_replay() {
        let config = config("测试设备"); let peer = offer(&config).unwrap(); let id = Uuid::new_v4(); let exporter = [1; 32];
        let request = Request { request_id: id, peer: peer.clone() };
        let signed = signed(request.clone(), Purpose::Request, &config, &exporter, id).unwrap();
        crypto::bluetooth::verify(Purpose::Request, &peer.identity, &exporter, &id.to_string(), &request, &signed.signature).unwrap();
        let mut changed = request.clone(); changed.peer.request_trust = false;
        assert!(crypto::bluetooth::verify(Purpose::Request, &peer.identity, &exporter, &id.to_string(), &changed, &signed.signature).is_err());
        assert!(crypto::bluetooth::verify(Purpose::Request, &peer.identity, &[2; 32], &id.to_string(), &request, &signed.signature).is_err());
        assert!(crypto::bluetooth::verify(Purpose::Ready, &peer.identity, &exporter, &id.to_string(), &request, &signed.signature).is_err());
        let mut changed_config = config.clone(); changed_config.device = identity::generate_device_config("不同身份".to_owned()).unwrap(); changed_config.device.device_id = peer.identity.device_id;
        assert!(prompt(&AuthConfig { trusted_devices: vec![trust(&config)], ..config }, &offer(&changed_config).unwrap(), &BluetoothPeer { address: "11:22:33:44:55:66".to_owned(), name: "发现线索".to_owned() }).unwrap().changed_identity);
    }
}
