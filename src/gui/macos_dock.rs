use std::sync::OnceLock;

#[cfg(target_os = "macos")]
mod ffi {
    unsafe extern "C" {
        pub(super) fn synly_dock_set_visible(visible: bool);
        pub(super) fn synly_dock_set_follow_window(follow: bool);
        pub(super) fn synly_dock_note_hidden();
        pub(super) fn synly_dock_set_show_callback(callback: extern "C" fn());
    }
}

static SHOW: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

#[cfg(target_os = "macos")]
extern "C" fn show_trampoline() {
    if let Some(callback) = SHOW.get() {
        callback();
    }
}

/// 控制 macOS Dock 中应用图标的可见性.
/// 窗口显示时传入 true, 窗口隐藏到托盘后按设置决定是否隐藏 Dock.
/// 非 macOS 平台为空操作.
#[cfg(target_os = "macos")]
pub(super) fn set_dock_visible(visible: bool) {
    unsafe { ffi::synly_dock_set_visible(visible) };
}

#[cfg(not(target_os = "macos"))]
pub(super) fn set_dock_visible(_visible: bool) {}

#[cfg(target_os = "macos")]
pub(super) fn set_follow_window(follow: bool) {
    unsafe { ffi::synly_dock_set_follow_window(follow) };
}

#[cfg(not(target_os = "macos"))]
pub(super) fn set_follow_window(_follow: bool) {}

#[cfg(target_os = "macos")]
pub(super) fn note_hidden() {
    unsafe { ffi::synly_dock_note_hidden() };
}

#[cfg(not(target_os = "macos"))]
pub(super) fn note_hidden() {}

pub(super) fn install_reopen_handler(callback: impl Fn() + Send + Sync + 'static) {
    let _ = SHOW.set(Box::new(callback));
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::synly_dock_set_show_callback(show_trampoline);
    }
}
