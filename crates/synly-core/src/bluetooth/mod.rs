//! 系统已配对设备的经典蓝牙 RFCOMM 接入.

use crate::transport::stream::ByteStream;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;
pub mod candidate;
pub mod provider;
pub mod session;
#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
use self::android as platform;
#[cfg(not(any(target_os = "macos", windows, target_os = "android")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows, target_os = "android")))]
use unsupported as platform;

/// 三端使用同一个项目专用 UUID, 不借用 HID, 串口或音频的系统服务 UUID.
pub const SERVICE_UUID: &str = "b392883b-e85b-4c92-b1a0-2cdfe0a7b6d4";
pub const SERVICE_UUID_BYTES: [u8; 16] = [
    0xb3, 0x92, 0x88, 0x3b, 0xe8, 0x5b, 0x4c, 0x92,
    0xb1, 0xa0, 0x2c, 0xdf, 0xe0, 0xa7, 0xb6, 0xd4,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothPeer {
    /// 仅作为发现和重连线索, 不用于确认 Synly 应用身份.
    pub address: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BluetoothEndpoint {
    pub peer: BluetoothPeer,
    /// 支持显式通道的平台返回通道号, UUID socket 平台在连接时由系统解析.
    pub channel: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BluetoothAvailability {
    Available,
    Disabled,
    PermissionDenied,
    Unsupported,
}

/// 原生后端只能在系统配对和链路加密校验成功后构造连接.
#[derive(Debug)]
pub struct BluetoothConnection {
    stream: ByteStream,
    peer: BluetoothPeer,
}

impl BluetoothConnection {
    #[cfg(any(target_os = "macos", windows, target_os = "android", test))]
    fn authenticated(stream: ByteStream, peer: BluetoothPeer) -> Self {
        Self { stream, peer }
    }

    pub fn peer(&self) -> &BluetoothPeer {
        &self.peer
    }

    pub fn into_parts(self) -> (ByteStream, BluetoothPeer) {
        (self.stream, self.peer)
    }
}

pub struct BluetoothListener(platform::Listener);

impl BluetoothListener {
    pub async fn accept(&mut self) -> Result<BluetoothConnection> {
        self.0.accept().await
    }
}

#[cfg(target_os = "android")]
pub(crate) fn register_provider(provider: std::sync::Arc<dyn provider::Provider>) -> Result<()> {
    android::register(provider)
}

pub async fn availability() -> Result<BluetoothAvailability> {
    platform::availability().await
}

pub async fn paired_devices() -> Result<Vec<BluetoothPeer>> {
    platform::paired_devices().await
}

pub async fn query_service(peer: &BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    let address = normalize_address(&peer.address)?;
    platform::query_service(BluetoothPeer { address, name: peer.name.clone() }).await
}

pub async fn connect(address: &str) -> Result<BluetoothConnection> {
    platform::connect(normalize_address(address)?).await
}

pub async fn listen() -> Result<BluetoothListener> {
    platform::listen().await.map(BluetoothListener)
}

pub fn normalize_address(address: &str) -> Result<String> {
    let separator = match address.as_bytes().get(2) {
        Some(b':') => ':',
        Some(b'-') => '-',
        _ => bail!("蓝牙地址必须由 6 组十六进制字节组成"),
    };
    let bytes = address.split(separator).collect::<Vec<_>>();
    if bytes.len() != 6 || bytes.iter().any(|byte| byte.len() != 2 || !byte.bytes().all(|c| c.is_ascii_hexdigit())) {
        bail!("蓝牙地址必须由 6 组十六进制字节组成");
    }
    Ok(bytes.join(":").to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_normalization_rejects_partial_and_mixed_addresses() {
        assert_eq!(normalize_address("ab-cd-ef-12-34-56").unwrap(), "AB:CD:EF:12:34:56");
        for invalid in ["", "01:02:03:04:05", "01:02:03:04:05:6", "01:02-03:04:05:06", "01:02:03:04:05:GG", "01:02:03:04:05:06\n"] {
            assert!(normalize_address(invalid).is_err());
        }
    }

    #[test]
    fn service_uuid_matches_native_bytes() {
        assert_eq!(uuid::Uuid::parse_str(SERVICE_UUID).unwrap().as_bytes(), &SERVICE_UUID_BYTES);
    }
}
