use super::install::SERVICE_NAME;
use super::protocol::{
    SERVICE_CONNECT_TIMEOUT, SERVICE_PIPE_NAME, ServiceRequest, ServiceResponse, read_request,
    write_response,
};
use super::super::agent::pipe::{NativePipe, PipeDirection};
use super::super::agent::protocol::is_timeout_error;
use super::super::agent::security::{
    process_image_path, process_session_id, token_user_sid_string, wide,
};
use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CALL_NOT_IMPLEMENTED, HANDLE, LUID, NO_ERROR, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, LookupPrivilegeValueW, PSECURITY_DESCRIPTOR,
    SE_ASSIGNPRIMARYTOKEN_NAME, SE_INCREASE_QUOTA_NAME, SE_PRIVILEGE_ENABLED, SE_TCB_NAME,
    SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
    LUID_AND_ATTRIBUTES, TokenPrimary, TokenSessionId,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION,
    STARTUPINFOW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WTS_CONSOLE_CONNECT;

const SERVICE_STOP_POLL_MS: u32 = 3_000;
const CREATION_FLAGS: u32 = 0x0800_0000 | 0x0000_0400; // CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT

static SERVICE_STATUS_HANDLE: OnceLock<AtomicUsize> = OnceLock::new();
static STOP_EVENT: OnceLock<AtomicUsize> = OnceLock::new();
static AGENT_PROCESSES: OnceLock<Mutex<Vec<(usize, usize)>>> = OnceLock::new();
static SERVICE_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

struct ServicePipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl ServicePipeSecurity {
    fn for_user(user_sid: &str) -> Result<Self> {
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})"));
        let mut descriptor = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("构建 Synly 输入服务管道 DACL 失败");
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
}

impl Drop for ServicePipeSecurity {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.descriptor.cast());
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn run_service() -> Result<()> {
    let service_name = wide(SERVICE_NAME);

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
        let result = service_main_inner();
        let _ = SERVICE_RESULT.set(result.map_err(|error| format!("{error:#}")));
    }

    let table = [SERVICE_TABLE_ENTRYW {
        lpServiceName: service_name.as_ptr().cast_mut(),
        lpServiceProc: Some(service_main),
    }];
    let dispatched = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if dispatched == 0 {
        return Err(std::io::Error::last_os_error())
            .context("注册 Synly 输入服务分发失败");
    }
    match SERVICE_RESULT.get() {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => bail!("Synly 输入服务运行失败: {error}"),
        None => Ok(()),
    }
}

fn service_main_inner() -> Result<()> {
    let status_handle = unsafe {
        RegisterServiceCtrlHandlerExW(
            wide(SERVICE_NAME).as_ptr(),
            Some(service_ctrl_handler),
            std::ptr::null(),
        )
    };
    if status_handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("注册 Synly 输入服务控制处理失败");
    }
    let _ = SERVICE_STATUS_HANDLE.set(AtomicUsize::new(status_handle as usize));
    set_service_status(status_handle, SERVICE_START_PENDING, 0);

    let stop_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop_event.is_null() {
        set_service_status(status_handle, SERVICE_STOPPED, 1);
        return Err(std::io::Error::last_os_error()).context("创建 Synly 输入服务停止事件失败");
    }
    let _ = STOP_EVENT.set(AtomicUsize::new(stop_event as usize));
    let _ = AGENT_PROCESSES.set(Mutex::new(Vec::new()));
    set_service_status(status_handle, SERVICE_RUNNING, 0);
    tracing::info!("Synly 输入服务已开始运行");
    if let Err(error) = enable_required_privileges() {
        tracing::warn!(error = %format!("{error:#}"), "启用输入服务所需特权失败");
    }

    let result = service_accept_loop();
    close_all_agent_processes();
    unsafe {
        CloseHandle(stop_event);
    }
    set_service_status(status_handle, SERVICE_STOPPED, 0);
    result
}

