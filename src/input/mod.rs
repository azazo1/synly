mod channel;
mod geometry;
mod hotkey;
mod mapping;
#[cfg(all(
    any(target_os = "macos", target_os = "windows"),
    feature = "input-screen-mock"
))]
pub mod mock;
#[cfg(all(target_os = "macos", feature = "input-macos-trackpad-debug"))]
pub mod trackpad_debug;
#[cfg(feature = "input-receiver-mock")]
pub mod receiver_mock;
mod platform;
mod protocol;
mod runtime;
#[cfg(windows)]
pub use platform::windows as windows_agent;
#[cfg(target_os = "macos")]
pub use platform::macos::permissions::{
    is_accessibility_trusted, request_accessibility, watch_accessibility_change,
};

pub use channel::{
    InputChannelOffer, InputChannelRole, InputHostChannel, read_preamble as read_input_preamble,
};
pub use synly_core::input::InputMode;
pub use geometry::{DesktopLayout, DisplayRect, Point, ScreenEdge};
pub use hotkey::{Hotkey, ModifierMask};
pub use mapping::{InputPlatform, KeyMappingConfig, validate_key_mapping};
pub use protocol::KeySnapshot;
pub use runtime::{
    CursorMode, InputRuntimeOptions, InputSessionContext, InputSocketConnection, InputSocketInbox,
    run_input_session,
};

use anyhow::Result;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalInputRole {
    Send,
    Receive,
}

pub fn negotiate_input(local: InputMode, remote: InputMode) -> Option<LocalInputRole> {
    match (local, remote) {
        (InputMode::Send, InputMode::Receive) => Some(LocalInputRole::Send),
        (InputMode::Receive, InputMode::Send) => Some(LocalInputRole::Receive),
        _ => None,
    }
}

pub fn ensure_platform_supported(mode: InputMode) -> Result<()> {
    if mode == InputMode::Off {
        return Ok(());
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        platform::ensure_permissions(mode)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(anyhow::anyhow!("鼠标键盘同步目前只支持 macOS 和 Windows"))
    }
}

#[cfg(windows)]
pub use platform::windows::{request_elevation as request_windows_input_elevation, run_agent};

#[cfg(windows)]
pub use platform::windows::init_agent_tracing as init_windows_agent_tracing;

#[cfg(windows)]
pub use platform::windows::{
    service_installed as windows_input_service_installed,
    uninstall_via_uac as request_windows_input_service_uninstall_via_uac,
    mark_install_attempted as mark_windows_input_service_install_attempted,
    init_tracing as init_windows_service_tracing,
    install as install_windows_input_service,
    uninstall as uninstall_windows_input_service,
    status as windows_input_service_status,
    run_service as run_windows_input_service,
};

#[cfg(windows)]
pub fn windows_input_agent_ready() -> bool {
    platform::windows::agent_ready()
}

#[cfg(windows)]
pub fn windows_input_elevation_requested() -> bool {
    platform::windows::agent_elevation_requested()
}

#[cfg(test)]
mod tests {
    use super::{InputMode, LocalInputRole, negotiate_input};

    #[test]
    fn input_direction_requires_complementary_modes() {
        assert_eq!(
            negotiate_input(InputMode::Send, InputMode::Receive),
            Some(LocalInputRole::Send)
        );
        assert_eq!(
            negotiate_input(InputMode::Receive, InputMode::Send),
            Some(LocalInputRole::Receive)
        );
        assert_eq!(negotiate_input(InputMode::Send, InputMode::Send), None);
        assert_eq!(negotiate_input(InputMode::Off, InputMode::Receive), None);
    }
}
