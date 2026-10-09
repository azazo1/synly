//! 外部平台提供者的安全 RFCOMM 契约与可取消连接生命周期.

use super::{BluetoothAvailability, BluetoothEndpoint, BluetoothPeer, SERVICE_UUID, normalize_address};
use crate::transport::{bridge::{self, BlockingByteIo, IO_FRAGMENT_BYTES}, stream::ByteStream};
use anyhow::{Context, Result, bail};
use std::io;
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
use tokio::sync::Semaphore;

/// 提供者属于受信任的本机平台层, 不从网络消息构造.
/// create_socket 只能创建安全 RFCOMM socket, 不发起系统配对.
/// connect_socket 必须验证已配对, 请求认证与加密, 且成功后重新验证配对.
/// close_socket 必须幂等, 且立即打断正在进行的 connect/read/write.
pub trait Provider: Send + Sync + 'static {
    fn availability(&self) -> Result<BluetoothAvailability>;
    fn paired_devices(&self) -> Result<Vec<BluetoothPeer>>;
    fn query_service(&self, address: String, uuid: String) -> Result<bool>;
    fn create_socket(&self, address: String, uuid: String) -> Result<u64>;
    fn connect_socket(&self, socket: u64) -> Result<()>;
    fn read_socket(&self, socket: u64, max_bytes: u32) -> io::Result<Vec<u8>>;
    fn write_socket(&self, socket: u64, bytes: Vec<u8>) -> io::Result<()>;
    fn close_socket(&self, socket: u64);
}

fn slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(2))).clone()
}

pub async fn availability(provider: Arc<dyn Provider>) -> Result<BluetoothAvailability> {
    tokio::task::spawn_blocking(move || provider.availability()).await?
}

async fn require_available(provider: Arc<dyn Provider>) -> Result<()> {
    match availability(provider).await? {
        BluetoothAvailability::Available => Ok(()),
        BluetoothAvailability::Disabled => bail!("系统蓝牙已关闭"),
        BluetoothAvailability::PermissionDenied => bail!("系统拒绝蓝牙权限"),
        BluetoothAvailability::Unsupported => bail!("当前平台没有可用的经典蓝牙控制器"),
    }
}

pub async fn paired_devices(provider: Arc<dyn Provider>) -> Result<Vec<BluetoothPeer>> {
    require_available(provider.clone()).await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<BluetoothPeer>> {
        let peers = provider.paired_devices()?;
        if peers.len() > 256 { bail!("系统已配对设备数量超出枚举上限"); }
        let mut peers = peers.into_iter().filter_map(|peer| {
            normalize_address(&peer.address).ok().map(|address| BluetoothPeer { address, name: peer.name })
        }).collect::<Vec<_>>();
        peers.sort_by(|a, b| a.address.cmp(&b.address));
        peers.dedup_by(|a, b| a.address == b.address);
        Ok(peers)
    }).await?
}

pub async fn query_service(provider: Arc<dyn Provider>, mut peer: BluetoothPeer) -> Result<Option<BluetoothEndpoint>> {
    peer.address = normalize_address(&peer.address)?;
    require_available(provider.clone()).await?;
    let permit = slots().acquire_owned().await?;
    tokio::task::spawn_blocking(move || -> Result<_> {
        let _permit = permit;
        let found = provider.query_service(peer.address.clone(), SERVICE_UUID.to_owned())?;
        // UUID socket 由系统在连接时解析通道, 不伪造一个可用的 RFCOMM 通道号.
        Ok(found.then_some(BluetoothEndpoint { peer, channel: None }))
    }).await?
}

struct Socket {
    provider: Arc<dyn Provider>,
    handle: u64,
    closed: AtomicBool,
}

impl Socket {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) { self.provider.close_socket(self.handle); }
    }
}

impl Drop for Socket {
    fn drop(&mut self) { self.close(); }
}

impl BlockingByteIo for Socket {
    fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::Acquire) { return Ok(0); }
        let maximum = buffer.len().min(IO_FRAGMENT_BYTES);
        let bytes = self.provider.read_socket(self.handle, maximum as u32)?;
        if bytes.len() > maximum { return Err(io::Error::new(io::ErrorKind::InvalidData, "平台读取超出请求的缓冲区大小")); }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) { return Err(io::Error::new(io::ErrorKind::BrokenPipe, "蓝牙 socket 已关闭")); }
        if bytes.len() > IO_FRAGMENT_BYTES { return Err(io::Error::new(io::ErrorKind::InvalidInput, "蓝牙写片段过大")); }
        self.provider.write_socket(self.handle, bytes.to_vec())
    }

    fn close(&self) { Self::close(self); }
}

