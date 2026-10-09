//! Android 由应用注册系统 BluetoothSocket 提供者, 不使用 TCP 替代蓝牙.

use super::{BluetoothAvailability, BluetoothConnection, BluetoothEndpoint, BluetoothPeer, provider};
use anyhow::{Context, Result, bail};
use std::sync::{Arc, OnceLock};

static PROVIDER: OnceLock<Arc<dyn provider::Provider>> = OnceLock::new();

pub(super) fn register(provider: Arc<dyn provider::Provider>) -> Result<()> {
    if PROVIDER.set(provider).is_err() { bail!("系统蓝牙提供者已注册, 不能在活跃连接中替换"); }
    Ok(())
}

fn current() -> Result<Arc<dyn provider::Provider>> {
    PROVIDER.get().cloned().context("系统蓝牙提供者尚未初始化")
}

pub async fn availability() -> Result<BluetoothAvailability> {
    match PROVIDER.get() {
        Some(provider) => provider::availability(provider.clone()).await,
        None => Ok(BluetoothAvailability::Unsupported),
    }
}

pub async fn paired_devices() -> Result<Vec<BluetoothPeer>> {
    provider::paired_devices(current()?).await
}

pub async fn query_service(peer: BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    provider::query_service(current()?, peer).await
}

pub async fn connect(address: String) -> Result<BluetoothConnection> {
    let (stream, peer) = provider::connect(current()?, address).await?;
    Ok(BluetoothConnection::authenticated(stream, peer))
}

pub struct Listener;
impl Listener {
    pub async fn accept(&mut self) -> Result<BluetoothConnection> {
        bail!("Android 首版仅支持蓝牙 client")
    }
}

pub async fn listen() -> Result<Listener> {
    bail!("Android 首版仅支持蓝牙 client")
}
