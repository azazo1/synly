use super::{CaptureContext, InputBackend};
use crate::input::InputMode;
use anyhow::{Result, bail};
use std::sync::Arc;

pub fn start(_context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    bail!("鼠标键盘同步目前只支持 macOS 和 Windows")
}

pub fn ensure_permissions(_mode: InputMode) -> Result<()> {
    bail!("鼠标键盘同步目前只支持 macOS 和 Windows")
}
