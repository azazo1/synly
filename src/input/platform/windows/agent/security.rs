use super::pipe::NativePipe;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

pub(crate) struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    pub(super) attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    pub(crate) fn for_current_user() -> Result<Self> {
        let user_sid = current_user_sid_string()?;
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
                .context("failed to build Windows input agent pipe DACL");
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub(crate) fn validate_parent_process(parent_pid: u32) -> Result<()> {
    if process_session_id(parent_pid)? != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent and GUI are not in the same session");
    }
    let parent_path = process_image_path(parent_pid)?;
    let agent_path = std::env::current_exe()?;
    validate_install_directory(&parent_path, &agent_path)?;
    Ok(())
}

pub(crate) fn validate_pipe_server(client: &NativePipe, expected_pid: u32) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeServerProcessId(client.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe server PID validation failed");
    }
    Ok(())
}

pub(crate) fn validate_pipe_client(
    server: &NativePipe,
    expected_pid: u32,
    reported_path: &Path,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeClientProcessId(server.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe client PID validation failed");
    }
    let actual_path = match process_image_path(actual_pid) {
        Ok(path) => path,
        Err(error) if is_access_denied_error(&error) => {
            // SYSTEM 代理进程不允许普通权限 GUI 打开, 回退到代理握手自报路径.
            // 同用户伪造进程仍可被打开并走严格校验, 其他用户进程被管道 DACL 拦截,
            // SYSTEM 进程本身完全可信, 因此该回退不削弱实际威胁模型.
            tracing::warn!(
                error = %error,
                pid = actual_pid,
                "无法打开 Windows 输入代理进程校验映像, 回退到握手路径"
            );
            reported_path.to_path_buf()
        }
        Err(error) => return Err(error),
    };
    if normalize_path(&actual_path) != normalize_path(reported_path) {
        bail!("Windows input agent image path validation failed");
    }
    let gui_path = std::env::current_exe()?;
    validate_install_directory(&gui_path, &actual_path)?;
    let agent_session = match process_session_id(actual_pid) {
        Ok(session_id) => session_id,
        Err(error) if is_access_denied_error(&error) => {
            // SYSTEM 代理进程不允许普通权限 GUI 打开, 无法查询会话.
            // 服务端拉起前已校验请求方属于控制台会话, 此校验仅为纵深防御.
            tracing::warn!(
                error = %error,
                pid = actual_pid,
                "无法查询 Windows 输入代理会话, 跳过会话校验"
            );
            process_session_id(unsafe { GetCurrentProcessId() })?
        }
        Err(error) => return Err(error),
    };
    if agent_session != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent session validation failed");
    }
    Ok(())
}

fn is_access_denied_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(5))
    })
}

fn validate_install_directory(left: &Path, right: &Path) -> Result<()> {
    if normalize_path(left.parent().unwrap_or(left)) != normalize_path(right.parent().unwrap_or(right)) {
        bail!("Windows input agent and GUI are not installed in the same directory");
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn process_session_id(process_id: u32) -> Result<u32> {
    let mut session_id = 0u32;
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to resolve Windows process session ID");
    }
    Ok(session_id)
}

pub(crate) fn current_process_is_elevated() -> Result<bool> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open current Windows process token for elevation check");
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to query current Windows process elevation");
    }
    if returned != std::mem::size_of::<TOKEN_ELEVATION>() as u32 {
        bail!("unexpected Windows process elevation token size: {returned}");
    }
    Ok(elevation.TokenIsElevated != 0)
}

pub(crate) fn process_image_path(process_id: u32) -> Result<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error()).context(format!(
            "failed to open Windows process {process_id} for image validation"
        ));
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to query Windows process image path");
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..length as usize],
    )))
}

pub(crate) fn current_user_sid_string() -> Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open current Windows process token");
    }
    let result = token_user_sid_string(token);
    unsafe {
        CloseHandle(token);
    }
    result
}

pub(crate) fn token_user_sid_string(token: HANDLE) -> Result<String> {
    let mut required = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 {
        unsafe {
            CloseHandle(token);
        }
        return Err(std::io::Error::last_os_error())
            .context("failed to size current Windows user SID");
    }

    let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; word_count];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to read current Windows user SID");
    }

    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to format current Windows user SID");
    }
    let mut length = 0usize;
    unsafe {
        while *sid_text.add(length) != 0 {
            length += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
    unsafe {
        LocalFree(sid_text.cast());
    }
    Ok(sid)
}

pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn current_process_is_system() -> Result<bool> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open current Windows process token for system check");
    }
    let result = token_user_sid_string(token).map(|sid| sid.eq_ignore_ascii_case("S-1-5-18"));
    unsafe {
        CloseHandle(token);
    }
    result
}
