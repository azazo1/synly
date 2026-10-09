//! IOBluetooth 与异步字节流的边界.

use super::{BluetoothAvailability, BluetoothConnection, BluetoothEndpoint, BluetoothPeer, SERVICE_UUID_BYTES, normalize_address};
use crate::transport::stream::ByteStream;
use anyhow::{Context, Result, bail};
use std::ffi::{CString, c_char, c_int, c_void};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream as StdStream;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[repr(C)]
#[derive(Clone, Copy)]
struct NativePeer {
    address: [c_char; 18],
    name: [c_char; 256],
}

unsafe extern "C" {
    fn synly_bt_available() -> c_int;
    fn synly_bt_paired(peers: *mut NativePeer, capacity: usize, count: *mut usize) -> c_int;
    fn synly_bt_query(address: *const c_char, uuid: *const u8, channel: *mut u8) -> c_int;
    fn synly_bt_connect(address: *const c_char, uuid: *const u8, fd: *mut c_int) -> c_int;
    fn synly_bt_listen(
        uuid: *const u8,
        callback: unsafe extern "C" fn(*mut c_void, c_int, *const NativePeer),
        context: *mut c_void,
        listener: *mut *mut c_void,
    ) -> c_int;
    fn synly_bt_stop_listener(listener: *mut c_void);
}

fn check(status: c_int) -> Result<()> {
    match status {
        0 => Ok(()),
        -1 => bail!("系统蓝牙已关闭"),
        -2 => bail!("系统拒绝蓝牙权限, 请在系统设置中允许 Synly 使用蓝牙"),
        -3 => bail!("当前系统没有可用的经典蓝牙控制器"),
        -4 => bail!("设备尚未系统配对, 或系统配对已被移除"),
        -5 => bail!("蓝牙服务查询或连接超时"),
        -6 => bail!("蓝牙链路未加密, 已拒绝连接"),
        -7 => bail!("无法创建蓝牙服务或字节流"),
        -8 => bail!("对端未提供 Synly 蓝牙服务"),
        -9 => bail!("蓝牙服务入口或查询正在使用中"),
        -10 => bail!("系统已配对设备数量超出枚举上限"),
        status => bail!("macOS 蓝牙系统接口错误: 0x{:08x}", status as u32),
    }
}

fn text<const N: usize>(raw: &[c_char; N]) -> String {
    let bytes = raw.iter().take_while(|&&byte| byte != 0).map(|&byte| byte as u8).collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn peer(raw: &NativePeer) -> Result<BluetoothPeer> {
    Ok(BluetoothPeer { address: normalize_address(&text(&raw.address))?, name: text(&raw.name) })
}

pub async fn availability() -> Result<BluetoothAvailability> {
    let status = tokio::task::spawn_blocking(|| unsafe { synly_bt_available() }).await?;
    Ok(match status {
        0 => BluetoothAvailability::Available,
        -1 => BluetoothAvailability::Disabled,
        -2 => BluetoothAvailability::PermissionDenied,
        -3 => BluetoothAvailability::Unsupported,
        status => { check(status)?; unreachable!() }
    })
}

pub async fn paired_devices() -> Result<Vec<BluetoothPeer>> {
    tokio::task::spawn_blocking(|| {
        let mut peers = [NativePeer { address: [0; 18], name: [0; 256] }; 256];
        let mut count = 0;
        check(unsafe { synly_bt_paired(peers.as_mut_ptr(), peers.len(), &mut count) })?;
        if count > peers.len() { bail!("蓝牙枚举返回了无效的设备数量"); }
        peers[..count].iter().map(peer).collect()
    }).await?
}

pub async fn query_service(peer: BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    tokio::task::spawn_blocking(move || {
        let address = CString::new(peer.address.as_str())?;
        let mut channel = 0;
        check(unsafe { synly_bt_query(address.as_ptr(), SERVICE_UUID_BYTES.as_ptr(), &mut channel) })?;
        Ok((channel != 0).then_some(BluetoothEndpoint { peer, channel: Some(channel) }))
    }).await?
}

pub async fn connect(address: String) -> Result<BluetoothConnection> {
    let stream = tokio::task::spawn_blocking({
        let address = address.clone();
        move || -> Result<StdStream> {
            let address = CString::new(address)?;
            let mut fd = -1;
            check(unsafe { synly_bt_connect(address.as_ptr(), SERVICE_UUID_BYTES.as_ptr(), &mut fd) })?;
            if fd < 0 { bail!("蓝牙连接没有返回有效字节流"); }
            // 成功返回时桥接层将 fd 的唯一所有权转移给 Rust.
            Ok(unsafe { StdStream::from_raw_fd(fd) })
        }
    }).await??;
    let stream = UnixStream::from_std(stream).context("无法注册蓝牙异步字节流")?;
    tracing::info!(%address, "系统已配对且加密的蓝牙链路已连接");
    Ok(BluetoothConnection::authenticated(ByteStream::new(stream), BluetoothPeer { name: address.clone(), address }))
}

struct Accepted {
    stream: StdStream,
    peer: BluetoothPeer,
}

struct AcceptContext(mpsc::Sender<Accepted>);

pub struct Listener {
    native: usize,
    // 原生 listener 停止并确认没有回调后, 才能释放 context.
    _context: Box<AcceptContext>,
    incoming: mpsc::Receiver<Accepted>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe { synly_bt_stop_listener(self.native as *mut c_void) };
    }
}

impl Listener {
    pub async fn accept(&mut self) -> Result<BluetoothConnection> {
        let incoming = self.incoming.recv().await.context("蓝牙监听器已关闭")?;
        let stream = UnixStream::from_std(incoming.stream).context("无法注册蓝牙异步字节流")?;
        tracing::info!(address = %incoming.peer.address, "接受系统已配对且加密的蓝牙连接");
        Ok(BluetoothConnection::authenticated(ByteStream::new(stream), incoming.peer))
    }
}

unsafe extern "C" fn accepted(context: *mut c_void, fd: c_int, raw_peer: *const NativePeer) {
    if fd < 0 { return; }
    // 原生端在 callback 前移交 fd; 拒绝入队时关闭它, 防止积压连接耗尽资源.
    let stream = unsafe { StdStream::from_raw_fd(fd) };
    let Some(context) = (unsafe { (context as *const AcceptContext).as_ref() }) else { return };
    let Some(raw_peer) = (unsafe { raw_peer.as_ref() }) else { return };
    let Ok(peer) = peer(raw_peer) else { return };
    if context.0.try_send(Accepted { stream, peer }).is_err() {
        tracing::warn!("蓝牙待处理连接队列已满或已关闭, 已拒绝新连接");
    }
}

pub async fn listen() -> Result<Listener> {
    tokio::task::spawn_blocking(|| {
        let (sender, incoming) = mpsc::channel(8);
        let mut context = Box::new(AcceptContext(sender));
        let mut native = std::ptr::null_mut();
        check(unsafe {
            synly_bt_listen(SERVICE_UUID_BYTES.as_ptr(), accepted, (&mut *context as *mut AcceptContext).cast(), &mut native)
        })?;
        if native.is_null() { bail!("蓝牙监听器没有返回有效句柄"); }
        tracing::info!("Synly 蓝牙服务已注册, 等待系统已配对设备接入");
        Ok(Listener { native: native as usize, _context: context, incoming })
    }).await?
}
