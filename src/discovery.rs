use crate::cli::{AudioMode, ClipboardMode, FileSyncMode};
use crate::input::InputMode;
use crate::config::{DeviceConfig, DiscoveryConfig, LndDiscoveryConfig};
use anyhow::{Context, Result, anyhow, bail};
use if_addrs::{IfAddr, get_if_addrs};
use lnd::{AnnounceHandle, AnnounceSpec, DiscoveryFilter, DiscoveredNode, LndClient};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;

pub const MDNS_SERVICE_TYPE: &str = "_synly._tcp.local.";
pub const LND_SERVICE_TYPE: &str = "_synly._tcp";
const MDNS_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const LND_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const CONTINUOUS_LND_INTERVAL: Duration = Duration::from_secs(5);
const CONTINUOUS_LND_TIMEOUT: Duration = Duration::from_secs(3);
const MDNS_STALE_AFTER: Duration = Duration::from_secs(120);
const LND_STALE_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct Advertisement {
    pub protocol_version: u16,
    pub port: u16,
    pub device: DeviceConfig,
    pub file_sync_mode: FileSyncMode,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input_mode: InputMode,
    pub instance_name: Option<String>,
}

struct MdnsRegistration {
    daemon: ServiceDaemon,
}

impl Drop for MdnsRegistration {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

pub struct DiscoveryRegistration {
    mdns: Option<MdnsRegistration>,
    lnd: Option<AnnounceHandle>,
}

impl DiscoveryRegistration {
    pub async fn stop(self) {
        let Self { mdns, lnd } = self;
        drop(mdns);
        if let Some(handle) = lnd {
            match tokio::time::timeout(LND_STOP_TIMEOUT, handle.stop()).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!(error = %err, "停止 LND 租约续期失败"),
                Err(_) => tracing::warn!("停止 LND 租约续期超时, 已继续退出"),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum DiscoverySource {
    Mdns,
    Lnd,
    MdnsAndLnd,
}

impl DiscoverySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mdns => "mDNS",
            Self::Lnd => "LND",
            Self::MdnsAndLnd => "mDNS+LND",
        }
    }

