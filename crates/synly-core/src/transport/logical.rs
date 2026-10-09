//! 逻辑会话的物理连接绑定握手与生命周期, 不使用地址或名称作为身份.

use super::{binding::{self, AttachedLink, LinkChallenge, LinkProof, SessionLinks}, routing::TransportKind, stream::ByteStream};
use crate::{crypto, protocol::DeviceIdentity};
use anyhow::{Context, Result, bail};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"SYNLYLK1";
const MAX_PACKET: usize = 1024;
const BIND_TIMEOUT: Duration = Duration::from_secs(10);

/// 在擦除 TLS 类型前提取专用 exporter. 不实现 Debug, 不向 UI 暴露密钥.
pub struct CandidateExporter([u8; 32]);
impl Drop for CandidateExporter { fn drop(&mut self) { self.0.fill(0); } }
impl CandidateExporter { pub fn value(&self) -> [u8; 32] { self.0 } }
pub struct SessionKeys { id: Uuid, master: [u8; 32], candidate: [u8; 32] }
impl Drop for SessionKeys { fn drop(&mut self) { self.master.fill(0); self.candidate.fill(0); } }
impl SessionKeys {
    pub fn client<T>(stream: &tokio_rustls::client::TlsStream<T>, request_id: &str) -> Result<Self> {
        let id = Uuid::parse_str(request_id).context("应用会话请求 ID 必须是 UUID")?;
        Ok(Self { id, master: binding::export_link_master_from_client(stream, id)?, candidate: binding::export_candidate_from_client(stream, id)? })
    }
    pub fn server<T>(stream: &tokio_rustls::server::TlsStream<T>, request_id: &str) -> Result<Self> {
        let id = Uuid::parse_str(request_id).context("应用会话请求 ID 必须是 UUID")?;
        Ok(Self { id, master: binding::export_link_master_from_server(stream, id)?, candidate: binding::export_candidate_from_server(stream, id)? })
    }
    pub fn bluetooth<T>(stream: &tokio_rustls::TlsStream<T>, id: Uuid, master: [u8; 32]) -> Result<Self> {
        let candidate = match stream { tokio_rustls::TlsStream::Client(stream) => binding::export_candidate_from_client(stream, id)?, tokio_rustls::TlsStream::Server(stream) => binding::export_candidate_from_server(stream, id)? };
        Ok(Self { id, master, candidate })
    }
    pub fn logical(&self, local: DeviceIdentity, remote: DeviceIdentity, primary: TransportKind) -> Result<LogicalSession> {
        LogicalSession::new(self.id, local, remote, primary, self.master)
    }
    pub fn candidate_exporter(&self) -> CandidateExporter { CandidateExporter(self.candidate) }
}
struct State { links: SessionLinks, primary: AttachedLink, owner_claimed: bool }
#[derive(Clone)]
pub struct LogicalSession { state: Arc<Mutex<State>>, local: DeviceIdentity, remote: DeviceIdentity, shutdown: CancellationToken }
impl LogicalSession {
    pub fn new(id: Uuid, local: DeviceIdentity, remote: DeviceIdentity, primary: TransportKind, secret: [u8; 32]) -> Result<Self> {
        crypto::verify_device_identity_material(&local)?;
        if local.device_id == remote.device_id { bail!("不能将本机身份作为远端逻辑会话"); }
        let (links, primary) = SessionLinks::new(id, remote.clone(), primary, secret)?;
        Ok(Self { state: Arc::new(Mutex::new(State { links, primary, owner_claimed: false })), local, remote, shutdown: CancellationToken::new() })
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> { self.state.lock().unwrap_or_else(|error| error.into_inner()) }
    pub fn id(&self) -> Uuid { self.state().links.session_id() }
    pub fn primary(&self) -> TransportKind { self.state().links.primary() }
    pub fn is_open(&self) -> bool { !self.shutdown.is_cancelled() && !self.state().links.is_closed() }
    pub fn has(&self, transport: TransportKind) -> bool { self.state().links.contains(transport) }
    pub fn cancellation(&self) -> CancellationToken { self.shutdown.clone() }
    pub fn local(&self) -> &DeviceIdentity { &self.local }
    pub fn peer(&self) -> &DeviceIdentity { &self.remote }
    pub fn matches(&self, peer: &DeviceIdentity) -> bool { peer.device_id == self.remote.device_id && crypto::public_keys_match(&peer.identity_public_key, &self.remote.identity_public_key) && crypto::verify_device_identity_material(peer).is_ok() }
    pub fn owner(&self) -> Result<LogicalOwner> {
        let mut state = self.state();
        if state.owner_claimed || state.links.is_closed() { bail!("逻辑会话主所有者已认领或关闭"); }
        state.owner_claimed = true;
        Ok(LogicalOwner(self.clone()))
    }
    pub fn close(&self) { self.state().links.close(); self.shutdown.cancel(); }
    fn detach(&self, link: AttachedLink) { let mut state = self.state(); if state.links.detach(link) && state.links.is_closed() { self.shutdown.cancel(); } }
    fn validate(&self, peer: &DeviceIdentity, transport: TransportKind) -> Result<()> {
        if self.shutdown.is_cancelled() || !self.matches(peer) { bail!("候选连接不属于当前已授权设备身份"); }
        if self.has(transport) { bail!("当前逻辑会话已经包含此物理传输"); }
        Ok(())
    }
    /// 只接受完成独立应用 TLS 身份认证的原始候选流, exporter 必须在该 TLS 上本机提取.
    pub async fn accept(&self, stream: ByteStream, peer: &DeviceIdentity, transport: TransportKind, exporter: [u8; 32]) -> Result<BoundLink> {
        self.validate(peer, transport)?;
        self.run(self.accept_inner(stream, peer, transport, exporter)).await
    }
    pub async fn connect(&self, stream: ByteStream, peer: &DeviceIdentity, transport: TransportKind, exporter: [u8; 32]) -> Result<BoundLink> {
        self.validate(peer, transport)?;
        self.run(self.connect_inner(stream, peer, transport, exporter)).await
    }
    async fn run(&self, work: impl Future<Output = Result<BoundLink>>) -> Result<BoundLink> {
        tokio::select! { biased; _ = self.shutdown.cancelled() => bail!("主逻辑会话已结束"), result = tokio::time::timeout(BIND_TIMEOUT, work) => result.context("候选物理连接绑定超时")? }
    }
    async fn accept_inner(&self, mut stream: ByteStream, peer: &DeviceIdentity, transport: TransportKind, exporter: [u8; 32]) -> Result<BoundLink> {
        let challenge = self.state().links.challenge(transport, exporter, Instant::now())?;
        let mut pending = PendingGuard { session: self.clone(), challenge, exporter, attached: None };
        stream.write_all(MAGIC).await?;
        write(&mut stream, &Packet::Challenge(challenge)).await?;
        let Packet::Proof(proof) = read(&mut stream).await? else { bail!("候选连接缺少客户端会话证明"); };
        let ticket = self.state().links.attach(proof, peer, transport, &exporter, Instant::now())?;
        pending.attached = Some(ticket);
        let proof = self.state().links.local_proof(challenge, &self.local, &exporter, true)?;
        write(&mut stream, &Packet::Ready(proof)).await?;
        let Packet::Commit(nonce) = read(&mut stream).await? else { bail!("候选连接缺少最终提交"); };
        if nonce != challenge.nonce { bail!("候选提交不匹配当前 nonce"); }
        if self.shutdown.is_cancelled() { bail!("主逻辑会话已经结束"); }
        pending.attached = None;
        Ok(BoundLink { stream, binding_id: challenge.nonce, lan_peer: None, guard: BoundLinkGuard { session: self.clone(), ticket } })
    }
    async fn connect_inner(&self, mut stream: ByteStream, peer: &DeviceIdentity, transport: TransportKind, exporter: [u8; 32]) -> Result<BoundLink> {
        let mut magic = [0; 8]; stream.read_exact(&mut magic).await?;
        if &magic != MAGIC { bail!("候选连接不是逻辑会话绑定协议"); }
        let Packet::Challenge(challenge) = read(&mut stream).await? else { bail!("候选连接缺少主会话挑战"); };
        self.state().links.adopt(challenge, transport, exporter, Instant::now())?;
        let mut pending = PendingGuard { session: self.clone(), challenge, exporter, attached: None };
        let proof = self.state().links.local_proof(challenge, &self.local, &exporter, false)?;
        write(&mut stream, &Packet::Proof(proof)).await?;
        let Packet::Ready(proof) = read(&mut stream).await? else { bail!("候选连接缺少服务端会话证明"); };
        let ticket = self.state().links.attach_role(proof, peer, transport, &exporter, Instant::now(), true)?;
        pending.attached = Some(ticket);
        write(&mut stream, &Packet::Commit(challenge.nonce)).await?;
        if self.shutdown.is_cancelled() { bail!("主逻辑会话已经结束"); }
        pending.attached = None;
        Ok(BoundLink { stream, binding_id: challenge.nonce, lan_peer: None, guard: BoundLinkGuard { session: self.clone(), ticket } })
    }
}
/// 只能由主业务会话持有, 主会话结束时撤销所有副链路和未完成候选.
pub struct LogicalOwner(LogicalSession);
impl Drop for LogicalOwner { fn drop(&mut self) { let primary = self.0.state().primary; self.0.detach(primary); self.0.close(); } }
pub struct BoundLink { pub stream: ByteStream, pub guard: BoundLinkGuard, binding_id: Uuid, lan_peer: Option<std::net::SocketAddr> }
/// 副承载不接受业务控制消息, 主控制路径在逻辑会话期间保持稳定.
pub struct SecondaryTunnel {
    pub channels: super::bluetooth::BluetoothChannels,
    control: ByteStream,
    guard: BoundLinkGuard,
    binding_id: Uuid,
    lan_peer: Option<std::net::SocketAddr>,
}
impl BoundLink {
    /// 只从本机 TCP socket 元数据填入地址, 不接受发现广播或远端控制帧提供的 IP.
    pub fn with_lan_peer(mut self, peer: std::net::SocketAddr) -> Result<Self> {
        if self.guard.transport() != TransportKind::Lan || peer.ip().is_unspecified() || peer.ip().is_multicast() || peer.port() == 0 { bail!("LAN 副承载缺少有效的实际 TCP 对端"); }
        self.lan_peer = Some(peer); Ok(self)
    }
    pub fn binding_id(&self) -> Uuid { self.binding_id }
    pub fn lan_peer(&self) -> Option<std::net::SocketAddr> { self.lan_peer }
    pub fn multiplex(self) -> SecondaryTunnel {
        let (control, channels) = super::bluetooth::open(self.stream);
        SecondaryTunnel { channels, control, guard: self.guard, binding_id: self.binding_id, lan_peer: self.lan_peer }
    }
}
impl SecondaryTunnel {
    pub fn binding_id(&self) -> Uuid { self.binding_id }
    pub fn lan_peer(&self) -> Option<std::net::SocketAddr> { self.lan_peer }
    pub fn transport(&self) -> TransportKind { self.guard.transport() }
    pub async fn failed(&mut self) -> String {
        tokio::select! {
            error = self.channels.failed() => error,
            result = self.control.read_u8() => match result {
                Ok(_) => "副承载不能接管业务控制通道".to_owned(),
                Err(error) => format!("副承载控制已关闭: {error}"),
            },
        }
    }
}
pub async fn receive_secondary(inbox: &mut Option<tokio::sync::mpsc::Receiver<BoundLink>>) -> Option<BoundLink> {
    match inbox { Some(inbox) => inbox.recv().await, None => std::future::pending().await }
}
pub async fn secondary_failure(tunnel: &mut Option<SecondaryTunnel>) -> String {
    match tunnel { Some(tunnel) => tunnel.failed().await, None => std::future::pending().await }
}
pub struct BoundLinkGuard { session: LogicalSession, ticket: AttachedLink }
impl BoundLinkGuard { pub fn transport(&self) -> TransportKind { self.ticket.transport() } }
impl Drop for BoundLinkGuard { fn drop(&mut self) { self.session.detach(self.ticket); } }
struct PendingGuard { session: LogicalSession, challenge: LinkChallenge, exporter: [u8; 32], attached: Option<AttachedLink> }
impl Drop for PendingGuard {
    fn drop(&mut self) { self.session.state().links.cancel_challenge(self.challenge, &self.exporter); self.exporter.fill(0); if let Some(link) = self.attached { self.session.detach(link); } }
}
#[derive(Serialize, Deserialize)]
enum Packet { Challenge(LinkChallenge), Proof(LinkProof), Ready(LinkProof), Commit(Uuid) }
async fn write(stream: &mut (impl AsyncWrite + Unpin), packet: &Packet) -> Result<()> {
    let bytes = bincode::serialize(packet)?;
    if bytes.len() > MAX_PACKET { bail!("候选绑定消息超过限制"); }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
async fn read(stream: &mut (impl AsyncRead + Unpin)) -> Result<Packet> {
    let size = stream.read_u32().await? as usize;
    if size == 0 || size > MAX_PACKET { bail!("候选绑定消息长度无效"); }
    let mut bytes = vec![0; size]; stream.read_exact(&mut bytes).await?;
    bincode::DefaultOptions::new().with_fixint_encoding().reject_trailing_bytes().with_limit(MAX_PACKET as u64).deserialize(&bytes).context("候选绑定消息格式无效")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{device::{DeviceConfig, TrustedDeviceConfig}, identity};
    fn peer(device: &DeviceConfig) -> DeviceIdentity {
        DeviceIdentity { device_id: device.device_id, device_name: device.device_name.clone(), instance_name: None, identity_public_key: device.identity_public_key().unwrap().to_owned(), tls_root_certificate: crypto::device_tls_root_certificate(device).unwrap() }
    }
    fn trusted(device: &DeviceConfig) -> TrustedDeviceConfig {
        TrustedDeviceConfig { device_id: device.device_id, device_name: device.device_name.clone(), public_key: device.identity_public_key().unwrap().to_owned(), tls_root_certificate: crypto::device_tls_root_certificate(device).unwrap(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
    }
    fn devices() -> (DeviceConfig, DeviceConfig) {
        (identity::generate_device_config("客户端".to_owned()).unwrap(), identity::generate_device_config("主机".to_owned()).unwrap())
    }
    fn logical(client: &DeviceConfig, server: &DeviceConfig) -> (LogicalSession, LogicalSession) {
        let id = Uuid::new_v4();
        (LogicalSession::new(id, peer(client), peer(server), TransportKind::Lan, [4; 32]).unwrap(), LogicalSession::new(id, peer(server), peer(client), TransportKind::Lan, [4; 32]).unwrap())
    }
    async fn tls(client: &DeviceConfig, server: &DeviceConfig) -> (tokio_rustls::client::TlsStream<ByteStream>, tokio_rustls::server::TlsStream<ByteStream>) {
        let connector = crypto::build_client_connector(client, &trusted(server).tls_root_certificate).unwrap();
        let acceptor = crypto::build_server_acceptor(server, &[trusted(client)]).unwrap();
        let (a, b) = tokio::io::duplex(4096);
        let (a, b) = tokio::join!(connector.connect(crypto::server_name().unwrap(), ByteStream::new(a)), acceptor.accept(ByteStream::new(b)));
        (a.unwrap(), b.unwrap())
    }
    #[tokio::test]
    async fn two_real_tls_links_bind_to_one_session_and_preserve_primary() {
        let (client, server) = devices();
        let (primary_client, primary_server) = tls(&client, &server).await;
        let id = Uuid::new_v4().to_string();
        let client_keys = SessionKeys::client(&primary_client, &id).unwrap();
        let server_keys = SessionKeys::server(&primary_server, &id).unwrap();
        assert_eq!(client_keys.master, server_keys.master);
        let client_logical = client_keys.logical(peer(&client), peer(&server), TransportKind::Lan).unwrap();
        let server_logical = server_keys.logical(peer(&server), peer(&client), TransportKind::Lan).unwrap();
        let _client_owner = client_logical.owner().unwrap(); let server_owner = server_logical.owner().unwrap();
        assert!(client_logical.owner().is_err());
        let (candidate_client, candidate_server) = tls(&client, &server).await;
        let id = Uuid::new_v4().to_string();
        let client_exporter = SessionKeys::client(&candidate_client, &id).unwrap().candidate_exporter();
        let server_exporter = SessionKeys::server(&candidate_server, &id).unwrap().candidate_exporter();
        assert_eq!(client_exporter.value(), server_exporter.value());
        assert_ne!(client_exporter.value(), client_keys.candidate_exporter().value());
        let client_peer = peer(&client); let server_peer = peer(&server);
        let (a, b) = tokio::join!(client_logical.connect(ByteStream::new(candidate_client), &server_peer, TransportKind::Bluetooth, client_exporter.value()), server_logical.accept(ByteStream::new(candidate_server), &client_peer, TransportKind::Bluetooth, server_exporter.value()));
        let (mut a, mut b) = (a.unwrap(), b.unwrap());
        assert!(client_logical.has(TransportKind::Lan) && client_logical.has(TransportKind::Bluetooth));
        assert!(server_logical.has(TransportKind::Lan) && server_logical.has(TransportKind::Bluetooth));
        a.stream.write_all(&[1, 2, 3]).await.unwrap(); let mut bytes = [0; 3]; b.stream.read_exact(&mut bytes).await.unwrap(); assert_eq!(bytes, [1, 2, 3]);
        drop(a); drop(b);
        assert!(client_logical.has(TransportKind::Lan) && !client_logical.has(TransportKind::Bluetooth));
        drop(server_owner); assert!(!server_logical.is_open());
        assert!(!server_logical.has(TransportKind::Lan));
    }
    #[tokio::test]
    async fn different_candidate_exporters_cannot_join_and_do_not_close_primary() {
        let (client, server) = devices(); let (a, b) = logical(&client, &server);
        let (x, y) = tokio::io::duplex(2048);
        let client_peer = peer(&client); let server_peer = peer(&server);
        let (x, y) = tokio::join!(a.connect(ByteStream::new(x), &server_peer, TransportKind::Bluetooth, [1; 32]), b.accept(ByteStream::new(y), &client_peer, TransportKind::Bluetooth, [2; 32]));
        assert!(x.is_err() && y.is_err()); assert!(!a.has(TransportKind::Bluetooth) && !b.has(TransportKind::Bluetooth));
        assert!(a.has(TransportKind::Lan) && b.has(TransportKind::Lan));
    }
    #[tokio::test]
    async fn lost_final_commit_rolls_back_host_slot() {
        let (client, server) = devices(); let (a, b) = logical(&client, &server);
        let (mut x, y) = tokio::io::duplex(2048); let host = b.clone(); let client_peer = peer(&client);
        let task = tokio::spawn(async move { host.accept(ByteStream::new(y), &client_peer, TransportKind::Bluetooth, [1; 32]).await });
        let mut magic = [0; 8]; x.read_exact(&mut magic).await.unwrap(); assert_eq!(&magic, MAGIC);
        let Packet::Challenge(challenge) = read(&mut x).await.unwrap() else { panic!("挑战类型错误") };
        let proof = a.state().links.local_proof(challenge, &peer(&client), &[1; 32], false).unwrap();
        write(&mut x, &Packet::Proof(proof)).await.unwrap(); assert!(matches!(read(&mut x).await.unwrap(), Packet::Ready(_)));
        assert!(b.has(TransportKind::Bluetooth)); drop(x);
        assert!(task.await.unwrap().is_err());
        assert!(!b.has(TransportKind::Bluetooth)); assert!(b.has(TransportKind::Lan));
    }
    #[tokio::test]
    async fn primary_owner_close_interrupts_pending_bind() {
        let (client, server) = devices(); let (_a, b) = logical(&client, &server); let owner = b.owner().unwrap();
        let (mut x, y) = tokio::io::duplex(2048); let host = b.clone(); let client_peer = peer(&client);
        let task = tokio::spawn(async move { host.accept(ByteStream::new(y), &client_peer, TransportKind::Bluetooth, [1; 32]).await });
        let mut magic = [0; 8]; x.read_exact(&mut magic).await.unwrap(); assert!(matches!(read(&mut x).await.unwrap(), Packet::Challenge(_)));
        drop(owner);
        assert!(tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap().is_err());
        assert!(!b.is_open() && !b.has(TransportKind::Bluetooth));
    }
    #[tokio::test]
    async fn server_proof_cannot_use_client_purpose() {
        let (client, server) = devices(); let (a, b) = logical(&client, &server);
        let (x, mut y) = tokio::io::duplex(2048);
        let forged = async {
            let challenge = b.state().links.challenge(TransportKind::Bluetooth, [1; 32], Instant::now()).unwrap();
            y.write_all(MAGIC).await.unwrap(); write(&mut y, &Packet::Challenge(challenge)).await.unwrap();
            assert!(matches!(read(&mut y).await.unwrap(), Packet::Proof(_)));
            let proof = b.state().links.local_proof(challenge, &peer(&server), &[1; 32], false).unwrap();
            write(&mut y, &Packet::Ready(proof)).await.unwrap();
        };
        let server_peer = peer(&server);
        let (result, ()) = tokio::join!(a.connect(ByteStream::new(x), &server_peer, TransportKind::Bluetooth, [1; 32]), forged);
        assert!(result.is_err() && !a.has(TransportKind::Bluetooth));
    }
}
