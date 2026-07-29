use anyhow::{Context, Result, bail};
use slint::{ComponentHandle, Weak};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

use super::AppWindow;

const ACTIVATE_REQUEST: &[u8] = b"SYNLY-ACTIVATE\n";
const ACTIVATE_RESPONSE: &[u8] = b"SYNLY-OK\n";

pub enum SingleInstance {
    Primary(TcpListener),
    ActivatedExisting,
}

pub struct SingleInstanceGuard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SingleInstance {
    pub fn acquire(device_id: Uuid) -> Result<Self> {
        let address = activation_address(device_id);
        match TcpListener::bind(address) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .context("failed to configure single instance listener")?;
                Ok(Self::Primary(listener))
            }
            Err(error) if error.kind() == ErrorKind::AddrInUse => {
                activate_existing(address)?;
                Ok(Self::ActivatedExisting)
            }
            Err(error) => Err(error).context("failed to create single instance listener"),
        }
    }
}

impl SingleInstanceGuard {
    pub fn start(listener: TcpListener, window: Weak<AppWindow>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("synly-single-instance".to_string())
            .spawn(move || activation_loop(listener, window, thread_stop))
            .context("failed to start single instance activation thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn activation_address(device_id: Uuid) -> SocketAddrV4 {
    let bytes = device_id.as_bytes();
    let seed = u16::from_be_bytes([bytes[0], bytes[1]]);
    let port = 49_152 + seed % 10_000;
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

fn activate_existing(address: SocketAddrV4) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(&address.into(), Duration::from_secs(1))
        .context("single instance port is occupied but the existing Synly process did not respond")?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(ACTIVATE_REQUEST)?;
    stream.flush()?;
    let mut response = [0u8; ACTIVATE_RESPONSE.len()];
    stream.read_exact(&mut response)?;
    if response != ACTIVATE_RESPONSE {
        bail!("single instance port returned an invalid activation response");
    }
    Ok(())
}

fn activation_loop(listener: TcpListener, window: Weak<AppWindow>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, address)) => {
                if handle_activation(&mut stream) {
                    tracing::info!(%address, "收到重复启动激活请求");
                    let window = window.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            let _ = window.show();
                        }
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                tracing::warn!(error = %error, "单实例激活监听失败");
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn handle_activation(stream: &mut TcpStream) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let mut request = [0u8; ACTIVATE_REQUEST.len()];
    if stream.read_exact(&mut request).is_err() || request != ACTIVATE_REQUEST {
        return false;
    }
    stream.write_all(ACTIVATE_RESPONSE).is_ok() && stream.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::activation_address;
    use uuid::Uuid;

    #[test]
    fn activation_port_is_stable_and_unprivileged() {
        let device_id = Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap();
        let address = activation_address(device_id);
        assert_eq!(address.ip(), &std::net::Ipv4Addr::LOCALHOST);
        assert!((49_152..59_152).contains(&address.port()));
        assert_eq!(address, activation_address(device_id));
    }
}