    fn merge(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::MdnsAndLnd
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct DiscoveredPeer {
    pub fullname: String,
    pub device_name: String,
    pub instance_name: Option<String>,
    pub device_id: String,
    pub protocol_version: u16,
    pub file_sync_mode: FileSyncMode,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input_mode: InputMode,
    pub source: DiscoverySource,
    pub port: u16,
    pub addresses: Vec<Ipv4Addr>,
}

impl DiscoveredPeer {
    pub fn display_name(&self) -> String {
        format_display_name(self.instance_name.as_deref(), &self.device_name)
    }

    pub fn label(&self) -> String {
        let addresses = self
            .addresses
            .iter()
            .map(|addr| format!("{addr}:{}", self.port))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} ({})  文件:{}  剪贴板:{}  音频:{}  输入:{}  来源:{}  {}",
            self.display_name(),
            &self.device_id[..8.min(self.device_id.len())],
            self.file_sync_mode.label(),
            self.clipboard_mode.label(),
            self.audio_mode.label(),
            self.input_mode.label(),
            self.source.label(),
            addresses
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct PeerKey {
    device_id: String,
}

#[derive(Clone)]
struct SeenPeer {
    peer: DiscoveredPeer,
    last_seen: Instant,
}

#[derive(Default)]
struct DiscoveryCache {
    mdns: BTreeMap<String, SeenPeer>,
    lnd: BTreeMap<PeerKey, SeenPeer>,
}

impl DiscoveryCache {
    fn upsert_mdns(&mut self, peer: DiscoveredPeer, now: Instant) {
        self.mdns.retain(|fullname, seen| {
            fullname == &peer.fullname || seen.peer.device_id != peer.device_id
        });
        self.mdns.insert(
            peer.fullname.clone(),
            SeenPeer {
                peer,
                last_seen: now,
            },
        );
    }

    fn remove_mdns(&mut self, fullname: &str) {
        self.mdns.remove(fullname);
    }

    fn update_lnd(&mut self, peers: Vec<DiscoveredPeer>, now: Instant) {
        let mut next = BTreeMap::new();
        for peer in peers {
            let key = PeerKey {
                device_id: peer.device_id.clone(),
            };
            next.insert(
                key,
                SeenPeer {
                    peer,
                    last_seen: now,
                },
            );
        }
        self.lnd = next;
    }

    fn expire(&mut self, now: Instant) -> bool {
        let mdns_len = self.mdns.len();
        let lnd_len = self.lnd.len();
        self.mdns
            .retain(|_, seen| now.duration_since(seen.last_seen) <= MDNS_STALE_AFTER);
        self.lnd
            .retain(|_, seen| now.duration_since(seen.last_seen) <= LND_STALE_AFTER);
        mdns_len != self.mdns.len() || lnd_len != self.lnd.len()
    }

    fn snapshot(&self) -> Vec<DiscoveredPeer> {
        merge_peers(
            self.mdns.values().map(|seen| seen.peer.clone()).collect(),
            self.lnd.values().map(|seen| seen.peer.clone()).collect(),
        )
    }
}

enum ContinuousMdnsEvent {
    Resolved(DiscoveredPeer),
    Removed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalIpv4Interface {
    address: Ipv4Addr,
    netmask: Ipv4Addr,
}

pub struct PeerAddressGroups {
    pub same_subnet: Vec<Ipv4Addr>,
    pub fallback: Vec<Ipv4Addr>,
}

struct LndRegistrationStart {
    handle: AnnounceHandle,
    initial_error: Option<anyhow::Error>,
}

pub async fn advertise(
    advertisement: &Advertisement,
    discovery: &DiscoveryConfig,
) -> Result<DiscoveryRegistration> {
    let mdns_result = discovery
        .mdns_enabled
        .then(|| advertise_mdns(advertisement));
    let Some(lnd_config) = discovery.lnd.as_ref() else {
        return match mdns_result {
            Some(result) => result.map(|mdns| DiscoveryRegistration {
                mdns: Some(mdns),
                lnd: None,
            }),
            None => Ok(DiscoveryRegistration {
                mdns: None,
                lnd: None,
            }),
        };
    };

    let lnd_result = start_lnd_registration(advertisement, lnd_config).await;
    match (mdns_result, lnd_result) {
        (Some(Ok(mdns)), Ok(lnd)) => {
            if let Some(err) = &lnd.initial_error {
                tracing::warn!(error = %err, "LND 初始注册失败, 将保留 mDNS 并在后台重试");
            }
            Ok(DiscoveryRegistration {
                mdns: Some(mdns),
                lnd: Some(lnd.handle),
            })
        }
        (Some(Ok(mdns)), Err(err)) => {
            tracing::warn!(error = %err, "LND 发现后端无法启动, 本次仅使用 mDNS");
            Ok(DiscoveryRegistration {
                mdns: Some(mdns),
                lnd: None,
            })
        }
        (Some(Err(mdns_err)), Ok(lnd)) if lnd.initial_error.is_none() => {
            tracing::warn!(error = %mdns_err, "mDNS 发现后端无法启动, 本次仅使用 LND");
            Ok(DiscoveryRegistration {
                mdns: None,
                lnd: Some(lnd.handle),
            })
        }
        (Some(Err(mdns_err)), Ok(lnd)) => {
            let lnd_err = lnd
                .initial_error
                .as_ref()
                .expect("LND 初始错误已经过分支判断");
            let message = format!(
                "mDNS 与 LND 注册均失败; mDNS: {mdns_err:#}; LND: {lnd_err:#}"
            );
            if let Err(err) = lnd.handle.stop().await {
                tracing::warn!(error = %err, "清理失败的 LND 注册任务失败");
            }
            bail!("{message}")
        }
        (Some(Err(mdns_err)), Err(lnd_err)) => {
            bail!("mDNS 与 LND 注册均失败; mDNS: {mdns_err:#}; LND: {lnd_err:#}")
        }
        (None, Ok(lnd)) if lnd.initial_error.is_none() => Ok(DiscoveryRegistration {
            mdns: None,
            lnd: Some(lnd.handle),
        }),
        (None, Ok(lnd)) => {
            let error = lnd
                .initial_error
                .as_ref()
                .expect("LND 初始错误已经过分支判断");
            let message = format!("LND 注册失败: {error:#}");
            if let Err(stop_error) = lnd.handle.stop().await {
                tracing::warn!(error = %stop_error, "清理失败的 LND 注册任务失败");
            }
            bail!("{message}")
        }
        (None, Err(error)) => Err(error).context("LND 发现后端无法启动"),
    }
}

pub fn validate_config(discovery: &DiscoveryConfig) -> Result<()> {
    if let Some(config) = discovery.lnd.as_ref() {
        normalize_lnd_config(config)?;
    }
    Ok(())
}

pub async fn browse(
    timeout: Duration,
    discovery: &DiscoveryConfig,
) -> Result<Vec<DiscoveredPeer>> {
    let mdns_cancelled = Arc::new(AtomicBool::new(false));
    let _mdns_cancellation = BrowseCancellation(Arc::clone(&mdns_cancelled));
    let mdns_task = discovery.mdns_enabled.then(|| {
        tokio::task::spawn_blocking(move || browse_mdns(timeout, &mdns_cancelled))
    });
    let lnd_config = discovery.lnd.clone();
    let lnd_task = async move {
        match lnd_config.as_ref() {
            Some(config) => Some(browse_lnd(config, timeout).await),
            None => None,
        }
    };
    let (mdns_result, lnd_result) = tokio::join!(async move {
        match mdns_task {
            Some(task) => Some(
                task.await
                    .map_err(|err| anyhow!("mDNS discovery task failed: {err}"))
                    .and_then(|result| result),
            ),
            None => None,
        }
    }, lnd_task);

    match (mdns_result, lnd_result) {
        (Some(mdns_result), Some(lnd_result)) => {
            combine_browse_results(mdns_result, lnd_result)
        }
        (Some(mdns_result), None) => mdns_result,
        (None, Some(lnd_result)) => lnd_result,
        (None, None) => Ok(Vec::new()),
    }
}

pub fn continuous_browse(
    discovery: DiscoveryConfig,
) -> tokio::sync::watch::Receiver<Vec<DiscoveredPeer>> {
    let (updates, receiver) = tokio::sync::watch::channel(Vec::new());
    tokio::spawn(run_continuous_browse(discovery, updates));
    receiver
}

async fn run_continuous_browse(
    discovery: DiscoveryConfig,
    updates: tokio::sync::watch::Sender<Vec<DiscoveredPeer>>,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancellation = BrowseCancellation(Arc::clone(&cancelled));
    let (mdns_tx, mut mdns_rx) = tokio::sync::mpsc::unbounded_channel();
    if discovery.mdns_enabled {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = browse_mdns_events(mdns_tx, &cancelled) {
                tracing::warn!(error = %error, "持续 mDNS 发现已停止");
            }
        });
    }

    let mut cache = DiscoveryCache::default();
    let mut mdns_open = discovery.mdns_enabled;
    let mut lnd_tick = tokio::time::interval(CONTINUOUS_LND_INTERVAL);
    lnd_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut lnd_failures = 0u32;
    let mut expiry_tick = tokio::time::interval(Duration::from_secs(1));
    expiry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if updates.is_closed() {
            break;
        }
        let changed = tokio::select! {
            event = mdns_rx.recv(), if mdns_open => {
                match event {
                    Some(ContinuousMdnsEvent::Resolved(peer)) => {
                        cache.upsert_mdns(peer, Instant::now());
                        true
                    }
                    Some(ContinuousMdnsEvent::Removed(fullname)) => {
                        cache.remove_mdns(&fullname);
                        true
                    }
                    None => {
                        mdns_open = false;
                        false
                    }
                }
            }
            _ = lnd_tick.tick(), if discovery.lnd.is_some() => {
                let config = discovery.lnd.as_ref().expect("LND config checked");
                match browse_lnd(config, CONTINUOUS_LND_TIMEOUT).await {
                    Ok(peers) => {
                        if lnd_failures > 0 {
                            tracing::info!(failed_attempts = lnd_failures, "持续 LND 发现已恢复");
                        }
                        lnd_failures = 0;
                        cache.update_lnd(peers, Instant::now());
                        true
                    }
                    Err(error) => {
                        lnd_failures = lnd_failures.saturating_add(1);
                        if should_report_lnd_failure(lnd_failures) {
                            tracing::warn!(
                                error = %format_args!("{error:#}"),
                                server_url = %config.server_url,
                                failed_attempts = lnd_failures,
                                "持续 LND 发现刷新失败"
                            );
                        }
                        false
                    }
                }
            }
            _ = expiry_tick.tick() => cache.expire(Instant::now()),
        };
        if changed {
            updates.send_replace(cache.snapshot());
        }
    }
}

fn should_report_lnd_failure(failures: u32) -> bool {
    failures == 1 || failures.is_multiple_of(12)
}

fn browse_mdns_events(
    events: tokio::sync::mpsc::UnboundedSender<ContinuousMdnsEvent>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let daemon = ServiceDaemon::new().context("failed to start continuous mDNS daemon")?;
    let receiver = daemon
        .browse(MDNS_SERVICE_TYPE)
        .context("failed to start continuous mDNS browsing")?;

    while !cancelled.load(Ordering::Relaxed) && !events.is_closed() {
        match receiver.recv_timeout(MDNS_CANCEL_POLL_INTERVAL) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(peer) = discovered_peer_from_mdns(&info)
                    && events.send(ContinuousMdnsEvent::Resolved(peer)).is_err()
                {
                    break;
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                if events.send(ContinuousMdnsEvent::Removed(fullname)).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) if receiver.is_disconnected() => break,
            Err(_) => {}
        }
    }

