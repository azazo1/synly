use super::{ServiceStatus, status};
use super::protocol::{
    SERVICE_CONNECT_TIMEOUT, SERVICE_PIPE_NAME, ServiceRequest, ServiceResponse, read_response,
    write_request,
};
use super::super::agent::pipe::{NativePipe, PipeDirection};
use super::super::agent::security::wide;
use anyhow::{Context, Result, bail};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

const ELEVATED_ACTION_TIMEOUT_MS: u32 = 30_000;

static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static PATH_REPAIR_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static SERVICE_SEEN_INSTALLED: AtomicBool = AtomicBool::new(false);
static SERVICE_MANUALLY_UNINSTALLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_attempted() -> bool {
    INSTALL_ATTEMPTED.load(Ordering::Acquire)
}

pub fn mark_install_attempted() {
    INSTALL_ATTEMPTED.store(true, Ordering::Release);
}

pub(crate) fn path_repair_attempted() -> bool {
    PATH_REPAIR_ATTEMPTED.load(Ordering::Acquire)
}

pub(crate) fn mark_path_repair_attempted() {
    PATH_REPAIR_ATTEMPTED.store(true, Ordering::Release);
}

pub(crate) fn manual_uninstall_requested() -> bool {
    SERVICE_MANUALLY_UNINSTALLED.load(Ordering::Acquire)
}

pub(crate) fn mark_manual_uninstall() {
    mark_install_attempted();
    SERVICE_MANUALLY_UNINSTALLED.store(true, Ordering::Release);
}

pub(crate) fn mark_service_seen_installed() {
    SERVICE_SEEN_INSTALLED.store(true, Ordering::Release);
}

fn note_service_not_installed() {
    if SERVICE_SEEN_INSTALLED.load(Ordering::Acquire) {
        SERVICE_MANUALLY_UNINSTALLED.store(true, Ordering::Release);
    }
}

pub fn is_installed() -> bool {
    match status() {
        Ok(ServiceStatus::Stopped | ServiceStatus::Running) => {
            mark_service_seen_installed();
            true
        }
        Ok(ServiceStatus::NotInstalled) => {
            note_service_not_installed();
            false
        }
        Err(_) => false,
    }
}

pub(crate) fn is_available() -> bool {
    match status() {
        Ok(ServiceStatus::Running) => {
            mark_service_seen_installed();
            true
        }
        Ok(ServiceStatus::Stopped) => {
            mark_service_seen_installed();
            false
        }
        Ok(ServiceStatus::NotInstalled) => {
            note_service_not_installed();
            false
        }
        Err(_) => false,
    }
}

pub(crate) fn spawn_agent(
    command_pipe: &str,
    event_pipe: &str,
    token: &str,
    _parent_pid: u32,
) -> Result<()> {
    let mut pipe = NativePipe::connect_client(
        SERVICE_PIPE_NAME,
        PipeDirection::Duplex,
        SERVICE_CONNECT_TIMEOUT,
    )
    .context("连接 Synly 输入服务失败")?;
    write_request(
        &mut pipe,
        &ServiceRequest::SpawnInputAgent {
            command_pipe: command_pipe.to_string(),
            event_pipe: event_pipe.to_string(),
            token: token.to_string(),
        },
    )?;
    match read_response(&mut pipe)? {
        ServiceResponse::Ok => Ok(()),
        ServiceResponse::Err(message) => bail!("SYSTEM 输入服务拒绝请求: {message}"),
    }
}

pub(crate) fn install_via_uac() -> Result<bool> {
    let installed = run_elevated("service install")?;
    if installed {
        mark_service_seen_installed();
    }
    Ok(installed)
}

pub fn uninstall_via_uac() -> Result<bool> {
    let uninstalled = run_elevated("service uninstall")?;
    if uninstalled {
        mark_manual_uninstall();
    }
    Ok(uninstalled)
}

fn run_elevated(parameters: &str) -> Result<bool> {
    let executable = std::env::current_exe().context("无法定位当前 Synly 可执行文件")?;
    let verb = wide("runas");
    let file = wide(&executable.to_string_lossy());
    let args = wide(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.as_ptr().cast_mut(),
        lpFile: file.as_ptr().cast_mut(),
        lpParameters: args.as_ptr().cast_mut(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Ok(false);
        }
        return Err(error).context("请求管理员权限执行失败");
    }
    let wait = unsafe { WaitForSingleObject(info.hProcess, ELEVATED_ACTION_TIMEOUT_MS) };
    if wait != WAIT_OBJECT_0 {
        unsafe {
            CloseHandle(info.hProcess);
        }
        bail!("等待提权命令完成超时");
    }
    let mut exit_code = 0u32;
    if unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) } == 0 {
        unsafe {
            CloseHandle(info.hProcess);
        }
        return Err(std::io::Error::last_os_error()).context("读取提权命令退出码失败");
    }
    unsafe {
        CloseHandle(info.hProcess);
    }
    if exit_code == 0 {
        Ok(true)
    } else {
        bail!("提权命令失败, 退出码 {exit_code}")
    }
}
