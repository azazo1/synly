#[cfg(target_os = "macos")]
mod ffi {
    unsafe extern "C" {
        pub(super) fn synly_dock_set_visible(visible: bool);
    }
}

/// 控制 macOS Dock 中应用图标的可见性.
/// 窗口显示时传入 true, 窗口隐藏到托盘后传入 false.
/// 非 macOS 平台为空操作.
#[cfg(target_os = "macos")]
pub(super) fn set_dock_visible(visible: bool) {
    unsafe { ffi::synly_dock_set_visible(visible) };
}

#[cfg(not(target_os = "macos"))]
pub(super) fn set_dock_visible(_visible: bool) {}
