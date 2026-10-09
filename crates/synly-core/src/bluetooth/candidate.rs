//! 为已认证的 LAN 主会话寻找蓝牙副承载, 发现线索不能改变固定身份.

use super::{session, BluetoothConnection, BluetoothPeer};
use crate::{crypto, transport::{logical::{BoundLink, LogicalSession, SessionKeys}, routing::TransportKind, stream::ByteStream}};
use anyhow::{Result, bail};
use std::{future::Future, pin::Pin, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};

const RETRY_DELAY: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(target_os = "macos"))]
const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(35);
#[cfg(not(target_os = "macos"))]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(11);
const MAX_PAIRED: usize = 256;

/// 所有者丢弃即取消扫描, 原生连接, TLS 与绑定等待, 不分离后台任务.
pub struct CandidateTask(JoinHandle<()>);
impl Drop for CandidateTask { fn drop(&mut self) { self.0.abort(); } }

type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
trait Backend: Send + Sync + 'static {
    fn paired(&self) -> BackendFuture<'_, Vec<BluetoothPeer>>;
    fn service<'a>(&'a self, peer: &'a BluetoothPeer) -> BackendFuture<'a, bool>;
    fn connect<'a>(&'a self, address: &'a str) -> BackendFuture<'a, BluetoothConnection>;
}
struct SystemBackend;
impl Backend for SystemBackend {
    fn paired(&self) -> BackendFuture<'_, Vec<BluetoothPeer>> { Box::pin(super::paired_devices()) }
    fn service<'a>(&'a self, peer: &'a BluetoothPeer) -> BackendFuture<'a, bool> { Box::pin(async move { Ok(super::query_service(peer).await?.is_some()) }) }
    fn connect<'a>(&'a self, address: &'a str) -> BackendFuture<'a, BluetoothConnection> { Box::pin(super::connect(address)) }
}

