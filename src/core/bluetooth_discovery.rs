//! 手动蓝牙发现只查询系统已配对设备, 不修改配对或应用信任.
use super::model::BluetoothPeerView;
use synly_core::bluetooth::{self, BluetoothAvailability, BluetoothPeer};
use std::{future::Future, pin::Pin, time::Duration};
use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

const MAX_PEERS: usize = 16;
const SYSTEM_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(12);
type Work<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
trait Backend: Send + Sync {
    fn availability(&self) -> Work<'_, BluetoothAvailability>;
    fn paired(&self) -> Work<'_, Vec<BluetoothPeer>>;
    fn service<'a>(&'a self, peer: &'a BluetoothPeer) -> Work<'a, bool>;
}
struct System;
impl Backend for System {
    fn availability(&self) -> Work<'_, BluetoothAvailability> { Box::pin(bluetooth::availability()) }
    fn paired(&self) -> Work<'_, Vec<BluetoothPeer>> { Box::pin(bluetooth::paired_devices()) }
    fn service<'a>(&'a self, peer: &'a BluetoothPeer) -> Work<'a, bool> { Box::pin(async move { Ok(bluetooth::query_service(peer).await?.is_some()) }) }
}
pub(super) async fn browse(cancel: CancellationToken, changed: impl FnMut(BluetoothPeerView) + Send) -> Result<String> { run(&System, cancel, changed).await }
async fn run(backend: &impl Backend, cancel: CancellationToken, mut changed: impl FnMut(BluetoothPeerView) + Send) -> Result<String> {
    tokio::select! { biased; _ = cancel.cancelled() => anyhow::bail!("蓝牙刷新已取消"), result = async {
        let availability = tokio::time::timeout(SYSTEM_TIMEOUT, backend.availability()).await.context("读取系统蓝牙状态超时")??;
        match availability {
            BluetoothAvailability::Disabled => return Ok("系统蓝牙已关闭, 请打开系统蓝牙设置".to_owned()),
            BluetoothAvailability::PermissionDenied => return Ok("系统拒绝蓝牙权限, 请在系统设置授权后刷新".to_owned()),
            BluetoothAvailability::Unsupported => return Ok("当前系统或适配器不支持经典蓝牙 RFCOMM".to_owned()),
            BluetoothAvailability::Available => {}
        }
        let paired = tokio::time::timeout(SYSTEM_TIMEOUT, backend.paired()).await.context("读取系统已配对设备超时")??;
        let mut unique = std::collections::BTreeMap::new();
        for peer in paired {
            if let Ok(address) = bluetooth::normalize_address(&peer.address) { unique.entry(address.clone()).or_insert(BluetoothPeer { address, name: peer.name }); }
        }
        let total = unique.len(); let mut connected = 0;
        for peer in unique.into_values().take(MAX_PEERS) {
            let mut row = BluetoothPeerView { address: peer.address.clone(), display_name: if peer.name.is_empty() { peer.address.clone() } else { peer.name.clone() }, connectable: false, detail: "查询 Synly 服务中".to_owned() };
            changed(row.clone());
            row.detail = match tokio::time::timeout(QUERY_TIMEOUT, backend.service(&peer)).await {
                Ok(Ok(true)) => { row.connectable = true; connected += 1; "Synly 服务可用, 连接后验证应用身份".to_owned() },
                Ok(Ok(false)) => "未找到 Synly 服务, 请在对端开启蓝牙接入".to_owned(),
                Ok(Err(error)) => { tracing::debug!(address = %peer.address, error = %error, "已配对设备服务查询失败"); "服务查询失败, 设备可能离线或权限受限".to_owned() },
                Err(_) => "服务查询超时, 可稍后刷新".to_owned(),
            };
            changed(row);
        }
        tracing::info!(paired = total, connectable = connected, "手动蓝牙设备刷新完成");
        Ok(if total > MAX_PEERS { format!("已查询前 {MAX_PEERS}/{total} 个已配对设备, {connected} 个 Synly 服务可用") }
            else if total == 0 { "没有系统已配对设备, 请先在系统蓝牙设置中配对".to_owned() }
            else { format!("已查询 {total} 个已配对设备, {connected} 个 Synly 服务可用") })
    } => result }
}

pub(super) fn open_settings() -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut child = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.BluetoothSettings").spawn().context("无法启动系统设置")?;
    #[cfg(windows)]
    let mut child = std::process::Command::new("explorer.exe").arg("ms-settings:bluetooth").spawn().context("无法启动系统设置")?;
    #[cfg(any(target_os = "macos", windows))]
    { if !child.wait()?.success() { anyhow::bail!("系统蓝牙设置启动失败"); } tracing::info!("已打开系统蓝牙设置"); Ok(()) }
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("当前桌面平台不支持系统蓝牙设置快捷入口")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fake { calls: AtomicUsize, block: bool }
    impl Backend for Fake {
        fn availability(&self) -> Work<'_, BluetoothAvailability> { Box::pin(async { Ok(BluetoothAvailability::Available) }) }
        fn paired(&self) -> Work<'_, Vec<BluetoothPeer>> { Box::pin(async {
            let mut peers = (0..30).map(|i| BluetoothPeer { address: format!("AA:BB:CC:DD:EE:{i:02X}"), name: "同名设备".to_owned() }).collect::<Vec<_>>();
            peers.push(peers[0].clone()); peers.push(BluetoothPeer { address: "无效".to_owned(), name: "无效".to_owned() }); Ok(peers)
        }) }
        fn service<'a>(&'a self, peer: &'a BluetoothPeer) -> Work<'a, bool> { Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.block { std::future::pending::<()>().await; }
            if peer.address.ends_with("01") { anyhow::bail!("设备离线"); } Ok(peer.address.ends_with("00"))
        }) }
    }
    #[tokio::test]
    async fn discovery_caps_service_work_and_never_merges_same_names_or_trusts_addresses() {
        let fake = Fake { calls: AtomicUsize::new(0), block: false }; let mut rows = std::collections::BTreeMap::new();
        run(&fake, CancellationToken::new(), |row| { rows.insert(row.address.clone(), row); }).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::Relaxed), MAX_PEERS); assert_eq!(rows.len(), MAX_PEERS);
        assert_eq!(rows.values().filter(|row| row.connectable).count(), 1);
        assert!(rows["AA:BB:CC:DD:EE:00"].connectable); assert!(!rows["AA:BB:CC:DD:EE:01"].connectable);
    }
    #[tokio::test]
    async fn cancellation_interrupts_inflight_query_without_late_rows() {
        let fake = Fake { calls: AtomicUsize::new(0), block: true }; let cancel = CancellationToken::new(); let mut rows = 0;
        let query = run(&fake, cancel.clone(), |_| { rows += 1; });
        let stop = async { tokio::task::yield_now().await; cancel.cancel(); };
        let (result, ()) = tokio::join!(query, stop); assert!(result.is_err()); assert_eq!(rows, 1);
    }
}
