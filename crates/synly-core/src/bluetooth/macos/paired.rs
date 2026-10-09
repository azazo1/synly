//! 在短生命周期进程中枚举系统配对设备, 避免影响查询进程的 IOBluetooth 对象缓存.

use super::{BluetoothPeer, NativePeer, check, normalize_address, peer, synly_bt_paired};
use anyhow::{Context, Result, bail};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{OnceLock, Arc};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

const MAGIC: &[u8; 8] = b"SYNLYBP1";
const MAX_BYTES: usize = 256 * 1024;
const MAX_PEERS: usize = 256;
const DEADLINE: Duration = Duration::from_secs(4);
static HELPER: OnceLock<(PathBuf, &'static str)> = OnceLock::new();

#[derive(Serialize, Deserialize)]
enum Reply {
    Peers(Vec<BluetoothPeer>),
    NativeError(i32),
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new().with_fixint_encoding().with_limit(MAX_BYTES as u64).reject_trailing_bytes()
}

/// 宿主在开始发现前注册自身的内部枚举命令, 不从配置或网络消息选择可执行程序.
pub fn register(program: PathBuf, argument: &'static str) -> Result<()> {
    if !program.is_absolute() { bail!("macOS 蓝牙枚举辅助程序必须使用绝对路径"); }
    HELPER.set((program, argument)).map_err(|_| anyhow::anyhow!("macOS 蓝牙枚举辅助命令已注册"))
}

fn write_reply(mut output: impl Write, reply: &Reply) -> Result<()> {
    output.write_all(MAGIC)?;
    codec().serialize_into(&mut output, reply).context("无法编码 macOS 蓝牙枚举结果")?;
    output.flush()?;
    Ok(())
}

/// 仅供独立辅助进程的主线程调用, 标准输出是私有管道, 不写配置或启动 GUI.
pub fn write_native(output: impl Write) -> Result<()> {
    let mut peers = [NativePeer { address: [0; 18], name: [0; 256] }; MAX_PEERS];
    let mut count = 0;
    let status = unsafe { synly_bt_paired(peers.as_mut_ptr(), peers.len(), &mut count) };
    let reply = if status != 0 {
        Reply::NativeError(status)
    } else {
        if count > peers.len() { bail!("蓝牙枚举返回了无效的设备数量"); }
        Reply::Peers(peers[..count].iter().map(peer).collect::<Result<_>>()?)
    };
    write_reply(output, &reply)
}

fn decode(bytes: &[u8]) -> Result<Vec<BluetoothPeer>> {
    if bytes.len() > MAX_BYTES { bail!("macOS 蓝牙枚举辅助进程输出过大"); }
    let body = bytes.strip_prefix(MAGIC).context("macOS 蓝牙枚举辅助进程协议不匹配")?;
    let mut peers = match codec().deserialize::<Reply>(body).context("macOS 蓝牙枚举辅助进程返回无效数据")? {
        Reply::Peers(peers) => peers,
        Reply::NativeError(status) => { check(status)?; bail!("macOS 蓝牙枚举辅助进程未返回设备列表"); }
    };
    if peers.len() > MAX_PEERS { bail!("系统已配对设备数量超出枚举上限"); }
    for peer in &mut peers {
        peer.address = normalize_address(&peer.address)?;
        // 原生名称最多 255 字节, UTF-8 有损转换最多扩张到三倍.
        if peer.name.len() > 3 * 256 { bail!("macOS 蓝牙枚举辅助进程返回过长的设备名称"); }
    }
    peers.sort_by(|a, b| a.address.cmp(&b.address));
    peers.dedup_by(|a, b| a.address == b.address);
    Ok(peers)
}

async fn collect(mut child: Child, deadline: Duration) -> Result<Vec<BluetoothPeer>> {
    let result = tokio::time::timeout(deadline, async {
        let mut stdout = child.stdout.take().context("macOS 蓝牙枚举辅助进程没有输出管道")?.take(MAX_BYTES as u64 + 1);
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        if bytes.len() > MAX_BYTES { bail!("macOS 蓝牙枚举辅助进程输出过大"); }
        let status = child.wait().await?;
        if !status.success() { bail!("macOS 蓝牙枚举辅助进程退出异常: {status}"); }
        decode(&bytes)
    }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("macOS 配对设备枚举超时")));
    if result.is_err() {
        // 错误与超时均终止并回收进程; 外层任务被取消时由 kill_on_drop 终止.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}

pub async fn enumerate() -> Result<Vec<BluetoothPeer>> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let _permit = GATE.get_or_init(|| Arc::new(Semaphore::new(1))).clone().acquire_owned().await?;
    let (program, argument) = HELPER.get().context("macOS 配对设备枚举需要宿主注册独立辅助命令")?;
    let started = Instant::now();
    let child = Command::new(program).arg(argument).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().context("无法启动 macOS 蓝牙枚举辅助进程")?;
    tracing::debug!(pid = child.id(), "已启动 macOS 配对设备枚举辅助进程");
    let result = collect(child, DEADLINE).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(peers) => tracing::info!(count = peers.len(), elapsed_ms, "macOS 配对设备枚举结束, 已隔离系统对象缓存"),
        Err(error) => tracing::warn!(elapsed_ms, %error, "macOS 配对设备枚举失败"),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(reply: &Reply) -> Vec<u8> {
        let mut bytes = Vec::new(); write_reply(&mut bytes, reply).unwrap(); bytes
    }

    fn spawn(command: &mut Command) -> Child {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap()
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("/bin/kill").args(["-0", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success()
    }

    #[test]
    fn rejects_invalid_or_unbounded_helper_frames() {
        let peer = BluetoothPeer { address: "01:02:03:04:05:06".into(), name: "fixture".into() };
        let bytes = frame(&Reply::Peers(vec![peer.clone(), peer.clone()]));
        assert_eq!(decode(&bytes).unwrap(), vec![peer.clone()]);
        let mut trailing = bytes.clone(); trailing.push(0);
        for invalid in [&b"invalid"[..], &bytes[..bytes.len() - 1], trailing.as_slice()] { assert!(decode(invalid).is_err()); }
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_err());
        assert!(decode(&frame(&Reply::Peers(vec![peer; MAX_PEERS + 1]))).is_err());
        assert!(decode(&frame(&Reply::NativeError(-2))).is_err());
    }

    #[tokio::test]
    async fn collects_a_real_process_pipe_and_rejects_failed_exit() {
        let peers = vec![BluetoothPeer { address: "01:02:03:04:05:06".into(), name: "fixture".into() }];
        let escaped = frame(&Reply::Peers(peers.clone())).iter().map(|byte| format!("\\{byte:03o}")).collect::<String>();
        let child = spawn(Command::new("/bin/sh").args(["-c", &format!("printf '{escaped}'")]));
        assert_eq!(collect(child, Duration::from_secs(2)).await.unwrap(), peers);
        let child = spawn(Command::new("/bin/sh").args(["-c", "exit 9"]));
        assert!(collect(child, Duration::from_secs(2)).await.is_err());
    }

    #[tokio::test]
    async fn oversized_output_and_deadline_kill_and_reap_the_helper() {
        let child = spawn(Command::new("/bin/dd").args(["if=/dev/zero", &format!("bs={}", MAX_BYTES + 1), "count=1"]));
        let pid = child.id().unwrap();
        assert!(collect(child, Duration::from_secs(2)).await.is_err());
        assert!(!alive(pid));
        let child = spawn(Command::new("/bin/sleep").arg("60"));
        let pid = child.id().unwrap();
        assert!(collect(child, Duration::from_millis(25)).await.is_err());
        assert!(!alive(pid));
    }

    #[tokio::test]
    async fn cancelling_the_caller_terminates_the_helper() {
        let child = spawn(Command::new("/bin/sleep").arg("60"));
        let pid = child.id().unwrap();
        let task = tokio::spawn(collect(child, Duration::from_secs(30)));
        tokio::task::yield_now().await;
        task.abort(); assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            while alive(pid) { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
    }
}