    let _ = daemon.shutdown();
    Ok(())
}

struct BrowseCancellation(Arc<AtomicBool>);

impl Drop for BrowseCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn combine_browse_results(
    mdns_result: Result<Vec<DiscoveredPeer>>,
    lnd_result: Result<Vec<DiscoveredPeer>>,
) -> Result<Vec<DiscoveredPeer>> {
    match lnd_result {
        Ok(lnd_peers) => match mdns_result {
            Ok(mdns_peers) => Ok(merge_peers(mdns_peers, lnd_peers)),
            Err(err) => {
                tracing::warn!(error = %err, "mDNS 搜索失败, 本次使用 LND 结果");
                Ok(merge_peers(Vec::new(), lnd_peers))
            }
        },
        Err(lnd_err) => match mdns_result {
            Ok(mdns_peers) => {
                tracing::warn!(error = %lnd_err, "LND 搜索失败, 本次使用 mDNS 结果");
                Ok(merge_peers(mdns_peers, Vec::new()))
            }
            Err(mdns_err) => {
                bail!("mDNS 与 LND 搜索均失败; mDNS: {mdns_err:#}; LND: {lnd_err:#}")
            }
        },
    }
}

fn advertise_mdns(advertisement: &Advertisement) -> Result<MdnsRegistration> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;
    let addresses = local_ipv4_addresses()?;
    if addresses.is_empty() {
        bail!("no non-loopback IPv4 addresses were found for mDNS advertisement");
    }

