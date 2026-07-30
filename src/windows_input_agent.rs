use anyhow::{Context, Result};

pub fn request_elevation() -> Result<()> {
    synly::input::request_windows_input_elevation()
}

pub fn request_startup_elevation() -> Result<()> {
    tracing::info!("配置要求启动 Windows 输入管理员代理");
    request_elevation()
        .inspect_err(|error| {
            tracing::error!(error = %error, "Windows 输入管理员代理启动失败");
        })
        .context("无法完成 Windows 输入启动提权")?;
    tracing::info!("Windows 输入管理员代理已在启动阶段就绪");
    Ok(())
}
