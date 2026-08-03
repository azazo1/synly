use anyhow::{Context, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, DF_ALLOWOTHERACCOUNTHOOK, GetUserObjectInformationW,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};

/// 当前输入桌面是否为安全桌面 (Winlogon/UAC/锁屏), 由 agent 桌面轮询更新.
static SECURE_INPUT_DESKTOP: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_input_desktop_secure(secure: bool) {
    SECURE_INPUT_DESKTOP.store(secure, Ordering::Release);
}

pub(crate) fn input_desktop_is_secure() -> bool {
    SECURE_INPUT_DESKTOP.load(Ordering::Acquire)
}

/// 当前输入桌面是否是常规 Default 桌面.
pub(crate) fn current_input_desktop_is_default() -> bool {
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return false;
    }
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(desktop, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
    }
    let mut buffer = vec![0u16; (needed as usize / 2).max(1)];
    let ok = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    };
    unsafe {
        CloseDesktop(desktop);
    }
    if ok == 0 {
        return false;
    }
    let end = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).eq_ignore_ascii_case("Default")
}

/// 将当前线程切换到当前输入桌面, 用于 SendInput 失败后的桌面跟随重试 (参考 Sunshine).
pub(crate) fn sync_thread_input_desktop() -> Result<()> {
    let desktop = unsafe { OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, 0, GENERIC_ALL) };
    if desktop.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("OpenInputDesktop 失败, 无法跟随 Windows 输入桌面");
    }
    let ok = unsafe { SetThreadDesktop(desktop) };
    unsafe {
        CloseDesktop(desktop);
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("SetThreadDesktop 失败, 无法切换 Windows 输入桌面");
    }
    Ok(())
}
