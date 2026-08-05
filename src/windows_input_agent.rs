use anyhow::{Context, Result};

pub fn request_elevation() -> Result<()> {
    synly::input::request_windows_input_elevation()
}

pub fn request_elevation_for_auto_recovery() -> Result<()> {
    synly::input::request_windows_input_elevation_for_auto_recovery()
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

pub fn service_is_installed() -> bool {
    synly::input::windows_input_service_installed()
}

pub fn request_service_uninstall_via_uac() -> Result<()> {
    match synly::input::request_windows_input_service_uninstall_via_uac() {
        Ok(true) => {
            synly::input::mark_windows_input_service_install_attempted();
            tracing::info!("Synly 输入服务已通过提权命令卸载");
            Ok(())
        }
        Ok(false) => anyhow::bail!("用户取消了输入服务卸载"),
        Err(error) => Err(error),
    }
}
