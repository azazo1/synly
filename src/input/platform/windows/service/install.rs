use super::super::agent::security::wide;
use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, SERVICE_STATUS, StartServiceW,
};

pub(crate) const SERVICE_NAME: &str = "SynlyInputService";
pub(crate) const SERVICE_DISPLAY_NAME: &str = "Synly Input Service";
pub(crate) const SERVICE_DESCRIPTION: &str = "以 SYSTEM 权限拉起 Synly 输入代理, 支持 UAC 与锁屏输入控制";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceStatus {
    NotInstalled,
    Stopped,
    Running,
}

impl ServiceStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }
}

fn service_bin_path() -> Result<String> {
    let executable = std::env::current_exe().context("无法定位当前 Synly 可执行文件")?;
    Ok(format!(
        "\"{}\" __service",
        executable.to_string_lossy()
    ))
}

pub fn install() -> Result<()> {
    let scm = unsafe { OpenSCManagerW(std::ptr::null_mut(), std::ptr::null_mut(), SC_MANAGER_ALL_ACCESS) };
    if scm.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("打开 Windows 服务管理器失败, 安装输入服务需要管理员权限");
    }
    let scm = ServiceManagerHandle(scm);
    let bin_path = service_bin_path()?;
    let service_name = wide(SERVICE_NAME);
    let display_name = wide(SERVICE_DISPLAY_NAME);
    let bin_path_wide = wide(&bin_path);
    let service = unsafe {
        CreateServiceW(
            scm.0,
            service_name.as_ptr(),
            display_name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            bin_path_wide.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    let error = if service.is_null() {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    if service.is_null() && error != Some(ERROR_SERVICE_EXISTS as i32) {
        return Err(std::io::Error::from_raw_os_error(error.unwrap_or(0)))
            .context("创建 Synly 输入服务失败");
    }
    let service = if !service.is_null() {
        ServiceHandle(service)
    } else {
        let handle = unsafe {
            OpenServiceW(
                scm.0,
                service_name.as_ptr(),
                SERVICE_CHANGE_CONFIG | SERVICE_START | SERVICE_QUERY_STATUS,
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("打开已有 Synly 输入服务失败");
        }
        ServiceHandle(handle)
    };

    let ok = unsafe {
        ChangeServiceConfigW(
            service.0,
            SERVICE_NO_CHANGE,
            SERVICE_AUTO_START,
            SERVICE_NO_CHANGE,
            bin_path_wide.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("更新 Synly 输入服务配置失败");
    }
    let delayed = SERVICE_DELAYED_AUTO_START_INFO {
        fDelayedAutostart: 1,
    };
    let ok = unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            (&raw const delayed).cast::<c_void>(),
        )
    };
    if ok == 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "无法设置输入服务延迟自动启动");
    }
    let mut description = wide(SERVICE_DESCRIPTION);
    let description_info = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    let ok = unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const description_info).cast::<c_void>(),
        )
    };
    if ok == 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "无法设置输入服务描述");
    }

    let started = unsafe { StartServiceW(service.0, 0, std::ptr::null()) };
    if started == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(error).context("启动 Synly 输入服务失败");
        }
    }
    tracing::info!("Synly 输入服务已安装并启动");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let scm = unsafe { OpenSCManagerW(std::ptr::null_mut(), std::ptr::null_mut(), SC_MANAGER_ALL_ACCESS) };
    if scm.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("打开 Windows 服务管理器失败, 卸载输入服务需要管理员权限");
    }
    let scm = ServiceManagerHandle(scm);
    let service_name = wide(SERVICE_NAME);
    let service = unsafe {
        OpenServiceW(
            scm.0,
            service_name.as_ptr(),
            SERVICE_ALL_ACCESS,
        )
    };
    if service.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            tracing::info!("Synly 输入服务未安装, 无需卸载");
            return Ok(());
        }
        return Err(error).context("打开 Synly 输入服务失败");
    }
    let service = ServiceHandle(service);

    let mut status = SERVICE_STATUS::default();
    let stopped = unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) };
    if stopped == 0 {
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error().unwrap_or(0);
        if !(code == ERROR_SERVICE_DOES_NOT_EXIST as i32 || code == 1051) {
            tracing::warn!(error = %error, "停止 Synly 输入服务失败, 继续尝试删除");
        }
    } else {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut current = SERVICE_STATUS::default();
            if unsafe { QueryServiceStatus(service.0, &mut current) } == 0 {
                break;
            }
            if current.dwCurrentState == SERVICE_STOPPED {
                break;
            }
            if Instant::now() >= deadline {
                bail!("等待 Synly 输入服务停止超时");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    if unsafe { DeleteService(service.0) } == 0 {
        return Err(std::io::Error::last_os_error()).context("删除 Synly 输入服务失败");
    }
    tracing::info!("Synly 输入服务已卸载");
    Ok(())
}

pub fn status() -> Result<ServiceStatus> {
    let scm = unsafe { OpenSCManagerW(std::ptr::null_mut(), std::ptr::null_mut(), SC_MANAGER_CONNECT) };
    if scm.is_null() {
        return Err(std::io::Error::last_os_error()).context("打开 Windows 服务管理器失败");
    }
    let scm = ServiceManagerHandle(scm);
    let service_name = wide(SERVICE_NAME);
    let service = unsafe {
        OpenServiceW(scm.0, service_name.as_ptr(), SERVICE_QUERY_STATUS)
    };
    if service.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            return Ok(ServiceStatus::NotInstalled);
        }
        return Err(error).context("查询 Synly 输入服务状态失败");
    }
    let service = ServiceHandle(service);
    let mut status = SERVICE_STATUS::default();
    if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
        return Err(std::io::Error::last_os_error()).context("查询 Synly 输入服务状态失败");
    }
    Ok(if status.dwCurrentState == SERVICE_RUNNING {
        ServiceStatus::Running
    } else {
        ServiceStatus::Stopped
    })
}

struct ServiceManagerHandle(SC_HANDLE);

impl Drop for ServiceManagerHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
