//! 蓝牙主会话的 LAN 副承载发现, 固定已认证身份且不执行 PIN 或持久信任变更.
use super::{logical::{BoundLink, LogicalSession, SessionKeys}, routing::TransportKind, stream::{ByteStream, AsyncByteStream}};
use crate::{bluetooth::session::AuthConfig, crypto, device::DiscoveryConfig, discovery::DiscoveredPeer, protocol::{ControlMessage, Frame, FrameReader, FrameWriter, PairRequestPayload, PairAuthMethod, PROTOCOL_VERSION, TransferLimits}};
use anyhow::{Result, bail};
use std::{future::Future, pin::Pin, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

const RETRY: Duration = Duration::from_secs(30);
const SCAN: Duration = Duration::from_secs(3);
const ATTEMPT: Duration = Duration::from_secs(20);
const MAX_ENDPOINTS: usize = 32;
/// owner 结束或丢弃 guard 时中断发现, TCP, TLS 和绑定, 不留分离任务.
pub struct CandidateTask(JoinHandle<()>);
impl Drop for CandidateTask { fn drop(&mut self) { self.0.abort(); } }
type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
trait Backend: Send + Sync + 'static {
    fn discover(&self) -> BackendFuture<'_, Vec<DiscoveredPeer>>;
    fn connect(&self, endpoint: std::net::SocketAddrV4) -> BackendFuture<'_, (ByteStream, std::net::SocketAddr)>;
}
struct SystemBackend(DiscoveryConfig);
impl Backend for SystemBackend {
    fn discover(&self) -> BackendFuture<'_, Vec<DiscoveredPeer>> { Box::pin(crate::discovery::browse(SCAN, &self.0)) }
    fn connect(&self, endpoint: std::net::SocketAddrV4) -> BackendFuture<'_, (ByteStream, std::net::SocketAddr)> { Box::pin(async move {
        let stream = tokio::net::TcpStream::connect(endpoint).await?;
        stream.set_nodelay(true)?;
        let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(3)).with_interval(Duration::from_secs(1)).with_retries(3);
        socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive)?;
        let peer = stream.peer_addr()?;
        Ok((ByteStream::new(stream), peer))
    }) }
}
pub fn spawn_lan(config: AuthConfig, logical: LogicalSession, discovery: DiscoveryConfig, limits: TransferLimits, input: Option<Arc<AtomicBool>>) -> Result<(mpsc::Receiver<BoundLink>, CandidateTask)> {
    crate::discovery::validate_config(&discovery)?;
    spawn(config, logical, limits, input, Arc::new(SystemBackend(discovery)), RETRY)
}
fn spawn(config: AuthConfig, logical: LogicalSession, limits: TransferLimits, input: Option<Arc<AtomicBool>>, backend: Arc<dyn Backend>, retry: Duration) -> Result<(mpsc::Receiver<BoundLink>, CandidateTask)> {
    if logical.primary() != TransportKind::Bluetooth || !logical.is_open() { bail!("LAN 副承载需要在线蓝牙主会话"); }
    if config.device.device_id != logical.local().device_id || !crypto::public_keys_match(&config.device.identity_public_key, &logical.local().identity_public_key) { bail!("候选签名身份与主会话本机身份不一致"); }
    let (tx, rx) = mpsc::channel(1); let shutdown = logical.cancellation();
    let task = tokio::spawn(async move {
        let work = async {
            let mut first = true; let mut preferred = None;
            loop {
                if !first { tokio::time::sleep(retry).await; } first = false;
                if logical.has(TransportKind::Lan) || active(&input) { continue; }
                let peers = match tokio::time::timeout(SCAN + Duration::from_secs(2), backend.discover()).await {
                    Ok(Ok(peers)) => peers,
                    Ok(Err(error)) => { tracing::debug!(error = %error, "LAN 候选发现不可用, 蓝牙主会话继续运行"); continue; },
                    Err(_) => { tracing::warn!("LAN 候选发现超时"); continue; },
                };
                let mut endpoints = endpoints(&peers, logical.peer().device_id);
                if let Some(index) = preferred.and_then(|address| endpoints.iter().position(|endpoint| *endpoint == address)) { endpoints.swap(0, index); }
                for endpoint in endpoints {
                    if active(&input) || logical.has(TransportKind::Lan) { break; }
                    let attempt = async {
                        let (socket, peer) = backend.connect(endpoint).await?;
                        let connector = crypto::build_client_connector(&config.device, &logical.peer().tls_root_certificate)?;
                        let stream = connector.connect(crypto::server_name()?, socket).await?;
                        authenticate(stream, &config, &logical, limits).await?.with_lan_peer(peer)
                    };
                    match tokio::time::timeout(ATTEMPT, attempt).await {
                        Ok(Ok(bound)) => {
                            tracing::info!(session = %logical.id(), "LAN 副承载身份与绑定验证完成"); preferred = Some(endpoint);
                            if tx.send(bound).await.is_err() { return; } break;
                        },
                        Ok(Err(error)) => tracing::debug!(%endpoint, error = %error, "LAN 候选无法加入当前蓝牙主会话"),
                        Err(_) => tracing::warn!(%endpoint, "LAN 候选接入超时, 蓝牙主会话保持不变"),
                    }
                }
            }
        };
        tokio::select! { biased; _ = shutdown.cancelled() => {}, _ = tx.closed() => {}, _ = work => {} }
    });
    Ok((rx, CandidateTask(task)))
}
fn active(input: &Option<Arc<AtomicBool>>) -> bool { input.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)) }
fn endpoints(peers: &[DiscoveredPeer], expected: Uuid) -> Vec<std::net::SocketAddrV4> {
    let mut endpoints = Vec::new();
    for peer in peers {
        if Uuid::parse_str(&peer.device_id).ok() != Some(expected) || peer.protocol_version != PROTOCOL_VERSION || peer.port == 0 { continue; }
        for address in &peer.addresses {
            if address.is_unspecified() || address.is_multicast() || address.is_broadcast() { continue; }
            let endpoint = std::net::SocketAddrV4::new(*address, peer.port);
            if !endpoints.contains(&endpoint) { endpoints.push(endpoint); }
            if endpoints.len() >= MAX_ENDPOINTS { return endpoints; }
        }
    }
    endpoints
}
async fn authenticate<T: AsyncByteStream + 'static>(mut stream: tokio_rustls::client::TlsStream<T>, config: &AuthConfig, logical: &LogicalSession, limits: TransferLimits) -> Result<BoundLink> {
    let request_id = Uuid::new_v4().to_string();
    let exporter = crypto::export_keying_material_from_client(&stream, &request_id)?;
    let payload = PairRequestPayload { protocol_version: PROTOCOL_VERSION, client: logical.local().clone(), capabilities: config.capabilities, request_trust: false };
    let proof = crypto::sign_trusted_pair_auth(&exporter, config.device.identity_private_key()?, &request_id, &payload)?;
    // 认证控制帧不允许夹带候选业务体, 发现 ID 也不是可信身份.
    let auth_limits = TransferLimits { max_meta_len: limits.max_meta_len.min(16 * 1024), max_frame_data_len: 0, ..limits };
    FrameWriter::with_limits(&mut stream, auth_limits).write_frame(Frame::Control(ControlMessage::PairRequest { request_id: request_id.clone(), payload, trusted_proof: Some(proof) })).await?;
    let decision = match FrameReader::with_limits(&mut stream, auth_limits).read_frame().await? { Frame::Control(message) => message, _ => bail!("LAN 候选认证收到业务载荷") };
    let remote = match &decision {
        ControlMessage::PairDecision { accepted, server, auth_method, trust_established, .. } => {
            if !accepted || *auth_method != PairAuthMethod::TrustedDevice || *trust_established || !logical.matches(server) { bail!("LAN 候选不能变更主会话身份或持久信任"); }
            crypto::verify_trusted_pair_decision(&decision, &exporter, &request_id, &logical.peer().identity_public_key)?;
            server.clone()
        },
        ControlMessage::Error { message } => bail!("LAN 副承载被拒绝: {message}"),
        _ => bail!("LAN 候选收到非可信身份决定"),
    };
    let keys = SessionKeys::client(&stream, &request_id)?;
    logical.connect(ByteStream::new(stream), &remote, TransportKind::Lan, keys.candidate_exporter().value()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{identity, device::TrustedDeviceConfig, protocol::{DeviceIdentity, RuntimeCapabilities, SessionAgreement}, settings::{ClipboardMode, AudioMode}, input::InputMode};
    use std::{collections::VecDeque, sync::{Mutex, atomic::AtomicUsize}};
    use tokio::sync::Notify;
    fn config(name: &str) -> AuthConfig {
        AuthConfig { device: identity::generate_device_config(name.to_owned()).unwrap(), instance_name: None, capabilities: RuntimeCapabilities { clipboard_mode: ClipboardMode::Both, audio_mode: AudioMode::Off, input_mode: InputMode::Off }, policies: Default::default(), trusted_devices: Vec::new(), request_trust: false, trusted_only: true }
    }
    fn peer(config: &AuthConfig) -> DeviceIdentity {
        DeviceIdentity { device_id: config.device.device_id, device_name: config.device.device_name.clone(), instance_name: None, identity_public_key: config.device.identity_public_key.clone(), tls_root_certificate: crypto::device_tls_root_certificate(&config.device).unwrap() }
    }
    fn trust(config: &AuthConfig) -> TrustedDeviceConfig {
        let peer = peer(config); TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name, public_key: peer.identity_public_key, tls_root_certificate: peer.tls_root_certificate, trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
    }
    fn logical(client: &AuthConfig, server: &AuthConfig) -> (LogicalSession, LogicalSession) {
        let id = Uuid::new_v4();
        (LogicalSession::new(id, peer(client), peer(server), TransportKind::Bluetooth, [4; 32]).unwrap(), LogicalSession::new(id, peer(server), peer(client), TransportKind::Bluetooth, [4; 32]).unwrap())
    }
    fn discovered(id: Uuid) -> DiscoveredPeer {
        DiscoveredPeer { fullname: "候选".to_owned(), device_name: "同名设备".to_owned(), instance_name: None, device_id: id.to_string(), protocol_version: PROTOCOL_VERSION, clipboard_mode: ClipboardMode::Both, audio_mode: AudioMode::Off, input_mode: InputMode::Off, source: crate::discovery::DiscoverySource::Mdns, port: 5050, addresses: vec![std::net::Ipv4Addr::LOCALHOST] }
    }
    struct Mock { peers: Vec<DiscoveredPeer>, sockets: Mutex<VecDeque<ByteStream>>, scans: AtomicUsize, scanned: Notify, block: bool, cancelled: Arc<Notify> }
    impl Mock {
        fn new(id: Uuid, socket: Option<ByteStream>, block: bool) -> Arc<Self> { Arc::new(Self { peers: vec![discovered(id)], sockets: Mutex::new(socket.into_iter().collect()), scans: AtomicUsize::new(0), scanned: Notify::new(), block, cancelled: Arc::new(Notify::new()) }) }
    }
    struct Cancelled(Arc<Notify>);
    impl Drop for Cancelled { fn drop(&mut self) { self.0.notify_one(); } }
    impl Backend for Mock {
        fn discover(&self) -> BackendFuture<'_, Vec<DiscoveredPeer>> { Box::pin(async {
            self.scans.fetch_add(1, Ordering::Relaxed); let _probe = Cancelled(Arc::clone(&self.cancelled)); self.scanned.notify_one();
            if self.block { std::future::pending::<()>().await; } Ok(self.peers.clone())
        }) }
        fn connect(&self, _endpoint: std::net::SocketAddrV4) -> BackendFuture<'_, (ByteStream, std::net::SocketAddr)> { Box::pin(async { self.sockets.lock().unwrap().pop_front().map(|stream| (stream, "127.0.0.1:6060".parse().unwrap())).ok_or_else(|| anyhow::anyhow!("没有可用测试候选")) }) }
    }
    async fn serve(socket: ByteStream, server: &AuthConfig, client: &AuthConfig, logical: &LogicalSession, wrong_identity: bool) -> Result<BoundLink> {
        let mut stream = crypto::build_server_acceptor(&server.device, &[trust(client)])?.accept(socket).await?;
        let (request_id, payload, trusted_proof) = match FrameReader::new(&mut stream).read_frame().await? {
            Frame::Control(ControlMessage::PairRequest { request_id, payload, trusted_proof: Some(proof) }) => (request_id, payload, proof),
            _ => bail!("候选请求没有可信签名"),
        };
        assert!(!payload.request_trust && logical.matches(&payload.client));
        let exporter = crypto::export_keying_material_from_server(&stream, &request_id)?;
        crypto::verify_trusted_pair_auth(&exporter, &client.device.identity_public_key, &request_id, &payload, &trusted_proof)?;
        let mut identity = peer(server); if wrong_identity { identity.device_id = Uuid::new_v4(); }
        let agreement = SessionAgreement { host_to_client: true, client_to_host: true };
        let proof = crypto::sign_trusted_pair_decision(server.device.identity_private_key()?, &exporter, &request_id, true, "候选", &identity, &agreement, &server.capabilities, true, false)?;
        let message = ControlMessage::PairDecision { accepted: true, message: "候选".to_owned(), server: identity, capabilities: server.capabilities, clipboard_agreement: agreement, auth_method: PairAuthMethod::TrustedDevice, server_trusts_client: true, proof, trust_established: false };
        FrameWriter::new(&mut stream).write_frame(Frame::Control(message)).await?;
        let keys = SessionKeys::server(&stream, &request_id)?;
        logical.accept(ByteStream::new(stream), &payload.client, TransportKind::Lan, keys.candidate_exporter().value()).await
    }
    #[tokio::test]
    async fn discovered_lan_reuses_actual_identity_and_preserves_bluetooth_primary() {
        let client = config("client"); let server = config("server"); let (a, b) = logical(&client, &server);
        let _owner_a = a.owner().unwrap(); let _owner_b = b.owner().unwrap();
        let (x, y) = tokio::io::duplex(4096); let backend = Mock::new(server.device.device_id, Some(ByteStream::new(x)), false);
        let host = { let server = server.clone(); let client = client.clone(); let b = b.clone(); tokio::spawn(async move { serve(ByteStream::new(y), &server, &client, &b, false).await }) };
        let (mut inbox, worker) = spawn(client.clone(), a.clone(), TransferLimits::default(), None, backend, RETRY).unwrap();
        let bound = tokio::time::timeout(Duration::from_secs(2), inbox.recv()).await.unwrap().unwrap();
        let remote = host.await.unwrap().unwrap();
        assert!(a.has(TransportKind::Lan) && b.has(TransportKind::Lan)); assert!(client.trusted_devices.is_empty());
        assert_eq!(a.id(), b.id()); assert_eq!(a.primary(), TransportKind::Bluetooth);
        assert_eq!(bound.binding_id(), remote.binding_id()); assert!(!bound.binding_id().is_nil());
        assert_eq!(bound.lan_peer(), Some("127.0.0.1:6060".parse().unwrap()));
        let tunnel = bound.multiplex(); assert_eq!(tunnel.binding_id(), remote.binding_id()); assert_eq!(tunnel.lan_peer(), Some("127.0.0.1:6060".parse().unwrap()));
        drop(worker); drop(tunnel); drop(remote);
        assert!(a.is_open() && b.is_open()); assert!(!a.has(TransportKind::Lan) && !b.has(TransportKind::Lan));
        assert!(a.has(TransportKind::Bluetooth) && b.has(TransportKind::Bluetooth));
    }
    #[tokio::test]
    async fn signed_wrong_uuid_cannot_attach_even_with_expected_tls_key() {
        let client = config("client"); let server = config("server"); let (a, b) = logical(&client, &server);
        let _a = a.owner().unwrap(); let _b = b.owner().unwrap(); let (x, y) = tokio::io::duplex(4096);
        let connector = crypto::build_client_connector(&client.device, &peer(&server).tls_root_certificate).unwrap();
        let client_side = async { let stream = connector.connect(crypto::server_name().unwrap(), ByteStream::new(x)).await.unwrap(); authenticate(stream, &client, &a, TransferLimits::default()).await };
        let (client_result, host_result) = tokio::join!(client_side, serve(ByteStream::new(y), &server, &client, &b, true));
        assert!(client_result.is_err() && host_result.is_err()); assert!(!a.has(TransportKind::Lan) && !b.has(TransportKind::Lan));
        assert!(a.is_open() && b.is_open());
    }
    #[tokio::test]
    async fn input_defers_discovery_and_primary_close_cancels_pending_scan() {
        let client = config("client"); let server = config("server"); let (a, _) = logical(&client, &server); let owner = a.owner().unwrap();
        let active = Arc::new(AtomicBool::new(true)); let backend = Mock::new(server.device.device_id, None, true);
        let (_inbox, _worker) = spawn(client, a, TransferLimits::default(), Some(active.clone()), backend.clone(), Duration::from_millis(5)).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(25), backend.scanned.notified()).await.is_err());
        assert_eq!(backend.scans.load(Ordering::Relaxed), 0); active.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), backend.scanned.notified()).await.unwrap();
        drop(owner); tokio::time::timeout(Duration::from_secs(1), backend.cancelled.notified()).await.unwrap();
    }
    #[tokio::test]
    async fn mailbox_close_and_worker_drop_cancel_scan_without_closing_primary() {
        for close_mailbox in [true, false] {
            let client = config("client"); let server = config("server"); let (a, _) = logical(&client, &server); let _owner = a.owner().unwrap();
            let backend = Mock::new(server.device.device_id, None, true);
            let (inbox, worker) = spawn(client, a.clone(), TransferLimits::default(), None, backend.clone(), RETRY).unwrap();
            let mut inbox = Some(inbox); let mut worker = Some(worker);
            tokio::time::timeout(Duration::from_secs(1), backend.scanned.notified()).await.unwrap();
            if close_mailbox { inbox.take(); } else { worker.take(); }
            tokio::time::timeout(Duration::from_secs(1), backend.cancelled.notified()).await.unwrap();
            assert!(a.is_open() && a.has(TransportKind::Bluetooth)); assert!(!a.has(TransportKind::Lan));
        }
    }

    #[test]
    fn discovery_filters_identity_version_and_invalid_addresses_and_bounds_endpoints() {
        let id = Uuid::new_v4(); let mut peer = discovered(id); peer.addresses = vec![std::net::Ipv4Addr::UNSPECIFIED, std::net::Ipv4Addr::BROADCAST, std::net::Ipv4Addr::new(224, 0, 0, 1), std::net::Ipv4Addr::LOCALHOST, std::net::Ipv4Addr::LOCALHOST];
        assert_eq!(endpoints(&[peer.clone()], id).len(), 1);
        assert!(endpoints(&[peer.clone()], Uuid::new_v4()).is_empty());
        peer.protocol_version += 1; assert!(endpoints(&[peer.clone()], id).is_empty());
        peer.protocol_version = PROTOCOL_VERSION; peer.addresses = (1..100).map(|value| std::net::Ipv4Addr::new(192, 168, 0, value)).collect();
        assert_eq!(endpoints(&[peer], id).len(), MAX_ENDPOINTS);
    }
}