pub fn spawn_bluetooth(config: session::AuthConfig, logical: LogicalSession, preferred: Option<String>, input_activity: Option<Arc<AtomicBool>>) -> Result<(mpsc::Receiver<BoundLink>, CandidateTask)> {
    spawn(config, logical, preferred, input_activity, Arc::new(SystemBackend), RETRY_DELAY)
}
fn spawn(mut config: session::AuthConfig, logical: LogicalSession, preferred: Option<String>, input_activity: Option<Arc<AtomicBool>>, backend: Arc<dyn Backend>, retry_delay: Duration) -> Result<(mpsc::Receiver<BoundLink>, CandidateTask)> {
    if logical.primary() != TransportKind::Lan || !logical.is_open() { bail!("蓝牙副承载需要在线 LAN 主会话"); }
    if config.device.device_id != logical.local().device_id || !crypto::public_keys_match(&config.device.identity_public_key, &logical.local().identity_public_key) { bail!("候选签名身份与主会话本机身份不一致"); }
    let preferred = preferred.map(|address| super::normalize_address(&address)).transpose()?;
    // 只允许主会话已经认证的真实身份, 不使用保存的 UUID 候选或新授权路径.
    let peer = logical.peer();
    let expected = crate::device::TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name.clone(), public_key: peer.identity_public_key.clone(), tls_root_certificate: peer.tls_root_certificate.clone(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 };
    config.trusted_devices = vec![expected.clone()]; config.request_trust = false; config.trusted_only = true;
    let (tx, rx) = mpsc::channel(1);
    let shutdown = logical.cancellation();
    let task = tokio::spawn(async move {
        let work = async {
            let mut preferred = preferred;
            let mut first = true;
            loop {
                if !first { tokio::time::sleep(retry_delay).await; }
                first = false;
                if logical.has(TransportKind::Bluetooth) || active(&input_activity) { continue; }
                let peers = match tokio::time::timeout(DISCOVERY_TIMEOUT, backend.paired()).await {
                    Ok(Ok(peers)) if peers.len() <= MAX_PAIRED => peers,
                    Ok(Ok(_)) => { tracing::warn!("系统已配对候选数超过限制, 本次不扫描"); continue; },
                    Ok(Err(error)) => { tracing::debug!(error = %error, "副承载配对列表不可用, 主会话继续运行"); continue; },
                    Err(_) => { tracing::warn!("副承载配对列表读取超时"); continue; },
                };
                let mut peers = peers;
                if let Some(index) = preferred.as_ref().and_then(|address| peers.iter().position(|peer| super::normalize_address(&peer.address).is_ok_and(|value| &value == address))) { peers.swap(0, index); }
                for peer in peers {
                    if active(&input_activity) || logical.has(TransportKind::Bluetooth) { break; }
                    if !matches!(tokio::time::timeout(DISCOVERY_TIMEOUT, backend.service(&peer)).await, Ok(Ok(true))) { continue; }
                    if active(&input_activity) { break; }
                    let attempt = async {
                        let connection = backend.connect(&peer.address).await?;
                        authenticate_candidate(connection, &config, &expected, &logical).await
                    };
                    match tokio::time::timeout(CANDIDATE_TIMEOUT, attempt).await {
                        Ok(Ok(bound)) => {
                            tracing::info!(session = %logical.id(), "蓝牙副承载身份与绑定验证完成");
                            preferred = Some(peer.address);
                            if tx.send(bound).await.is_err() { return; }
                            break;
                        },
                        Ok(Err(error)) => tracing::debug!(address = %peer.address, error = %error, "蓝牙候选不是当前主会话的可用副承载"),
                        Err(_) => tracing::warn!(address = %peer.address, "蓝牙副承载接入超时, 主会话保持不变"),
                    }
                }
            }
        };
        tokio::select! { biased; _ = shutdown.cancelled() => {}, _ = tx.closed() => {}, _ = work => {} }
    });
    Ok((rx, CandidateTask(task)))
}
fn active(input: &Option<Arc<AtomicBool>>) -> bool { input.as_ref().is_some_and(|value| value.load(Ordering::Acquire)) }
async fn authenticate_candidate(connection: BluetoothConnection, config: &session::AuthConfig, expected: &crate::device::TrustedDeviceConfig, logical: &LogicalSession) -> Result<BoundLink> {
    let authenticated = session::connect(connection, config, Some(expected), |_| async { bail!("副承载不能请求新的应用授权") }).await?;
    if !logical.matches(&authenticated.remote) || !authenticated.trusted_reconnect || authenticated.remember_peer { bail!("副承载认证不能改变主会话身份或持久信任"); }
    let keys = SessionKeys::bluetooth(&authenticated.stream, authenticated.session_id, authenticated.link_master_secret)?;
    logical.connect(ByteStream::new(authenticated.stream), &authenticated.remote, TransportKind::Bluetooth, keys.candidate_exporter().value()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{identity, protocol::RuntimeCapabilities, settings::{ClipboardMode, AudioMode}, input::InputMode, device::TrustedDeviceConfig, transport::logical::SecondaryTunnel};
    use std::{collections::VecDeque, sync::{Mutex, atomic::AtomicUsize}};
    use tokio::sync::Notify;
    fn config(name: &str) -> session::AuthConfig {
        session::AuthConfig { device: identity::generate_device_config(name.to_owned()).unwrap(), instance_name: None, capabilities: RuntimeCapabilities { clipboard_mode: ClipboardMode::Both, audio_mode: AudioMode::Off, input_mode: InputMode::Off }, policies: Default::default(), trusted_devices: Vec::new(), request_trust: false, trusted_only: true }
    }
    fn peer(config: &session::AuthConfig) -> crate::protocol::DeviceIdentity {
        crate::protocol::DeviceIdentity { device_id: config.device.device_id, device_name: config.device.device_name.clone(), instance_name: None, identity_public_key: config.device.identity_public_key.clone(), tls_root_certificate: crypto::device_tls_root_certificate(&config.device).unwrap() }
    }
    fn trusted(config: &session::AuthConfig) -> TrustedDeviceConfig {
        let peer = peer(config); TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name, public_key: peer.identity_public_key, tls_root_certificate: peer.tls_root_certificate, trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
    }
    fn logical(client: &session::AuthConfig, host: &session::AuthConfig) -> (LogicalSession, LogicalSession) {
        let id = uuid::Uuid::new_v4();
        (LogicalSession::new(id, peer(client), peer(host), TransportKind::Lan, [5; 32]).unwrap(), LogicalSession::new(id, peer(host), peer(client), TransportKind::Lan, [5; 32]).unwrap())
    }
    fn connections() -> (BluetoothConnection, BluetoothConnection) {
        let hint = BluetoothPeer { address: "11:22:33:44:55:66".to_owned(), name: "名称不能作为身份".to_owned() };
        let (a, b) = tokio::io::duplex(32768);
        // 假安全连接仅验证应用 TLS/绑定与任务生命周期, 不证明无线加密或硬件互通.
        (BluetoothConnection::authenticated(ByteStream::new(a), hint.clone()), BluetoothConnection::authenticated(ByteStream::new(b), hint))
    }
    struct Mock {
        connections: Mutex<VecDeque<BluetoothConnection>>, scans: AtomicUsize,
        scanned: Notify, blocked_service: bool, service_started: Notify, service_cancelled: Arc<Notify>,
    }
    impl Mock {
        fn new(connection: Option<BluetoothConnection>, blocked_service: bool) -> Arc<Self> {
            Arc::new(Self { connections: Mutex::new(connection.into_iter().collect()), scans: AtomicUsize::new(0), scanned: Notify::new(), blocked_service, service_started: Notify::new(), service_cancelled: Arc::new(Notify::new()) })
        }
    }
    struct CancelProbe(Arc<Notify>);
    impl Drop for CancelProbe { fn drop(&mut self) { self.0.notify_one(); } }
    impl Backend for Mock {
        fn paired(&self) -> BackendFuture<'_, Vec<BluetoothPeer>> { Box::pin(async { self.scans.fetch_add(1, Ordering::Relaxed); self.scanned.notify_one(); Ok(vec![BluetoothPeer { address: "11:22:33:44:55:66".to_owned(), name: "相同名称".to_owned() }]) }) }
        fn service<'a>(&'a self, _peer: &'a BluetoothPeer) -> BackendFuture<'a, bool> { Box::pin(async {
            if self.blocked_service { let _probe = CancelProbe(Arc::clone(&self.service_cancelled)); self.service_started.notify_one(); std::future::pending::<()>().await; }
            Ok(true)
        }) }
        fn connect<'a>(&'a self, _address: &'a str) -> BackendFuture<'a, BluetoothConnection> { Box::pin(async { self.connections.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("没有更多测试连接")) }) }
    }
    async fn accept_candidate(connection: BluetoothConnection, mut host: session::AuthConfig, client: session::AuthConfig, logical: LogicalSession) -> Result<SecondaryTunnel> {
        host.trusted_devices = vec![trusted(&client)];
        let authenticated = session::accept(connection, &host, |_| async { panic!("副承载不能重新询问应用授权") }).await?;
        assert!(authenticated.trusted_reconnect && !authenticated.remember_peer);
        let keys = SessionKeys::bluetooth(&authenticated.stream, authenticated.session_id, authenticated.link_master_secret)?;
        let bound = logical.accept(ByteStream::new(authenticated.stream), &authenticated.remote, TransportKind::Bluetooth, keys.candidate_exporter().value()).await?;
        Ok(bound.multiplex())
    }
    #[tokio::test]
    async fn automatic_candidate_reuses_authorized_identity_without_persisting_trust() {
        let client = config("客户端"); let host = config("主机"); let (a, b) = logical(&client, &host);
        let _client_owner = a.owner().unwrap(); let _host_owner = b.owner().unwrap();
        let (x, y) = connections(); let backend = Mock::new(Some(x), false);
        let task = tokio::spawn(accept_candidate(y, host, client.clone(), b.clone()));
        let (mut inbox, worker) = spawn(client.clone(), a.clone(), None, None, backend, Duration::from_secs(30)).unwrap();
        let bound = tokio::time::timeout(Duration::from_secs(2), inbox.recv()).await.unwrap().unwrap();
        let tunnel = bound.multiplex(); let mut remote = task.await.unwrap().unwrap();
        assert!(a.has(TransportKind::Lan) && a.has(TransportKind::Bluetooth));
        assert!(b.has(TransportKind::Lan) && b.has(TransportKind::Bluetooth));
        assert!(client.trusted_devices.is_empty());
        drop(worker); drop(tunnel);
        tokio::time::timeout(Duration::from_secs(1), remote.failed()).await.unwrap(); drop(remote);
        assert!(a.has(TransportKind::Lan) && b.has(TransportKind::Lan));
        assert!(!a.has(TransportKind::Bluetooth) && !b.has(TransportKind::Bluetooth));
    }
    #[tokio::test]
    async fn active_input_defers_scans_and_primary_close_cancels_blocked_service_query() {
        let client = config("客户端"); let host = config("主机"); let (a, _b) = logical(&client, &host); let owner = a.owner().unwrap();
        let activity = Arc::new(AtomicBool::new(true)); let backend = Mock::new(None, true);
        let (_inbox, _worker) = spawn(client, a, None, Some(Arc::clone(&activity)), backend.clone(), Duration::from_millis(5)).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(25), backend.scanned.notified()).await.is_err());
        assert_eq!(backend.scans.load(Ordering::Relaxed), 0);
        activity.store(false, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), backend.service_started.notified()).await.unwrap();
        drop(owner);
        tokio::time::timeout(Duration::from_secs(1), backend.service_cancelled.notified()).await.unwrap();
    }
    #[tokio::test]
    async fn mailbox_or_worker_drop_cancels_query_without_closing_primary() {
        for drop_worker in [false, true] {
            let client = config("客户端"); let host = config("主机"); let (logical, _b) = logical(&client, &host); let _owner = logical.owner().unwrap();
            let backend = Mock::new(None, true);
            let (inbox, worker) = spawn(client, logical.clone(), None, None, backend.clone(), Duration::from_secs(30)).unwrap();
            let mut inbox = Some(inbox); let mut worker = Some(worker);
            tokio::time::timeout(Duration::from_secs(1), backend.service_started.notified()).await.unwrap();
            if drop_worker { drop(worker.take()); } else { drop(inbox.take()); }
            tokio::time::timeout(Duration::from_secs(1), backend.service_cancelled.notified()).await.unwrap();
            assert!(logical.is_open() && logical.has(TransportKind::Lan));
            assert!(!logical.has(TransportKind::Bluetooth));
        }
    }
    #[tokio::test]
    async fn paired_name_and_service_uuid_cannot_replace_pinned_primary_peer() {
        let client = config("客户端"); let primary = config("主机"); let impostor = config("主机");
        let (a, _b) = logical(&client, &primary); let _owner = a.owner().unwrap();
        let (x, y) = connections(); let backend = Mock::new(Some(x), false);
        let mut impostor = impostor; impostor.trusted_devices = vec![trusted(&client)];
        let server = tokio::spawn(async move { session::accept(y, &impostor, |_| async { panic!("候选不能触发授权") }).await });
        let (mut inbox, _worker) = spawn(client, a.clone(), None, None, backend, Duration::from_secs(30)).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap().is_err());
        assert!(inbox.try_recv().is_err());
        assert!(a.has(TransportKind::Lan) && !a.has(TransportKind::Bluetooth));
    }
}
