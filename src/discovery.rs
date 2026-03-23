use crate::cli::SyncMode;
use crate::config::DeviceConfig;
use anyhow::{Context, Result, bail};
use if_addrs::get_if_addrs;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_synly._tcp.local.";

#[derive(Clone, Debug)]
pub struct Advertisement {
    pub port: u16,
    pub device: DeviceConfig,
    pub mode: SyncMode,
    pub process_name: Option<String>,
}

pub struct DiscoveryRegistration {
    daemon: ServiceDaemon,
}

impl Drop for DiscoveryRegistration {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct DiscoveredPeer {
    pub fullname: String,
    pub device_name: String,
    pub process_name: Option<String>,
    pub device_id: String,
    pub mode: SyncMode,
    pub port: u16,
    pub addresses: Vec<Ipv4Addr>,
}

impl DiscoveredPeer {
    pub fn display_name(&self) -> String {
        format_display_name(self.process_name.as_deref(), &self.device_name)
    }

    pub fn label(&self) -> String {
        let addresses = self
            .addresses
            .iter()
            .map(|addr| format!("{addr}:{}", self.port))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} ({})  {}  {}",
            self.display_name(),
            &self.device_id[..8.min(self.device_id.len())],
            self.mode.label(),
            addresses
        )
    }
}

pub fn advertise(advertisement: &Advertisement) -> Result<DiscoveryRegistration> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;
    let addresses = local_ipv4_addresses()?;
    if addresses.is_empty() {
        bail!("no non-loopback IPv4 addresses were found for mDNS advertisement");
    }

    let instance = format!(
        "{}-{}-{}",
        sanitize_label(
            advertisement
                .process_name
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

    let mut properties = HashMap::new();
    properties.insert(
        "device_id".to_string(),
        advertisement.device.device_id.to_string(),
    );
    properties.insert(
        "device_name".to_string(),
        advertisement.device.device_name.clone(),
    );
    if let Some(process_name) = advertisement
        .process_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        properties.insert("process_name".to_string(), process_name.to_string());
    }
    properties.insert("mode".to_string(), advertisement.mode.as_wire().to_string());
    properties.insert("protocol".to_string(), "1".to_string());

    let ip_addrs = addresses
        .iter()
        .copied()
        .map(IpAddr::V4)
        .collect::<Vec<IpAddr>>();

    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
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

    Ok(DiscoveryRegistration { daemon })
}

pub fn browse(timeout: Duration) -> Result<Vec<DiscoveredPeer>> {
    let daemon = ServiceDaemon::new().context("failed to start mDNS browsing daemon")?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("failed to browse mDNS service type")?;

    let deadline = Instant::now() + timeout;
    let mut peers = BTreeMap::<String, DiscoveredPeer>::new();

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = deadline.saturating_duration_since(now);
        match receiver.recv_timeout(wait) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(peer) = discovered_peer_from_info(&info) {
                    peers.insert(peer.fullname.clone(), peer);
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                peers.remove(&fullname);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();
    Ok(peers.into_values().collect())
}

fn discovered_peer_from_info(info: &mdns_sd::ResolvedService) -> Option<DiscoveredPeer> {
    let mode = SyncMode::from_wire(info.get_property_val_str("mode")?)?;
    let device_name = info.get_property_val_str("device_name")?.to_string();
    let process_name = info
        .get_property_val_str("process_name")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let device_id = info.get_property_val_str("device_id")?.to_string();
    let addresses = info.get_addresses_v4().into_iter().collect::<Vec<_>>();

    if addresses.is_empty() {
        return None;
    }

    Some(DiscoveredPeer {
        fullname: info.get_fullname().to_string(),
        device_name,
        process_name,
        device_id,
        mode,
        port: info.get_port(),
        addresses,
    })
}

fn local_ipv4_addresses() -> Result<Vec<Ipv4Addr>> {
    let interfaces = get_if_addrs().context("failed to enumerate local network interfaces")?;
    let mut addrs = Vec::new();
    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }
        let ip = interface.ip();
        if let IpAddr::V4(v4) = ip {
            addrs.push(v4);
        }
    }
    addrs.sort();
    addrs.dedup();
    Ok(addrs)
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

pub fn format_display_name(process_name: Option<&str>, device_name: &str) -> String {
    match process_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(process_name) if !process_name.eq_ignore_ascii_case(device_name) => {
            format!("{process_name} @ {device_name}")
        }
        _ => device_name.to_string(),
    }
}