unsafe extern "system" fn service_ctrl_handler(
    control: u32,
    event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        SERVICE_CONTROL_STOP => {
            if let Some(handle) = SERVICE_STATUS_HANDLE.get() {
                set_service_status(
                    handle.load(Ordering::Acquire) as SERVICE_STATUS_HANDLE,
                    SERVICE_STOP_PENDING,
                    0,
                );
            }
            if let Some(stop) = STOP_EVENT.get() {
                unsafe {
                    windows_sys::Win32::System::Threading::SetEvent(
                        stop.load(Ordering::Acquire) as HANDLE,
                    );
                }
            }
            NO_ERROR
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            if event_type == WTS_CONSOLE_CONNECT {
                tracing::info!("SYSTEM 输入服务检测到控制台会话变化");
            }
            NO_ERROR
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

fn set_service_status(handle: SERVICE_STATUS_HANDLE, state: u32, checkpoint: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: 0,
    };
    unsafe {
        SetServiceStatus(handle, &status);
    }
}

fn service_accept_loop() -> Result<()> {
    loop {
        if stop_requested() {
            break;
        }
        reap_exited_agent_processes();
        let Some(session_id) = active_console_session() else {
            sleep_service();
            continue;
        };
        let user_token = match console_user_token(session_id) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(error = %error, "无法取得控制台用户令牌, 稍后重试");
                sleep_service();
                continue;
            }
        };
        let _user_token = OwnedHandle(user_token);
        let user_sid = match token_user_sid_string(user_token) {
            Ok(sid) => sid,
            Err(error) => {
                tracing::warn!(error = %error, "无法取得控制台用户 SID, 稍后重试");
                sleep_service();
                continue;
            }
        };
        let security = match ServicePipeSecurity::for_user(&user_sid) {
            Ok(security) => security,
            Err(error) => {
                tracing::warn!(error = %error, "构建输入服务管道安全属性失败, 稍后重试");
                sleep_service();
                continue;
            }
        };
        let mut pipe = match NativePipe::create_server(SERVICE_PIPE_NAME, PipeDirection::Duplex, &security.attributes) {
            Ok(pipe) => pipe,
            Err(error) => {
                tracing::warn!(error = %error, "创建输入服务管道失败, 稍后重试");
                sleep_service();
                continue;
            }
        };
        match pipe.connect_server(SERVICE_CONNECT_TIMEOUT) {
            Ok(()) => {}
            Err(error) if is_timeout_error(&error) || stop_requested() => continue,
            Err(error) => {
                tracing::warn!(error = %error, "输入服务管道等待连接失败, 稍后重试");
                continue;
            }
        }
        if let Err(error) = handle_client(&mut pipe, session_id, user_token) {
            tracing::warn!(error = %error, "处理输入服务请求失败");
        }
    }
    Ok(())
}

fn handle_client(
    pipe: &mut NativePipe,
    console_session_id: u32,
    user_token: HANDLE,
) -> Result<()> {
    let client_pid = match validate_client(pipe, console_session_id) {
        Ok(client_pid) => client_pid,
        Err(error) => {
            let detail = format!("{error:#}");
            tracing::warn!(error = %detail, "输入服务拒绝请求方");
            return write_response(pipe, &ServiceResponse::Err(detail));
        }
    };
    let request = match read_request(pipe) {
        Ok(request) => request,
        Err(error) => {
            let detail = format!("{error:#}");
            tracing::warn!(error = %detail, "输入服务读取请求失败");
            return write_response(pipe, &ServiceResponse::Err(detail));
        }
    };
    tracing::info!(request = request.name(), client_pid, "输入服务收到请求");
        let result = match &request {
            ServiceRequest::SpawnInputAgent {
                command_pipe,
                event_pipe,
                token,
            } => spawn_system_input_agent(
                console_session_id,
                user_token,
                command_pipe,
                event_pipe,
                token,
            client_pid,
        ),
    };
    match result {
        Ok(()) => {
            tracing::info!(client_pid, "输入服务已拉起 SYSTEM 输入代理");
            write_response(pipe, &ServiceResponse::Ok)
        }
        Err(error) => {
            let detail = format!("{error:#}");
            tracing::error!(error = %detail, client_pid, "输入服务拉起 SYSTEM 输入代理失败");
            write_response(pipe, &ServiceResponse::Err(detail))
        }
    }
}

/// CreateProcessAsUserW 需要调用进程持有并启用 SeAssignPrimaryTokenPrivilege 与
/// SeIncreaseQuotaPrivilege, 设置 TokenSessionId 需要 SeTcbPrivilege. LocalSystem
/// 默认拥有这些特权, 这里显式启用一次, 避免个别环境默认禁用导致拉起失败.
fn enable_required_privileges() -> Result<()> {
    let mut token = std::ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("打开输入服务令牌以启用特权失败");
    }
    let _token = OwnedHandle(token);
    for name in [
        SE_ASSIGNPRIMARYTOKEN_NAME,
        SE_INCREASE_QUOTA_NAME,
        SE_TCB_NAME,
    ] {
        let mut luid = LUID::default();
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), name, &mut luid) } == 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "查询服务特权 LUID 失败");
            continue;
        }
        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &mut privileges,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            tracing::warn!(error = %std::io::Error::last_os_error(), "启用服务特权失败");
        }
    }
    Ok(())
}

