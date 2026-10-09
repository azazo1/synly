//! 非阻塞 Winsock RFCOMM socket, 通过有界阻塞桥接接入 Tokio.

use super::discovery;
use crate::transport::bridge::BlockingByteIo;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH, SOL_RFCOMM, SO_BTH_AUTHENTICATE, SO_BTH_ENCRYPT};
use windows_sys::Win32::Networking::WinSock::*;

pub(super) fn initialize() -> io::Result<()> {
    static STARTUP: OnceLock<i32> = OnceLock::new();
    let result = *STARTUP.get_or_init(|| {
        let mut data = WSADATA::default();
        unsafe { WSAStartup(0x0202, &mut data) }
    });
    if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

pub(super) fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn check(result: i32) -> io::Result<()> {
    if result == SOCKET_ERROR { Err(last_error()) } else { Ok(()) }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "蓝牙 socket 已关闭")
}

struct State {
    socket: Option<SOCKET>,
    peer: Option<u64>,
    checked: Instant,
}

pub(super) struct Socket(Mutex<State>);

impl Socket {
    pub fn protected() -> io::Result<Arc<Self>> {
        initialize()?;
        let raw = unsafe { WSASocketW(i32::from(AF_BTH), SOCK_STREAM, BTHPROTO_RFCOMM as i32, std::ptr::null(), 0, 0) };
        if raw == INVALID_SOCKET { return Err(last_error()); }
        let socket = Self::owned(raw, None);
        socket.with_socket(|raw| {
            let enabled = 1u32;
            // Windows 只允许设置这两个选项, 不能用 getsockopt 伪造加密状态检查.
            // 成功的 connect/accept 是系统强制认证与加密已经生效的证据.
            for option in [SO_BTH_AUTHENTICATE, SO_BTH_ENCRYPT] {
                check(unsafe {
                    setsockopt(raw, SOL_RFCOMM as i32, option as i32, (&enabled as *const u32).cast(), size_of::<u32>() as i32)
                })?;
            }
            Ok(())
        })?;
        socket.prepare()?;
        Ok(socket)
    }

