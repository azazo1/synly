mod capture;
mod crypto;
mod queue;
mod receive;
mod render;
mod sdl_policy;
mod send;
mod workers;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod channel_tests;

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const AUDIO_IO_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannelDirection {
    HostToClient,
    ClientToHost,
}

impl AudioChannelDirection {
    fn as_label(self) -> &'static [u8] {
        match self {
            Self::HostToClient => b"host-to-client",
            Self::ClientToHost => b"client-to-host",
        }
    }
}

pub struct AudioTaskHandle {
    stop: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl AudioTaskHandle {
    pub async fn stop(mut self) -> Result<()> {
        self.stop.cancel();
        self.task.take().context("音频任务句柄丢失")?
            .await.context("音频监督任务异常退出")?
    }
}

impl Drop for AudioTaskHandle {
    fn drop(&mut self) {
        // 外层监督任务继续回收全部工作线程, 不直接 abort 阻塞音频操作.
        self.stop.cancel();
    }
}

pub fn bind_and_spawn_receiver(
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    expected_peer_ip: IpAddr,
) -> Result<(AudioTaskHandle, u16, [u8; 32])> {
    let (socket, channel_secret, channel_id) = prepare_receiver(master_secret, expected_peer_ip)?;
    let local_port = socket.local_addr()?.port();
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let task = tokio::spawn(receive::run(socket, task_stop, channel_secret, direction, expected_peer_ip));
    Ok((AudioTaskHandle { stop, task: Some(task) }, local_port, channel_id))
}

pub fn spawn_sender(
    master_secret: [u8; 32],
    channel_id: [u8; 32],
    direction: AudioChannelDirection,
    remote_addr: SocketAddr,
) -> Result<AudioTaskHandle> {
    let channel_secret = crypto::derive_channel_secret(master_secret, channel_id)?;
    let socket = bind_socket(remote_addr.ip())?;
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let task = tokio::spawn(async move {
        socket.connect(remote_addr).await.context("连接音频 UDP 接收端失败")?;
        send::run(socket, task_stop, channel_secret, direction).await
    });
    Ok(AudioTaskHandle { stop, task: Some(task) })
}

fn prepare_receiver(master_secret: [u8; 32], peer: IpAddr) -> Result<(UdpSocket, [u8; 32], [u8; 32])> {
    let mut channel_id = [0; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut channel_id)
        .map_err(|_| anyhow::anyhow!("生成音频通道随机标识失败"))?;
    let channel_secret = crypto::derive_channel_secret(master_secret, channel_id)?;
    Ok((bind_socket(peer)?, channel_secret, channel_id))
}

fn bind_socket(peer: IpAddr) -> Result<UdpSocket> {
    let address = match peer {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    // 同步入口仅绑定端口, 所有网络收发都交给 Tokio 非阻塞驱动.
    let socket = std::net::UdpSocket::bind(SocketAddr::new(address, 0))
        .context("绑定音频 UDP 端口失败")?;
    socket.set_nonblocking(true).context("设置音频 UDP 非阻塞模式失败")?;
    UdpSocket::from_std(socket).context("注册音频 UDP 异步驱动失败")
}