    let instance = format!(
        "{}-{}-{}",
        sanitize_label(
            advertisement
                .instance_name
                .as_deref()
                .unwrap_or(&advertisement.device.device_name)
        ),
        advertisement.device.short_id(),
        advertisement.port
    );
    let hostname = format!(
        "synly-{}.local.",
        advertisement.device.device_id.to_string().replace('-', "")
    );
    let properties = advertisement_metadata(advertisement)
        .into_iter()
        .collect::<HashMap<_, _>>();
    let ip_addrs = addresses
        .iter()
        .copied()
        .map(IpAddr::V4)
        .collect::<Vec<IpAddr>>();
    let service_info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance,
        &hostname,
        ip_addrs.as_slice(),
        advertisement.port,
        properties,
    )?
    .enable_addr_auto();

    daemon
        .register(service_info)
        .context("failed to register mDNS service")?;
    Ok(MdnsRegistration { daemon })
}

async fn start_lnd_registration(
    advertisement: &Advertisement,
    config: &LndDiscoveryConfig,
) -> Result<LndRegistrationStart> {
    let client = build_lnd_client(config, None)?;
    let spec = build_lnd_announce_spec(advertisement, config)?;
    let lan_addrs = client
        .resolve_announce_addrs(&spec)
        .context("failed to resolve LND announcement addresses")?;
    if lan_addrs.is_empty() {
        bail!("LND announcement has no eligible LAN addresses");
    }
    let reachability_scopes = client
        .resolve_reachability_scopes(&spec)
        .context("failed to resolve LND reachability scopes")?;
    let handle = client
        .announce_loop(spec.clone())
        .context("failed to start LND lease renewal")?;
    let mut announcement = spec.into_announcement(lan_addrs);
    announcement.reachability_scopes = reachability_scopes;
    let initial_error = client
        .announce_once(announcement)
        .await
        .err()
        .map(anyhow::Error::new);
    Ok(LndRegistrationStart {
        handle,
        initial_error,
    })
}

fn build_lnd_announce_spec(
    advertisement: &Advertisement,
    config: &LndDiscoveryConfig,
) -> Result<AnnounceSpec> {
    let normalized = normalize_lnd_config(config)?;
    let node_id = advertisement.device.device_id.to_string();
    let display_name = format_display_name(
        advertisement.instance_name.as_deref(),
        &advertisement.device.device_name,
    );
    let mut spec = AnnounceSpec::new(
        node_id,
        LND_SERVICE_TYPE,
        display_name,
        advertisement.port,
    )
    .with_metadata(advertisement_metadata(advertisement));
    if let Some(domain) = normalized.discovery_domain {
        spec = spec.with_discovery_domain(domain);
    }
    Ok(spec)
}

fn advertisement_metadata(advertisement: &Advertisement) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::from([
        (
            "device_id".to_string(),
            advertisement.device.device_id.to_string(),
        ),
        (
            "device_name".to_string(),
            advertisement.device.device_name.clone(),
        ),
        (
            "protocol_version".to_string(),
            advertisement.protocol_version.to_string(),
        ),
        (
            "fs_mode".to_string(),
            advertisement.file_sync_mode.as_wire().to_string(),
        ),
        (
            "clipboard_mode".to_string(),
            advertisement.clipboard_mode.as_wire().to_string(),
        ),
        (
            "audio_mode".to_string(),
            advertisement.audio_mode.as_wire().to_string(),
        ),
        (
            "input_mode".to_string(),
            advertisement.input_mode.as_wire().to_string(),
        ),
        ("protocol".to_string(), "1".to_string()),
    ]);
    if let Some(instance_name) = advertisement
        .instance_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        metadata.insert("instance_name".to_string(), instance_name.to_string());
    }
    metadata
}

fn browse_mdns(timeout: Duration, cancelled: &AtomicBool) -> Result<Vec<DiscoveredPeer>> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS browsing daemon")?;
    let receiver = daemon
        .browse(MDNS_SERVICE_TYPE)
        .context("failed to browse mDNS service type")?;
    let deadline = Instant::now() + timeout;
    let mut peers = BTreeMap::<String, DiscoveredPeer>::new();

    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = deadline
            .saturating_duration_since(now)
            .min(MDNS_CANCEL_POLL_INTERVAL);
        match receiver.recv_timeout(wait) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(peer) = discovered_peer_from_mdns(&info) {
                    peers.insert(peer.fullname.clone(), peer);
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                peers.remove(&fullname);
            }
            Ok(_) => {}
            Err(_) if receiver.is_disconnected() => break,
            Err(_) => {}
        }
    }

    let _ = daemon.shutdown();
    Ok(peers.into_values().collect())
}