    fn owned(raw: SOCKET, peer: Option<u64>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(State { socket: Some(raw), peer, checked: Instant::now() })))
    }

    fn prepare(&self) -> io::Result<()> {
        self.with_socket(|raw| {
            let mut enabled = 1;
            check(unsafe { ioctlsocket(raw, FIONBIO, &mut enabled) })?;
            let bytes = 2048i32;
            for option in [SO_SNDBUF, SO_RCVBUF] {
                if let Err(error) = check(unsafe {
                    setsockopt(raw, SOL_SOCKET, option, (&bytes as *const i32).cast(), size_of::<i32>() as i32)
                }) {
                    tracing::debug!(%error, option, "蓝牙驱动不支持小 socket 缓冲, 应用队列仍保持有界");
                }
            }
            Ok(())
        })
    }

    fn with_socket<T>(&self, operation: impl FnOnce(SOCKET) -> io::Result<T>) -> io::Result<T> {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let raw = state.socket.ok_or_else(closed)?;
        if let Some(peer) = state.peer {
            if state.checked.elapsed() >= Duration::from_secs(1) {
                discovery::require_paired(peer)?;
                state.checked = Instant::now();
            }
        }
        operation(raw)
    }

    pub fn close(&self) {
        let raw = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).socket.take();
        if let Some(raw) = raw {
            unsafe { shutdown(raw, SD_BOTH); closesocket(raw); }
        }
    }

    pub fn wait(&self, write: bool, duration: Duration) -> io::Result<bool> {
        let raw = self.with_socket(Ok)?;
        let mut ready = FD_SET::default();
        ready.fd_count = 1;
        ready.fd_array[0] = raw;
        let mut exceptional = ready;
        let timeout = TIMEVAL { tv_sec: duration.as_secs() as i32, tv_usec: duration.subsec_micros() as i32 };
        let result = unsafe {
            select(0,
                if write { std::ptr::null_mut() } else { &mut ready },
                if write { &mut ready } else { std::ptr::null_mut() },
                &mut exceptional, &timeout)
        };
        let outcome = check(result);
        // 等待期间允许 close, 但在使用就绪结果前重新核对归属, 避免句柄复用.
        self.with_socket(|current| if current == raw { Ok(()) } else { Err(closed()) })?;
        outcome?;
        Ok(result > 0)
    }

    pub fn connect(&self, address: u64, channel: u8) -> io::Result<()> {
        discovery::require_paired(address)?;
        let target = SOCKADDR_BTH { addressFamily: AF_BTH, btAddr: address, port: u32::from(channel), ..Default::default() };
        let result = self.with_socket(|raw| {
            check(unsafe { connect(raw, (&target as *const SOCKADDR_BTH).cast(), size_of::<SOCKADDR_BTH>() as i32) })
        });
        match result {
            Ok(()) => {}
            Err(error) if matches!(error.raw_os_error(), Some(WSAEWOULDBLOCK | WSAEINPROGRESS | WSAEALREADY)) => {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !self.wait(true, Duration::from_millis(100))? {
                    if Instant::now() >= deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "蓝牙连接超时")); }
                }
                self.with_socket(|raw| {
                    let mut result = 0i32;
                    let mut size = size_of::<i32>() as i32;
                    check(unsafe { getsockopt(raw, SOL_SOCKET, SO_ERROR, (&mut result as *mut i32).cast(), &mut size) })?;
                    if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
                })?;
            }
            Err(error) => return Err(error),
        }
        discovery::require_paired(address)?;
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).peer = Some(address);
        Ok(())
    }

    pub fn listen(&self) -> io::Result<SOCKADDR_BTH> {
        self.with_socket(|raw| {
            let local = SOCKADDR_BTH { addressFamily: AF_BTH, port: u32::MAX, ..Default::default() };
            check(unsafe { bind(raw, (&local as *const SOCKADDR_BTH).cast(), size_of::<SOCKADDR_BTH>() as i32) })?;
            check(unsafe { listen(raw, 8) })?;
            let mut local = SOCKADDR_BTH::default();
            let mut size = size_of::<SOCKADDR_BTH>() as i32;
            check(unsafe { getsockname(raw, (&mut local as *mut SOCKADDR_BTH).cast(), &mut size) })?;
            if size < size_of::<SOCKADDR_BTH>() as i32 || local.addressFamily != AF_BTH || !(1..=30).contains(&{local.port}) {
                return Err(io::Error::other("系统返回了无效的 RFCOMM 监听端点"));
            }
            Ok(local)
        })
    }

    pub fn accept(&self) -> io::Result<Option<(Arc<Self>, u64)>> {
        self.with_socket(|raw| {
            let mut remote = SOCKADDR_BTH::default();
            let mut size = size_of::<SOCKADDR_BTH>() as i32;
            let child = unsafe { accept(raw, (&mut remote as *mut SOCKADDR_BTH).cast(), &mut size) };
            if child == INVALID_SOCKET {
                let error = last_error();
                if matches!(error.raw_os_error(), Some(WSAEWOULDBLOCK | WSAEHOSTDOWN | WSAECONNABORTED | WSAECONNRESET)) { return Ok(None); }
                return Err(error);
            }
            let address = remote.btAddr;
            // 子 socket 继承监听器的强制认证与加密, 在进入应用前额外校验系统配对记录.
            let socket = Self::owned(child, Some(address));
            if size < size_of::<SOCKADDR_BTH>() as i32 || remote.addressFamily != AF_BTH { return Err(io::Error::other("系统返回了无效的蓝牙对端地址")); }
            if let Err(error) = discovery::require_paired(address).and_then(|_| socket.prepare()) {
                tracing::debug!(%error, "拒绝无法验证系统配对或建立异步 IO 的蓝牙连接");
                return Ok(None);
            }
            Ok(Some((socket, address)))
        })
    }
}

impl Drop for Socket {
    fn drop(&mut self) { self.close(); }
}

impl BlockingByteIo for Socket {
    fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = self.with_socket(|raw| {
                let size = unsafe { recv(raw, buffer.as_mut_ptr(), buffer.len().min(i32::MAX as usize) as i32, 0) };
                if size == SOCKET_ERROR { Err(last_error()) } else { Ok(size as usize) }
            });
            match result {
                Err(error) if error.raw_os_error() == Some(WSAEWOULDBLOCK) => { self.wait(false, Duration::from_millis(100))?; }
                result => return result,
            }
        }
    }

    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut offset = 0;
        while offset < bytes.len() {
            let result = self.with_socket(|raw| {
                let size = unsafe { send(raw, bytes[offset..].as_ptr(), (bytes.len() - offset).min(i32::MAX as usize) as i32, 0) };
                if size == SOCKET_ERROR { Err(last_error()) } else { Ok(size as usize) }
            });
            match result {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "蓝牙 socket 无法继续写入")),
                Ok(size) => offset += size,
                Err(error) if error.raw_os_error() == Some(WSAEWOULDBLOCK) => { self.wait(true, Duration::from_millis(100))?; }
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "蓝牙写入超时")); }
        }
        Ok(())
    }

    fn close(&self) { Self::close(self); }
}