struct PendingSocket(Option<Arc<Socket>>);
impl Drop for PendingSocket {
    fn drop(&mut self) {
        if let Some(socket) = &self.0 { socket.close(); }
    }
}

pub async fn connect(provider: Arc<dyn Provider>, address: String) -> Result<(ByteStream, BluetoothPeer)> {
    let address = normalize_address(&address)?;
    require_available(provider.clone()).await?;
    let permit = slots().acquire_owned().await?;
    let (socket, peer, permit) = tokio::task::spawn_blocking(move || -> Result<_> {
        let peers = provider.paired_devices()?;
        if peers.len() > 256 { bail!("系统已配对设备数量超出枚举上限"); }
        let peer = peers.into_iter().find(|peer| {
            normalize_address(&peer.address).is_ok_and(|candidate| candidate == address)
        }).context("设备尚未系统配对, 或系统配对已被移除")?;
        let peer = BluetoothPeer { address: address.clone(), name: peer.name };
        let handle = provider.create_socket(address, SERVICE_UUID.to_owned())?;
        if handle == 0 { bail!("平台未返回有效的蓝牙 socket"); }
        Ok((Arc::new(Socket { provider, handle, closed: AtomicBool::new(false) }), peer, permit))
    }).await??;
    // 单纯 Arc 的最后引用释放不够: 阻塞 connect 也持有引用, 取消必须主动 close.
    let mut pending = PendingSocket(Some(socket.clone()));
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        socket.provider.connect_socket(socket.handle)
    }).await??;
    let socket = pending.0.take().context("蓝牙连接已取消")?;
    tracing::info!(address = %peer.address, "平台安全 RFCOMM 连接已建立");
    Ok((bridge::open(socket), peer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex, atomic::AtomicUsize};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::sync::Notify;

    const ADDRESS: &str = "11:22:33:44:55:66";
    #[derive(Default)]
    struct Fake {
        closed: Mutex<bool>,
        changed: Condvar,
        connecting: Notify,
        connects_immediately: bool,
        closes: AtomicUsize,
    }
    impl Provider for Fake {
        fn availability(&self) -> Result<BluetoothAvailability> { Ok(BluetoothAvailability::Available) }
        fn paired_devices(&self) -> Result<Vec<BluetoothPeer>> { Ok(vec![BluetoothPeer { address: ADDRESS.to_owned(), name: "测试设备".to_owned() }]) }
        fn query_service(&self, _: String, _: String) -> Result<bool> { Ok(true) }
        fn create_socket(&self, _: String, uuid: String) -> Result<u64> { assert_eq!(uuid, SERVICE_UUID); Ok(1) }
        fn connect_socket(&self, _: u64) -> Result<()> {
            self.connecting.notify_one();
            if self.connects_immediately { return Ok(()); }
            let mut closed = self.closed.lock().unwrap();
            while !*closed { closed = self.changed.wait(closed).unwrap(); }
            bail!("连接已关闭")
        }
        fn read_socket(&self, _: u64, max: u32) -> io::Result<Vec<u8>> { Ok(vec![0; max as usize + 1]) }
        fn write_socket(&self, _: u64, _: Vec<u8>) -> io::Result<()> { Ok(()) }
        fn close_socket(&self, _: u64) {
            self.closes.fetch_add(1, Ordering::AcqRel);
            *self.closed.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    #[tokio::test]
    async fn cancellation_closes_a_socket_while_connect_is_blocked() {
        let provider = Arc::new(Fake::default());
        let connecting = tokio::spawn(connect(provider.clone(), ADDRESS.to_owned()));
        provider.connecting.notified().await;
        connecting.abort();
        assert!(connecting.await.unwrap_err().is_cancelled());
        assert!(*provider.closed.lock().unwrap());
        assert_eq!(provider.closes.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn oversized_foreign_read_fails_without_copying_out_of_bounds() {
        let provider = Arc::new(Fake { connects_immediately: true, ..Default::default() });
        let (mut stream, _) = connect(provider.clone(), ADDRESS.to_owned()).await.unwrap();
        let mut data = [0; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), stream.read(&mut data)).await.unwrap().unwrap_err().kind(), io::ErrorKind::InvalidData);
        drop(stream);
        assert_eq!(provider.closes.load(Ordering::Acquire), 1);
    }
}
