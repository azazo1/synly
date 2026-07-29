mod channel;
mod geometry;
mod hotkey;
#[cfg(target_os = "macos")]
pub mod mock;
mod platform;
mod protocol;
mod runtime;

pub use channel::{InputChannelOffer, InputChannelRole, InputHostChannel};
pub use geometry::{DesktopLayout, DisplayRect, Point, ScreenEdge};
pub use hotkey::{Hotkey, ModifierMask};
pub use protocol::KeySnapshot;
pub use runtime::{InputRuntimeOptions, InputSessionContext, run_input_session};

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Off,
    Send,
    Receive,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Send => "发送控制",
            Self::Receive => "接受控制",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Send => "send",
            Self::Receive => "receive",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "send" => Some(Self::Send),
            "receive" => Some(Self::Receive),
            _ => None,
        }
    }
}

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
