//! Windows 系统配对设备, SDP 和安全 RFCOMM 接入.

mod discovery;
mod socket;

use super::{BluetoothAvailability, BluetoothConnection, BluetoothEndpoint, BluetoothPeer};
use crate::transport::bridge;
use anyhow::{Context, Result, bail};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use ::windows::Devices::Radios::{Radio, RadioKind, RadioState};
use ::windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows_sys::Win32::Foundation::E_ACCESSDENIED;

struct Apartment;
impl Apartment {
    fn initialize() -> ::windows::core::Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)?; }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) { unsafe { RoUninitialize(); } }
}

pub async fn availability() -> Result<BluetoothAvailability> {
    let result = tokio::task::spawn_blocking(|| -> ::windows::core::Result<BluetoothAvailability> {
        let _apartment = Apartment::initialize()?;
        let radios = Radio::GetRadiosAsync()?.join()?;
        let mut found = false;
        for index in 0..radios.Size()? {
            let radio = radios.GetAt(index)?;
            if radio.Kind()? == RadioKind::Bluetooth {
                found = true;
                if radio.State()? == RadioState::On { return Ok(BluetoothAvailability::Available); }
            }
        }
        Ok(if found { BluetoothAvailability::Disabled } else { BluetoothAvailability::Unsupported })
    }).await?;
    match result {
        Err(error) if error.code().0 == E_ACCESSDENIED => Ok(BluetoothAvailability::PermissionDenied),
        result => result.context("无法读取 Windows 蓝牙控制器状态"),
    }
}

async fn ensure_available() -> Result<()> {
    match availability().await? {
        BluetoothAvailability::Available => Ok(()),
        BluetoothAvailability::Disabled => bail!("系统蓝牙已关闭或被系统策略禁用"),
        BluetoothAvailability::PermissionDenied => bail!("系统拒绝蓝牙权限"),
        BluetoothAvailability::Unsupported => bail!("当前系统没有可用的蓝牙控制器"),
    }
}

pub async fn paired_devices() -> Result<Vec<BluetoothPeer>> {
    ensure_available().await?;
    tokio::task::spawn_blocking(discovery::paired_devices).await?.context("读取系统已配对设备失败")
}

fn lookup_slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(2))).clone()
}

pub async fn query_service(peer: BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    ensure_available().await?;
    let permit = lookup_slots().acquire_owned().await?;
    tokio::task::spawn_blocking(move || -> Result<Option<BluetoothEndpoint>> {
        let _permit = permit;
        let address = discovery::parse_address(&peer.address)?;
        let channel = discovery::query_service(address)?;
        Ok(channel.map(|channel| BluetoothEndpoint { peer, channel: Some(channel) }))
    }).await?
}

pub async fn connect(address: String) -> Result<BluetoothConnection> {
    ensure_available().await?;
    let permit = lookup_slots().acquire_owned().await?;
    let (socket, peer) = tokio::task::spawn_blocking(move || -> Result<_> {
        let _permit = permit;
        let address = discovery::parse_address(&address)?;
        let peer = discovery::require_paired(address)?;
        let channel = discovery::query_service(address)?.context("对端未提供 Synly 蓝牙服务")?;
        let socket = socket::Socket::protected()?;
        socket.connect(address, channel)?;
        Ok((socket, peer))
    }).await??;
    tracing::info!(address = %peer.address, "系统已配对且加密的蓝牙链路已连接");
    Ok(BluetoothConnection::authenticated(bridge::open(socket), peer))
}

struct Accepted {
    socket: Arc<socket::Socket>,
    peer: BluetoothPeer,
}

pub struct Listener {
    socket: Arc<socket::Socket>,
    _advertisement: discovery::Advertisement,
    incoming: mpsc::Receiver<Result<Accepted>>,
    worker: JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.socket.close();
        self.worker.abort();
    }
}

impl Listener {
    pub async fn accept(&mut self) -> Result<BluetoothConnection> {
        let accepted = self.incoming.recv().await.context("蓝牙监听器已关闭")??;
        tracing::info!(address = %accepted.peer.address, "接受系统已配对且加密的蓝牙连接");
        Ok(BluetoothConnection::authenticated(bridge::open(accepted.socket), accepted.peer))
    }
}

pub async fn listen() -> Result<Listener> {
    ensure_available().await?;
    let (socket, advertisement) = tokio::task::spawn_blocking(|| -> Result<_> {
        let socket = socket::Socket::protected()?;
        let local = socket.listen()?;
        let advertisement = discovery::Advertisement::register(local)?;
        Ok((socket, advertisement))
    }).await??;
    let (sender, incoming) = mpsc::channel(8);
    let accepting = socket.clone();
    let worker = tokio::task::spawn_blocking(move || {
        loop {
            let accepted = (|| -> Result<Option<Accepted>> {
                if !accepting.wait(false, Duration::from_millis(100))? { return Ok(None); }
                let Some((socket, address)) = accepting.accept()? else { return Ok(None) };
                match discovery::require_paired(address) {
                    Ok(peer) => Ok(Some(Accepted { socket, peer })),
                    Err(error) => {
                        tracing::debug!(%error, "拒绝没有有效系统配对的蓝牙连接");
                        Ok(None)
                    }
                }
            })();
            match accepted {
                Ok(Some(accepted)) => if sender.blocking_send(Ok(accepted)).is_err() { accepting.close(); return; },
                Ok(None) => {},
                Err(error) => { let _ = sender.blocking_send(Err(error)); accepting.close(); return; }
            }
        }
    });
    tracing::info!("Synly 蓝牙服务已注册, 等待系统已配对设备接入");
    Ok(Listener { socket, _advertisement: advertisement, incoming, worker })
}
