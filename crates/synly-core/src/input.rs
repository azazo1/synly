use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputChannelOffer {
    pub session_id: Uuid,
    pub certificate_der: Vec<u8>,
}

