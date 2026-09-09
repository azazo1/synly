use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// 本次运行是否已经做过输入服务的版本对齐, 避免重复弹 UAC.
static SERVICE_ALIGNED: AtomicBool = AtomicBool::new(false);

/// 更新就地替换可执行文件后, 正在运行的输入服务仍映射着更新前的映像, 只有重启它才会换成新版本.
///
/// 替换过 exe 的进程自己就映射着那个旧映像, 不能用 `.old` 是否被占用判断服务是否过期,
/// 此时服务只要在运行就必然还是旧版本; 由旧版本完成的更新则没有这个标记, 退回用 `.old`
/// 是否删得掉来推断. 这里在真正申请输入提权前对齐一次, 失败时继续沿用现有服务, 不影响
/// 本次提权.
fn ensure_input_service_current() {
    if SERVICE_ALIGNED.load(Ordering::Acquire) || !service_is_installed() {
        return;
    }
    let stale = if crate::update::binary_replaced_in_process() {
        synly::input::windows_input_service_running()
    } else {
        crate::update::cleanup_old_binary_backups()
    };
    if !stale || SERVICE_ALIGNED.swap(true, Ordering::AcqRel) {
        return;
    }
    tracing::info!("输入服务仍运行更新前的映像, 请求提权重启");
    match synly::input::request_windows_input_service_restart_via_uac() {
        Ok(true) => {
            tracing::info!("输入服务已跟随更新重启");
            report_backup_cleanup();
        }
        Ok(false) => tracing::warn!("用户取消了输入服务重启, 继续沿用当前服务"),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "重启输入服务失败, 继续沿用当前服务")
        }
    }
}

/// 服务重启后再清理一次旧映像备份.
///
/// 替换过 exe 的进程自己也映射着那个旧映像, 因此它退出之前备份仍然删不掉, 这属于预期,
/// 不该报成异常.
fn report_backup_cleanup() {
    if !crate::update::cleanup_old_binary_backups() {
        tracing::info!("输入服务重启后旧映像备份已清理");
    } else if crate::update::binary_replaced_in_process() {
        tracing::debug!("旧映像备份仍被本进程映射, 将在本进程退出后清理");
    } else {
        tracing::warn!("旧映像备份仍被占用, 将在下次启动时重试清理");
    }
}

pub fn request_elevation() -> Result<()> {
    ensure_input_service_current();
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