async fn browse_lnd(config: &LndDiscoveryConfig, timeout: Duration) -> Result<Vec<DiscoveredPeer>> {
    let normalized = normalize_lnd_config(config)?;
    let client = build_lnd_client(&normalized, Some(timeout))?;
    let scopes = client
        .list_reachability_scopes()
        .context("failed to resolve local LND reachability scopes")?
        .into_iter()
        .map(|scope| scope.scope)
        .collect::<Vec<_>>();
    let mut filter = DiscoveryFilter::new().with_service(LND_SERVICE_TYPE);
    if let Some(domain) = normalized.discovery_domain {
        filter = filter.with_discovery_domain(domain);
    }
    if !scopes.is_empty() {
        filter = filter.with_reachability_scopes(scopes);
    }
    let nodes = client.list(filter).await.context("LND list request failed")?;
    Ok(discovered_peers_from_lnd(nodes))
}

fn build_lnd_client(
    config: &LndDiscoveryConfig,
    timeout: Option<Duration>,
) -> Result<LndClient> {
    let normalized = normalize_lnd_config(config)?;
    let mut builder = LndClient::builder(normalized.server_url)
        .bearer_token(normalized.bearer_token);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .context("failed to build LND client")
}

fn normalize_lnd_config(config: &LndDiscoveryConfig) -> Result<LndDiscoveryConfig> {
    let server_url = config.server_url.trim().trim_end_matches('/');
    if server_url.is_empty() {
        bail!("discovery.lnd.server_url cannot be empty");
    }
    let parsed = Url::parse(server_url).context("discovery.lnd.server_url is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("discovery.lnd.server_url must use http or https");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("discovery.lnd.server_url cannot contain a query or fragment");
    }
    if parsed
        .path_segments()
        .and_then(Iterator::last)
        .is_some_and(|segment| segment == "v1")
    {
        bail!("discovery.lnd.server_url must not include the /v1 API suffix");
    }

    Ok(LndDiscoveryConfig {
        server_url: server_url.to_string(),
        bearer_token: config.bearer_token.clone(),
        discovery_domain: config
            .discovery_domain
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    })
}

fn discovered_peer_from_mdns(info: &mdns_sd::ResolvedService) -> Option<DiscoveredPeer> {
    let file_sync_mode = info
        .get_property_val_str("fs_mode")
        .and_then(FileSyncMode::from_wire)?;
    let clipboard_mode = info
        .get_property_val_str("clipboard_mode")
        .and_then(ClipboardMode::from_wire)?;
    let audio_mode = info
        .get_property_val_str("audio_mode")
        .and_then(AudioMode::from_wire)?;
    let input_mode = info
        .get_property_val_str("input_mode")
        .and_then(InputMode::from_wire)?;
    let device_name = info.get_property_val_str("device_name")?.to_string();
    let instance_name = info
        .get_property_val_str("instance_name")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let device_id = info.get_property_val_str("device_id")?.to_string();
    let protocol_version = info
        .get_property_val_str("protocol_version")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let addresses = info.get_addresses_v4().into_iter().collect::<Vec<_>>();
    if addresses.is_empty() {
        return None;
    }

    Some(DiscoveredPeer {
        fullname: info.get_fullname().to_string(),
        device_name,
        instance_name,
        device_id,
        protocol_version,
        file_sync_mode,
        clipboard_mode,
        audio_mode,
        input_mode,
        source: DiscoverySource::Mdns,
        port: info.get_port(),
        addresses,
    })
}

fn discovered_peer_from_lnd(node: &DiscoveredNode) -> Option<DiscoveredPeer> {
    if node.service != LND_SERVICE_TYPE || node.port == 0 {
        return None;
    }
    let metadata = &node.metadata;
    if metadata.get("protocol")?.as_str() != "1" {
        return None;
    }
    let device_id = metadata.get("device_id")?.trim().to_string();
    Uuid::parse_str(&device_id).ok()?;
    let device_name = metadata.get("device_name")?.trim().to_string();
    if device_name.is_empty() {
        return None;
    }
    let protocol_version = metadata
        .get("protocol_version")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let file_sync_mode = metadata
        .get("fs_mode")
        .and_then(|value| FileSyncMode::from_wire(value))?;
    let clipboard_mode = metadata
        .get("clipboard_mode")
        .and_then(|value| ClipboardMode::from_wire(value))?;
    let audio_mode = metadata
        .get("audio_mode")
        .and_then(|value| AudioMode::from_wire(value))?;
    let input_mode = metadata
        .get("input_mode")
        .and_then(|value| InputMode::from_wire(value))?;
    let instance_name = metadata
        .get("instance_name")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let mut addresses = node
        .lan_addrs
        .iter()
        .filter_map(|address| match address.ip() {
            IpAddr::V4(address) => Some(address),
            IpAddr::V6(_) => None,
        })
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return None;
    }

    Some(DiscoveredPeer {
        fullname: format!("lnd:{}", node.node_id),
        device_name,
        instance_name,
        device_id,
        protocol_version,
        file_sync_mode,
        clipboard_mode,
        audio_mode,
        input_mode,
        source: DiscoverySource::Lnd,
        port: node.port,
        addresses,
    })
}

