use std::sync::{Mutex, OnceLock};

type ChangeCallback = Box<dyn Fn(bool) + Send + Sync + 'static>;
static CHANGE_CALLBACK: OnceLock<Mutex<Option<ChangeCallback>>> = OnceLock::new();

unsafe extern "C" {
    fn synly_permissions_is_accessibility_trusted() -> bool;
    fn synly_permissions_request_accessibility();
    fn synly_permissions_set_change_handler(
        handler: Option<unsafe extern "C" fn(bool)>,
    );
    fn synly_foreground_cursor_captured() -> bool;
}

unsafe extern "C" fn accessibility_change_received(trusted: bool) {
    let Some(slot) = CHANGE_CALLBACK.get() else { return };
    let Ok(guard) = slot.lock() else { return };
    let Some(callback) = guard.as_ref() else { return };
    callback(trusted);
}

pub fn is_accessibility_trusted() -> bool {
    unsafe { synly_permissions_is_accessibility_trusted() }
}

pub fn request_accessibility() {
    unsafe { synly_permissions_request_accessibility() };
}

pub fn watch_accessibility_change(callback: impl Fn(bool) + Send + Sync + 'static) {
    let _ = CHANGE_CALLBACK.set(Mutex::new(Some(Box::new(callback))));
    unsafe { synly_permissions_set_change_handler(Some(accessibility_change_received)) };
}

/// 前台应用是否处于光标捕获状态(隐藏系统光标并读取相对移动).
pub fn foreground_cursor_captured() -> bool {
    unsafe { synly_foreground_cursor_captured() }
}