fn validate_client(pipe: &NativePipe, console_session_id: u32) -> Result<u32> {
    let mut client_pid = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe.raw_handle(), &mut client_pid) } == 0 {
        bail!("无法取得输入服务请求方 PID");
    }
    let client_path = process_image_path(client_pid)?;
    let service_path = std::env::current_exe()?;
    if normalize_path(&client_path) != normalize_path(&service_path) {
        bail!("输入服务请求方映像路径校验失败");
    }
    if process_session_id(client_pid)? != console_session_id {
        bail!("输入服务请求方不在当前控制台会话");
    }
    Ok(client_pid)
}

fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn spawn_system_input_agent(
    console_session_id: u32,
    user_token: HANDLE,
    command_pipe: &str,
    event_pipe: &str,
    token: &str,
    parent_pid: u32,
) -> Result<()> {
    validate_pipe_argument(command_pipe, "command")?;
    validate_pipe_argument(event_pipe, "event")?;
    validate_token_argument(token)?;

    let session_token = duplicate_system_token_for_session(console_session_id)?;
    let _session_token = OwnedHandle(session_token);

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error()).context("创建输入代理 job 对象失败");
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        unsafe {
            CloseHandle(job);
        }
        return Err(std::io::Error::last_os_error()).context("配置输入代理 job 对象失败");
    }

    let mut environment = std::ptr::null_mut();
    if unsafe { CreateEnvironmentBlock(&mut environment, user_token, 0) } == 0 {
        unsafe {
            CloseHandle(job);
        }
        return Err(std::io::Error::last_os_error()).context("创建控制台用户环境块失败");
    }
    let environment = EnvironmentGuard(environment);

    let executable = std::env::current_exe().context("无法定位输入代理可执行文件")?;
    let working_dir = executable
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| executable.clone());
    let command_line = format!(
        "\"{}\" __input-agent --command-pipe \"{command_pipe}\" --event-pipe \"{event_pipe}\" --token \"{token}\" --parent-pid {parent_pid}",
        executable.to_string_lossy()
    );
    let executable_wide = wide(&executable.to_string_lossy());
    let mut command_line_wide = wide(&command_line);
    let working_dir_wide = wide(&working_dir.to_string_lossy());
    let mut desktop = wide("winsta0\\default");
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessAsUserW(
            session_token,
            executable_wide.as_ptr(),
            command_line_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATION_FLAGS,
            environment.0,
            working_dir_wide.as_ptr(),
            &startup,
            &mut process_info,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("CreateProcessAsUserW 启动输入代理失败");
    }
    if unsafe { AssignProcessToJobObject(job, process_info.hProcess) } == 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "输入代理加入 job 失败");
    }
    unsafe {
        CloseHandle(process_info.hThread);
    }
    if let Some(mut processes) = AGENT_PROCESSES.get().and_then(|slot| slot.lock().ok()) {
        processes.push((job as usize, process_info.hProcess as usize));
    }
    Ok(())
}

struct EnvironmentGuard(*mut c_void);

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DestroyEnvironmentBlock(self.0);
            }
        }
    }
}