fn discovered_peers_from_lnd(nodes: Vec<DiscoveredNode>) -> Vec<DiscoveredPeer> {
    let mut latest = BTreeMap::<String, (DiscoveredPeer, (u64, u64, u64))>::new();
    for node in nodes {
        let Some(peer) = discovered_peer_from_lnd(&node) else {
            continue;
        };
        let rank = (
            node.lease.last_seen_unix_ms,
            node.lease.expires_at_unix_ms,
            node.lease.revision,
        );
        match latest.get_mut(&peer.device_id) {
            Some((current, current_rank)) if rank > *current_rank => {
                *current = peer;
                *current_rank = rank;
            }
            None => {
                latest.insert(peer.device_id.clone(), (peer, rank));
            }
            _ => {}
        }
    }
    latest.into_values().map(|(peer, _)| peer).collect()
}

fn merge_peers(
    mdns_peers: Vec<DiscoveredPeer>,
    lnd_peers: Vec<DiscoveredPeer>,
) -> Vec<DiscoveredPeer> {
    let mut merged = BTreeMap::<PeerKey, DiscoveredPeer>::new();
    for peer in mdns_peers.into_iter().chain(lnd_peers) {
        let key = PeerKey {
            device_id: peer.device_id.clone(),
        };
        match merged.get_mut(&key) {
            Some(existing) => {
                if existing.port == peer.port {
                    existing.addresses.extend(peer.addresses);
                    existing.addresses.sort();
                    existing.addresses.dedup();
                }
                existing.source = existing.source.merge(peer.source);
            }
            None => {
                merged.insert(key, peer);
            }
        }
    }
    merged.into_values().collect()
}

pub fn group_peer_addresses(addresses: &[Ipv4Addr]) -> Result<PeerAddressGroups> {
    let interfaces = local_ipv4_interfaces()?;
    Ok(group_peer_addresses_for_interfaces(addresses, &interfaces))
}

fn group_peer_addresses_for_interfaces(
    addresses: &[Ipv4Addr],
    interfaces: &[LocalIpv4Interface],
) -> PeerAddressGroups {
    let (mut same_subnet, mut fallback): (Vec<Ipv4Addr>, Vec<Ipv4Addr>) =
        addresses.iter().copied().partition(|address| {
            interfaces
                .iter()
                .any(|interface| same_ipv4_subnet(*address, interface.address, interface.netmask))
        });
    same_subnet.sort();
    same_subnet.dedup();
    fallback.sort();
    fallback.dedup();
    PeerAddressGroups {
        same_subnet,
        fallback,
    }
}

fn same_ipv4_subnet(remote: Ipv4Addr, local: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    u32::from(remote) & u32::from(netmask) == u32::from(local) & u32::from(netmask)
}

fn local_ipv4_addresses() -> Result<Vec<Ipv4Addr>> {
    let interfaces = local_ipv4_interfaces()?;
    let mut addrs = interfaces
        .into_iter()
        .map(|interface| interface.address)
        .collect::<Vec<_>>();
    addrs.sort();
    addrs.dedup();
    Ok(addrs)
}

fn local_ipv4_interfaces() -> Result<Vec<LocalIpv4Interface>> {
    let interfaces = get_if_addrs().context("failed to enumerate local network interfaces")?;
    let mut ipv4_interfaces = Vec::new();
    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }
        if let IfAddr::V4(v4) = interface.addr {
            ipv4_interfaces.push(LocalIpv4Interface {
                address: v4.ip,
                netmask: v4.netmask,
            });
        }
    }
    Ok(ipv4_interfaces)
}

fn sanitize_label(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if sanitized.is_empty() {
        "synly".to_string()
    } else {
        sanitized
    }
}

