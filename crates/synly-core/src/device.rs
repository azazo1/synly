use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
    pub identity_private_key: String,
    pub identity_public_key: String,
}

impl DeviceConfig {
    pub fn short_id(&self) -> String {
        self.device_id.to_string().chars().take(8).collect()
    }

    pub fn identity_public_key(&self) -> Result<&str> {
        non_empty_key(&self.identity_public_key, "device identity public key is missing")
    }

    pub fn identity_private_key(&self) -> Result<&str> {
        non_empty_key(&self.identity_private_key, "device identity private key is missing")
    }
}

fn non_empty_key<'a>(value: &'a str, message: &'static str) -> Result<&'a str> {
    (!value.trim().is_empty()).then_some(value).context(message)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustedDeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
    pub public_key: String,
    pub tls_root_certificate: String,
    pub trusted_at_ms: u64,
    pub last_seen_ms: u64,
    pub successful_sessions: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub mdns_enabled: bool,
    pub lnd: Option<LndDiscoveryConfig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LndDiscoveryConfig {
    pub server_url: String,
    pub bearer_token: String,
    pub discovery_domain: Option<String>,
}

impl std::fmt::Debug for LndDiscoveryConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LndDiscoveryConfig")
            .field("server_url", &self.server_url)
            .field("bearer_token", &"<redacted>")
            .field("discovery_domain", &self.discovery_domain)
            .finish()
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mdns_enabled: true,
            lnd: None,
        }
    }
}