fn duplicate_system_token_for_session(session_id: u32) -> Result<HANDLE> {
    let mut current_token = std::ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY,
            &mut current_token,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("打开输入服务自身令牌失败");
    }
    let _current_token = OwnedHandle(current_token);
    let mut session_token = std::ptr::null_mut();
    if unsafe {
        DuplicateTokenEx(
            current_token,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut session_token,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("复制 SYSTEM 令牌失败");
    }
    let _session_token = OwnedHandle(session_token);
    let session_id_raw = session_id;
    if unsafe {
        SetTokenInformation(
            session_token,
            TokenSessionId,
            (&raw const session_id_raw).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("设置 SYSTEM 令牌会话 ID 失败");
    }
    Ok(session_token)
}

fn console_user_token(session_id: u32) -> Result<HANDLE> {
    let mut token = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(session_id, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("WTSQueryUserToken 失败");
    }
    Ok(token)
}

fn active_console_session() -> Option<u32> {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == u32::MAX {
        None
    } else {
        Some(session_id)
    }
}

fn validate_pipe_argument(pipe: &str, kind: &str) -> Result<()> {
    let prefix = format!(r"\\.\pipe\synly-input-{kind}-");
    if !pipe.starts_with(&prefix) {
        bail!("输入服务拒绝无效的 {kind} 管道名");
    }
    let id = &pipe[prefix.len()..];
    if !is_uuid_text(id) {
        bail!("输入服务拒绝无效的 {kind} 管道 ID");
    }
    Ok(())
}

fn validate_token_argument(token: &str) -> Result<()> {
    if !is_uuid_text(token) {
        bail!("输入服务拒绝无效的 token");
    }
    Ok(())
}

fn is_uuid_text(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let mut hex_groups = [0usize; 5];
    let mut group = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'-' if matches!(index, 8 | 13 | 18 | 23) => {
                group += 1;
            }
            b'-' => return false,
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                hex_groups[group] += 1;
            }
            _ => return false,
        }
    }
    hex_groups == [8, 4, 4, 4, 12]
}

fn reap_exited_agent_processes() {
    let Some(processes) = AGENT_PROCESSES.get() else {
        return;
    };
    let Ok(mut processes) = processes.lock() else {
        return;
    };
    let mut retained = Vec::with_capacity(processes.len());
    for (job, process) in processes.drain(..) {
        if unsafe { WaitForSingleObject(process as HANDLE, 0) } == WAIT_OBJECT_0 {
            unsafe {
                CloseHandle(job as HANDLE);
                CloseHandle(process as HANDLE);
            }
        } else {
            retained.push((job, process));
        }
    }
    *processes = retained;
}

fn close_all_agent_processes() {
    let Some(processes) = AGENT_PROCESSES.get() else {
        return;
    };
    let Ok(processes) = processes.lock() else {
        return;
    };
    for (job, process) in processes.iter() {
        unsafe {
            CloseHandle(*job as HANDLE);
            CloseHandle(*process as HANDLE);
        }
    }
}

fn stop_requested() -> bool {
    STOP_EVENT
        .get()
        .is_some_and(|event| {
            let waited =
                unsafe { WaitForSingleObject(event.load(Ordering::Acquire) as HANDLE, 0) };
            waited == WAIT_OBJECT_0
        })
}

fn sleep_service() {
    if let Some(event) = STOP_EVENT.get() {
        unsafe {
            WaitForSingleObject(event.load(Ordering::Acquire) as HANDLE, SERVICE_STOP_POLL_MS);
        }
    } else {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(SERVICE_STOP_POLL_MS)));
    }
}

#[cfg(test)]
mod tests {
    use super::{is_uuid_text, validate_pipe_argument};

    #[test]
    fn uuid_text_accepts_only_canonical_dashed_form() {
        assert!(is_uuid_text("736a2142-cf80-7767-fe7c-3586163d04c6"));
        assert!(is_uuid_text("736A2142-CF80-7767-FE7C-3586163D04C6"));
        assert!(!is_uuid_text("736a2142cf807767fe7c3586163d04c6"));
        assert!(!is_uuid_text("736a2142-cf80-7767-fe7c-3586163d04c6-"));
        assert!(!is_uuid_text("736a2142-cf80-7767-fe7c-3586163d04c"));
        assert!(!is_uuid_text("736a2142-cf80-7767-fe7c-3586163g04c6"));
    }

    #[test]
    fn pipe_arguments_require_expected_prefix_and_uuid() {
        let id = "736a2142-cf80-7767-fe7c-3586163d04c6";
        assert!(validate_pipe_argument(
            &format!(r"\\.\pipe\synly-input-command-{id}"),
            "command"
        )
        .is_ok());
        assert!(validate_pipe_argument(
            &format!(r"\\.\pipe\synly-input-event-{id}"),
            "event"
        )
        .is_ok());
        assert!(validate_pipe_argument(r"\\.\pipe\synly-input-command-bad", "command").is_err());
        assert!(validate_pipe_argument(
            &format!(r"\\.\pipe\synly-input-event-{id}"),
            "command"
        )
        .is_err());
        assert!(validate_pipe_argument(
            &format!(r"\\.\pipe\other-input-command-{id}"),
            "command"
        )
        .is_err());
    }
}
