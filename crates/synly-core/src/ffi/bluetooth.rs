//! 系统蓝牙提供者与有界 IO 的 UniFFI 回调接口.

use super::{FfiError, runtime};
use crate::bluetooth::{self, BluetoothAvailability, BluetoothPeer};

// Kotlin 异常生成器会实现 Throwable.message, 错误负载字段使用独立名称避免冲突.
#[derive(Debug, uniffi::Error)]
pub enum FfiBluetoothError {
    PermissionDenied { reason: String },
    Disabled { reason: String },
    Unpaired { reason: String },
    Failed { reason: String },
}

impl std::fmt::Display for FfiBluetoothError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::PermissionDenied { reason } | Self::Disabled { reason } | Self::Unpaired { reason } | Self::Failed { reason }) = self;
        write!(formatter, "{reason}")
    }
}
impl std::error::Error for FfiBluetoothError {}

#[derive(uniffi::Enum)]
pub enum FfiBluetoothAvailability { Available, Disabled, PermissionDenied, Unsupported }
impl From<BluetoothAvailability> for FfiBluetoothAvailability {
    fn from(value: BluetoothAvailability) -> Self {
        match value {
            BluetoothAvailability::Available => Self::Available,
            BluetoothAvailability::Disabled => Self::Disabled,
            BluetoothAvailability::PermissionDenied => Self::PermissionDenied,
            BluetoothAvailability::Unsupported => Self::Unsupported,
        }
    }
}
impl From<FfiBluetoothAvailability> for BluetoothAvailability {
    fn from(value: FfiBluetoothAvailability) -> Self {
        match value {
            FfiBluetoothAvailability::Available => Self::Available,
            FfiBluetoothAvailability::Disabled => Self::Disabled,
            FfiBluetoothAvailability::PermissionDenied => Self::PermissionDenied,
            FfiBluetoothAvailability::Unsupported => Self::Unsupported,
        }
    }
}

#[derive(uniffi::Record)]
pub struct FfiBluetoothPeer { pub address: String, pub name: String }
impl From<BluetoothPeer> for FfiBluetoothPeer {
    fn from(peer: BluetoothPeer) -> Self { Self { address: peer.address, name: peer.name } }
}
impl From<FfiBluetoothPeer> for BluetoothPeer {
    fn from(peer: FfiBluetoothPeer) -> Self { Self { address: peer.address, name: peer.name } }
}

/// 只由 Android 应用的系统后端实现. socket 标识不能从远端网络提供.
#[uniffi::export(callback_interface)]
pub trait FfiBluetoothProvider: Send + Sync {
    fn availability(&self) -> Result<FfiBluetoothAvailability, FfiBluetoothError>;
    fn paired_devices(&self) -> Result<Vec<FfiBluetoothPeer>, FfiBluetoothError>;
    fn query_service(&self, address: String, uuid: String) -> Result<bool, FfiBluetoothError>;
    fn create_socket(&self, address: String, uuid: String) -> Result<u64, FfiBluetoothError>;
    fn connect_socket(&self, socket: u64) -> Result<(), FfiBluetoothError>;
    fn read_socket(&self, socket: u64, max_bytes: u32) -> Result<Vec<u8>, FfiBluetoothError>;
    fn write_socket(&self, socket: u64, bytes: Vec<u8>) -> Result<(), FfiBluetoothError>;
    fn close_socket(&self, socket: u64);
}

#[cfg(target_os = "android")]
struct ProviderBridge { inner: Box<dyn FfiBluetoothProvider> }

#[cfg(target_os = "android")]
fn io_error(error: FfiBluetoothError) -> std::io::Error {
    let kind = match &error {
        FfiBluetoothError::PermissionDenied { .. } | FfiBluetoothError::Unpaired { .. } => std::io::ErrorKind::PermissionDenied,
        FfiBluetoothError::Disabled { .. } => std::io::ErrorKind::NotConnected,
        FfiBluetoothError::Failed { .. } => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error)
}

#[cfg(target_os = "android")]
impl bluetooth::provider::Provider for ProviderBridge {
    fn availability(&self) -> anyhow::Result<BluetoothAvailability> { Ok(self.inner.availability()?.into()) }
    fn paired_devices(&self) -> anyhow::Result<Vec<BluetoothPeer>> { Ok(self.inner.paired_devices()?.into_iter().map(Into::into).collect()) }
    fn query_service(&self, address: String, uuid: String) -> anyhow::Result<bool> { Ok(self.inner.query_service(address, uuid)?) }
    fn create_socket(&self, address: String, uuid: String) -> anyhow::Result<u64> { Ok(self.inner.create_socket(address, uuid)?) }
    fn connect_socket(&self, socket: u64) -> anyhow::Result<()> { Ok(self.inner.connect_socket(socket)?) }
    fn read_socket(&self, socket: u64, max_bytes: u32) -> std::io::Result<Vec<u8>> { self.inner.read_socket(socket, max_bytes).map_err(io_error) }
    fn write_socket(&self, socket: u64, bytes: Vec<u8>) -> std::io::Result<()> { self.inner.write_socket(socket, bytes).map_err(io_error) }
    fn close_socket(&self, socket: u64) { self.inner.close_socket(socket); }
}

#[uniffi::export]
pub fn register_bluetooth_provider(provider: Box<dyn FfiBluetoothProvider>) -> Result<(), FfiError> {
    #[cfg(target_os = "android")]
    { bluetooth::register_provider(std::sync::Arc::new(ProviderBridge { inner: provider })).map_err(Into::into) }
    #[cfg(not(target_os = "android"))]
    { let _ = provider; Err(FfiError::Failed { message: "当前平台使用原生蓝牙后端, 不接受外部提供者".to_owned() }) }
}

#[uniffi::export]
pub fn bluetooth_service_uuid() -> String { bluetooth::SERVICE_UUID.to_owned() }

#[uniffi::export]
pub fn bluetooth_availability() -> Result<FfiBluetoothAvailability, FfiError> {
    runtime().block_on(bluetooth::availability()).map(Into::into).map_err(Into::into)
}

#[uniffi::export]
pub fn bluetooth_paired_devices() -> Result<Vec<FfiBluetoothPeer>, FfiError> {
    runtime().block_on(bluetooth::paired_devices()).map(|peers| peers.into_iter().map(Into::into).collect()).map_err(Into::into)
}

#[uniffi::export]
pub fn bluetooth_query_service(peer: FfiBluetoothPeer) -> Result<bool, FfiError> {
    runtime().block_on(bluetooth::query_service(&peer.into())).map(|endpoint| endpoint.is_some()).map_err(Into::into)
}
