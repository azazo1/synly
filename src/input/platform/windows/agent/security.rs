use super::pipe::NativePipe;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

pub(super) struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    pub(super) attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    pub(super) fn for_current_user() -> Result<Self> {
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

pub(super) fn validate_parent_process(parent_pid: u32) -> Result<()> {
    if process_session_id(parent_pid)? != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent and GUI are not in the same session");
    }
    let parent_path = process_image_path(parent_pid)?;
    let agent_path = std::env::current_exe()?;
    validate_install_directory(&parent_path, &agent_path)?;
    Ok(())
}

pub(super) fn validate_pipe_server(client: &NativePipe, expected_pid: u32) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeServerProcessId(client.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe server PID validation failed");
    }
    Ok(())
}

pub(super) fn validate_pipe_client(
    server: &NativePipe,
    expected_pid: u32,
    reported_path: &Path,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeClientProcessId(server.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe client PID validation failed");
    }
    let actual_path = process_image_path(actual_pid)?;
    if normalize_path(&actual_path) != normalize_path(reported_path) {
        bail!("Windows input agent image path validation failed");
    }
    let gui_path = std::env::current_exe()?;
    validate_install_directory(&gui_path, &actual_path)?;
    if process_session_id(actual_pid)? != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent session validation failed");
    }
    Ok(())
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

pub(super) fn process_session_id(process_id: u32) -> Result<u32> {
    let mut session_id = 0u32;
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0 {
        bail!("failed to resolve Windows process session ID");
    }
    Ok(session_id)
}

fn process_image_path(process_id: u32) -> Result<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        bail!("failed to open Windows process {process_id} for image validation");
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 {
        bail!("failed to query Windows process image path");
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..length as usize],
    )))
}

fn current_user_sid_string() -> Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open current Windows process token");
    }

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
    unsafe {
        CloseHandle(token);
    }
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

pub(super) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
