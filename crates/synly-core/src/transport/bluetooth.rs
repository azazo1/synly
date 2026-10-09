//! 认证后的业务承载统一进入三通道复用, LAN 与蓝牙使用相同的独立反压窗口.

use super::{generation, mux, stream::ByteStream};
use tokio::sync::watch;

pub struct BluetoothChannels {
    pub clipboard: Option<ByteStream>,
    pub clipboard_route: Option<generation::GenerationLane>,
    _clipboard: Option<generation::GenerationGuard>,
    clipboard_failure: Option<watch::Receiver<Option<String>>>,
    pub input: generation::GenerationLane,
    _mux: mux::MuxGuard,
    _input: generation::GenerationGuard,
    mux_failure: watch::Receiver<Option<String>>,
    input_failure: watch::Receiver<Option<String>>,
}
pub fn open(authenticated_stream: ByteStream) -> (ByteStream, BluetoothChannels) {
    let (channels, mux) = mux::open(authenticated_stream);
    let (input, input_guard) = generation::open(channels.input);
    let mux_failure = mux.failure();
    let input_failure = input_guard.failure();
    (channels.control, BluetoothChannels { clipboard: Some(channels.clipboard), clipboard_route: None, _clipboard: None, clipboard_failure: None, input, _mux: mux, _input: input_guard, mux_failure, input_failure })
}
impl BluetoothChannels {
    pub fn enable_clipboard_routes(&mut self) -> anyhow::Result<generation::GenerationLane> {
        if let Some(lane) = &self.clipboard_route { return Ok(lane.clone()); }
        let stream = self.clipboard.take().ok_or_else(|| anyhow::anyhow!("剪贴板原始子流已经被使用"))?;
        let (lane, guard) = generation::open(stream); self.clipboard_failure = Some(guard.failure()); self._clipboard = Some(guard); self.clipboard_route = Some(lane.clone()); Ok(lane)
    }
    /// 任一子流协议错误关闭此承载, 让调用方释放输入状态而不是静默降级.
    pub async fn failed(&mut self) -> String {
        loop {
            if let Some(error) = self.mux_failure.borrow().clone() { return error; }
            if let Some(error) = self.input_failure.borrow().clone() { return error; }
            if let Some(error) = self.clipboard_failure.as_ref().and_then(|failure| failure.borrow().clone()) { return error; }
            let changed = tokio::select! {
                changed = self.mux_failure.changed() => changed, changed = self.input_failure.changed() => changed,
                changed = async { match &mut self.clipboard_failure { Some(failure) => failure.changed().await, None => std::future::pending().await } } => changed,
            };
            if changed.is_err() { return "主承载复用任务已经结束".to_owned(); }
        }
    }
}
pub async fn wait_failure(channels: &mut Option<BluetoothChannels>) -> String {
    match channels { Some(channels) => channels.failed().await, None => std::future::pending().await }
}
