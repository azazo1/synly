mod agent;
mod cursor_capture;
mod native;

use super::{CaptureContext, InputBackend};
use crate::input::InputMode;
use anyhow::{Context, Result, bail};
use std::sync::Arc;

pub use agent::{request_elevation, run_agent};

pub fn init_agent_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    agent::init_tracing()
}

pub(in crate::input) fn agent_ready() -> bool {
    agent::is_ready()
}

pub(super) fn ensure_permissions(_mode: InputMode) -> Result<()> {
    Ok(())
}

pub(super) fn start(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    let elevation_requested = agent::elevation_requested();
    if agent::is_ready() {
        match agent::start_client(context.clone()) {
            Ok(backend) => {
                tracing::info!("Windows 输入控制已使用管理员代理启动");
                return Ok(backend);
            }
            Err(error) => {
                if !allow_native_fallback(elevation_requested) {
                    return Err(error).context("Windows 管理员输入代理已失效");
                }
                tracing::warn!(error = %error, "Windows 管理员输入代理不可用, 回退到普通权限输入控制");
            }
        }
    } else if !allow_native_fallback(elevation_requested) {
        bail!("Windows 管理员输入代理已失效, 当前输入会话不会静默降级到普通权限");
    }
    tracing::info!("Windows 输入控制已使用普通权限启动");
    native::start(context)
}

fn allow_native_fallback(elevation_requested: bool) -> bool {
    !elevation_requested
}
