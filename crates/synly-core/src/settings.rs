use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum FileSyncMode {
    Off,
    Send,
    Receive,
    Both,
    Auto,
}

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    #[default]
    Off,
    Send,
    Receive,
    Both,
}

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Off,
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum InitialSyncMode {
    This,
    Other,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_filter(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPreference {
    Host,
    Join,
}

impl AudioMode {
    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "关闭",
            AudioMode::Send => "发送",
            AudioMode::Receive => "接收",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            AudioMode::Off => "off",
            AudioMode::Send => "send",
            AudioMode::Receive => "receive",
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

impl InitialSyncMode {
    pub fn label(self) -> &'static str {
        match self {
            InitialSyncMode::This => "本机目录",
            InitialSyncMode::Other => "对端目录",
        }
    }

}

impl FileSyncMode {
    pub fn can_send(self) -> bool {
        matches!(
            self,
            FileSyncMode::Send | FileSyncMode::Both | FileSyncMode::Auto
        )
    }

    pub fn can_receive(self) -> bool {
        matches!(
            self,
            FileSyncMode::Receive | FileSyncMode::Both | FileSyncMode::Auto
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            FileSyncMode::Off => "关闭文件同步",
            FileSyncMode::Send => "发送方",
            FileSyncMode::Receive => "接收方",
            FileSyncMode::Both => "双向同步",
            FileSyncMode::Auto => "自动协商",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            FileSyncMode::Off => "off",
            FileSyncMode::Send => "send",
            FileSyncMode::Receive => "receive",
            FileSyncMode::Both => "both",
            FileSyncMode::Auto => "auto",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "send" => Some(Self::Send),
            "receive" => Some(Self::Receive),
            "both" => Some(Self::Both),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

impl ClipboardMode {
    pub fn can_send(self) -> bool {
        matches!(self, ClipboardMode::Send | ClipboardMode::Both)
    }

    pub fn can_receive(self) -> bool {
        matches!(self, ClipboardMode::Receive | ClipboardMode::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            ClipboardMode::Off => "关闭",
            ClipboardMode::Send => "发送方",
            ClipboardMode::Receive => "接收方",
            ClipboardMode::Both => "双向同步",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            ClipboardMode::Off => "off",
            ClipboardMode::Send => "send",
            ClipboardMode::Receive => "receive",
            ClipboardMode::Both => "both",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "send" => Some(Self::Send),
            "receive" => Some(Self::Receive),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}