pub fn format_display_name(instance_name: Option<&str>, device_name: &str) -> String {
    match instance_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(instance_name) if !instance_name.eq_ignore_ascii_case(device_name) => {
            format!("{instance_name} @ {device_name}")
        }
        _ => device_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Advertisement, BrowseCancellation, DiscoveryCache, DiscoverySource, LND_SERVICE_TYPE,
        LND_STALE_AFTER, LocalIpv4Interface,
        build_lnd_announce_spec, combine_browse_results, discovered_peer_from_lnd,
        discovered_peers_from_lnd, group_peer_addresses_for_interfaces, merge_peers,
        normalize_lnd_config, should_report_lnd_failure,
    };
    use crate::cli::{AudioMode, ClipboardMode, FileSyncMode};
    use crate::config::{DeviceConfig, LndDiscoveryConfig};
    use crate::input::InputMode;
    use lnd::{
        DiscoveredNode, DiscoveryFilter, InMemoryRegistry, LeaseInfo, LndClient, ServerConfig,
        build_router,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    #[test]
    fn merge_peers_unions_addresses_for_same_instance() {
        let mut mdns = sample_peer(DiscoverySource::Mdns);
        mdns.fullname = "mdns".to_string();
        let mut lnd = sample_peer(DiscoverySource::Lnd);
        lnd.fullname = "lnd".to_string();
        lnd.addresses = vec![Ipv4Addr::new(10, 0, 0, 8)];

        let peers = merge_peers(vec![mdns], vec![lnd]);

        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].fullname, "mdns");
        assert_eq!(peers[0].addresses.len(), 2);
        assert_eq!(peers[0].source, DiscoverySource::MdnsAndLnd);
    }

    #[test]
    fn continuous_cache_merges_sources_and_expires_stale_lnd() {
        let now = std::time::Instant::now();
        let mut cache = DiscoveryCache::default();
        let mdns = sample_peer(DiscoverySource::Mdns);
        let mut lnd = sample_peer(DiscoverySource::Lnd);
        lnd.addresses = vec![Ipv4Addr::new(10, 0, 0, 8)];

        cache.upsert_mdns(mdns, now);
        cache.update_lnd(vec![lnd], now);
        let merged = cache.snapshot();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, DiscoverySource::MdnsAndLnd);
        assert_eq!(merged[0].addresses.len(), 2);

        assert!(cache.expire(now + LND_STALE_AFTER + std::time::Duration::from_secs(1)));
        let remaining = cache.snapshot();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].source, DiscoverySource::Mdns);
    }

    #[test]
    fn continuous_lnd_failure_reporting_is_rate_limited() {
        assert!(should_report_lnd_failure(1));
        assert!(!should_report_lnd_failure(2));
        assert!(!should_report_lnd_failure(11));
        assert!(should_report_lnd_failure(12));
        assert!(!should_report_lnd_failure(13));
        assert!(should_report_lnd_failure(24));
    }

    #[test]
    fn lnd_discovery_keeps_latest_registration_for_each_device() {
        let mut stale = sample_lnd_node();
        stale.port = 49100;
        stale.lan_addrs[0].set_port(stale.port);
        stale.lease.revision = 10;
        stale.lease.last_seen_unix_ms = 10_000;
        stale.lease.expires_at_unix_ms = 40_000;
        let mut current = stale.clone();
        current.node_id = format!("{}:49200", current.metadata["device_id"]);
        current.port = 49200;
        current.lan_addrs[0].set_port(current.port);
        current.lease.revision = 11;
        current.lease.last_seen_unix_ms = 11_000;
        current.lease.expires_at_unix_ms = 41_000;

        let peers = discovered_peers_from_lnd(vec![current, stale]);

        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].port, 49200);
    }

    #[test]
    fn peer_addresses_are_grouped_by_local_subnet() {
        let addresses = vec![
            Ipv4Addr::new(172, 18, 144, 1),
            Ipv4Addr::new(192, 168, 110, 138),
            Ipv4Addr::new(192, 168, 137, 1),
        ];
        let interfaces = vec![
            LocalIpv4Interface {
                address: Ipv4Addr::new(192, 168, 110, 42),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
            },
            LocalIpv4Interface {
                address: Ipv4Addr::new(172, 16, 0, 2),
                netmask: Ipv4Addr::new(255, 240, 0, 0),
            },
        ];

        let groups = group_peer_addresses_for_interfaces(&addresses, &interfaces);

        assert_eq!(
            groups.same_subnet,
            vec![
                Ipv4Addr::new(172, 18, 144, 1),
                Ipv4Addr::new(192, 168, 110, 138),
            ]
        );
        assert_eq!(
            groups.fallback,
            vec![Ipv4Addr::new(192, 168, 137, 1)]
        );
    }

    #[test]
    fn dropping_browse_cancellation_requests_stop() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = BrowseCancellation(Arc::clone(&cancelled));

        drop(cancellation);

        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn browse_results_degrade_when_one_backend_fails() {
        let mdns_peer = sample_peer(DiscoverySource::Mdns);
        let lnd_peer = sample_peer(DiscoverySource::Lnd);

        let from_mdns = combine_browse_results(
            Ok(vec![mdns_peer.clone()]),
            Err(anyhow::anyhow!("LND unavailable")),
        )
        .unwrap();
        let from_lnd = combine_browse_results(
            Err(anyhow::anyhow!("mDNS unavailable")),
            Ok(vec![lnd_peer.clone()]),
        )
        .unwrap();
        let both_failed = combine_browse_results(
            Err(anyhow::anyhow!("mDNS unavailable")),
            Err(anyhow::anyhow!("LND unavailable")),
        );

        assert_eq!(from_mdns, vec![mdns_peer]);
        assert_eq!(from_lnd, vec![lnd_peer]);
        assert!(both_failed.is_err());
    }

    #[test]
    fn invalid_lnd_nodes_are_filtered() {
        let mut node = sample_lnd_node();
        assert!(discovered_peer_from_lnd(&node).is_some());

        node.metadata
            .insert("protocol".to_string(), "unsupported".to_string());
        assert!(discovered_peer_from_lnd(&node).is_none());

        node = sample_lnd_node();
        node.metadata
            .insert("device_id".to_string(), "not-a-uuid".to_string());
        assert!(discovered_peer_from_lnd(&node).is_none());

        node = sample_lnd_node();
        node.lan_addrs.clear();
        assert!(discovered_peer_from_lnd(&node).is_none());
    }

    #[test]
    fn lnd_base_url_rejects_v1_suffix() {
        let err = normalize_lnd_config(&LndDiscoveryConfig {
            server_url: "https://example.com/lnd/v1".to_string(),
            bearer_token: String::new(),
            discovery_domain: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("/v1"));
    }

    #[tokio::test]
    async fn lnd_announce_and_list_roundtrip() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let server_addr = listener.local_addr().unwrap();
        let app = build_router(
            ServerConfig {
                listen_addr: server_addr,
                bearer_token: "test-token".to_string(),
                ..ServerConfig::default()
            },
            InMemoryRegistry::new(32),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let config = LndDiscoveryConfig {
            server_url: format!("http://{server_addr}"),
            bearer_token: "test-token".to_string(),
            discovery_domain: Some("test-domain".to_string()),
        };
        let advertisement = sample_advertisement();
        let client = LndClient::builder(&config.server_url)
            .bearer_token(&config.bearer_token)
            .build()
            .unwrap();
        let spec = build_lnd_announce_spec(&advertisement, &config).unwrap();
        assert_eq!(spec.node_id, advertisement.device.device_id.to_string());
        let lan_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), advertisement.port);
        let mut announcement = spec.into_announcement(vec![lan_addr]);
        announcement.reachability_scopes = Vec::new();
        client.announce_once(announcement).await.unwrap();

        let nodes = client
            .list(
                DiscoveryFilter::new()
                    .with_discovery_domain("test-domain")
                    .with_service(LND_SERVICE_TYPE),
            )
            .await
            .unwrap();
        let peer = discovered_peer_from_lnd(&nodes[0]).unwrap();

        assert_eq!(peer.device_id, advertisement.device.device_id.to_string());
        assert_eq!(peer.instance_name.as_deref(), Some("worker-a"));
        assert_eq!(peer.file_sync_mode, FileSyncMode::Both);
        assert_eq!(peer.clipboard_mode, ClipboardMode::Receive);
        assert_eq!(peer.audio_mode, AudioMode::Send);
        assert_eq!(peer.input_mode, InputMode::Receive);
        assert_eq!(peer.source, DiscoverySource::Lnd);
        assert_eq!(peer.addresses, vec![Ipv4Addr::LOCALHOST]);

        let _ = shutdown_tx.send(());
        server.await.unwrap();
    }

    fn sample_advertisement() -> Advertisement {
        Advertisement {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            port: 8080,
            device: DeviceConfig {
                device_id: Uuid::new_v4(),
                device_name: "demo-device".to_string(),
                identity_private_key: String::new(),
                identity_public_key: String::new(),
            },
            file_sync_mode: FileSyncMode::Both,
            clipboard_mode: ClipboardMode::Receive,
            audio_mode: AudioMode::Send,
            input_mode: InputMode::Receive,
            instance_name: Some("worker-a".to_string()),
        }
    }

    fn sample_peer(source: DiscoverySource) -> super::DiscoveredPeer {
        super::DiscoveredPeer {
            fullname: String::new(),
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-a".to_string()),
            device_id: Uuid::nil().to_string(),
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            file_sync_mode: FileSyncMode::Both,
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: InputMode::Off,
            source,
            port: 8080,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 20)],
        }
    }

    fn sample_lnd_node() -> DiscoveredNode {
        let advertisement = sample_advertisement();
        DiscoveredNode {
            discovery_domain: Some("test-domain".to_string()),
            node_id: advertisement.device.device_id.to_string(),
            service: LND_SERVICE_TYPE.to_string(),
            display_name: "worker-a @ demo-device".to_string(),
            port: advertisement.port,
            lan_addrs: vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
                advertisement.port,
            )],
            reachability_scopes: vec!["ipv4:192.168.1.0/24".to_string()],
            tags: Vec::new(),
            metadata: super::advertisement_metadata(&advertisement),
            lease: LeaseInfo {
                revision: 1,
                ttl_secs: 30,
                expires_at_unix_ms: 30_000,
                last_seen_unix_ms: 1,
            },
        }
    }
}
