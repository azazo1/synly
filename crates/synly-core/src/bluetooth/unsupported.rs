//! 尚无原生后端的平台明确报告不可用, 不伪造蓝牙发现结果.

use super::{BluetoothAvailability, BluetoothConnection, BluetoothEndpoint, BluetoothPeer};
use anyhow::{Result, bail};

pub struct Listener;

impl Listener {
    pub async fn accept(&mut self) -> Result<BluetoothConnection> {
        bail!("当前平台没有可用的经典蓝牙后端")
    }
}

pub async fn availability() -> Result<BluetoothAvailability> {
    Ok(BluetoothAvailability::Unsupported)
}

pub async fn paired_devices() -> Result<Vec<BluetoothPeer>> {
    bail!("当前平台没有可用的经典蓝牙后端")
}

pub async fn query_service(_peer: BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    bail!("当前平台没有可用的经典蓝牙后端")
}

pub async fn connect(_address: String) -> Result<BluetoothConnection> {
    bail!("当前平台没有可用的经典蓝牙后端")
}

pub async fn listen() -> Result<Listener> {
    bail!("当前平台没有可用的经典蓝牙后端")
}
