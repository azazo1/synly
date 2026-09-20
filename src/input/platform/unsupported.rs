use super::{CaptureContext, InputBackend};
use anyhow::{Result, bail};
use std::sync::Arc;

pub fn start(_context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    bail!("鼠标键盘同步目前只支持 macOS 和 Windows")
}
