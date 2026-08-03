use super::pipe::NativePipe;
use super::super::super::NativeEvent;
use crate::input::{DesktopLayout, DisplayRect, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) const IPC_MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum AgentRequest {
    Start { mode: InputMode, hotkey: Hotkey },
    Stop,
    Health,
    CursorPosition,
    Snapshot,
    SetCapture(bool),
    WarpCursor(Point),
    InjectKey {
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    },
    InjectButton { button: u8, down: bool },
    InjectCursor(Point),
    InjectMotion { dx: i32, dy: i32 },
    InjectWheel { x: i32, y: i32 },
    ReleaseAll,
}

impl AgentRequest {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "Start",
            Self::Stop => "Stop",
            Self::Health => "Health",
            Self::CursorPosition => "CursorPosition",
            Self::Snapshot => "Snapshot",
            Self::SetCapture(_) => "SetCapture",
            Self::WarpCursor(_) => "WarpCursor",
            Self::InjectKey { .. } => "InjectKey",
            Self::InjectButton { .. } => "InjectButton",
            Self::InjectCursor(_) => "InjectCursor",
            Self::InjectMotion { .. } => "InjectMotion",
            Self::InjectWheel { .. } => "InjectWheel",
            Self::ReleaseAll => "ReleaseAll",
        }
    }

    pub(super) fn requires_cursor_ordering(&self) -> bool {
        matches!(self, Self::InjectButton { .. } | Self::InjectWheel { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum AgentResponse {
    Ok,
    Pong,
    Started {
        layout: DesktopLayout,
        secure_desktop: bool,
        primary: Option<DisplayRect>,
    },
    Point(Point),
    Snapshot(KeySnapshot),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum GuiToAgentPacket {
    HelloAck {
        session_id: u32,
    },
    Request {
        id: u64,
        request: AgentRequest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum AgentToGuiPacket {
    Hello {
        token: String,
        agent_pid: u32,
        parent_pid: u32,
        agent_path: PathBuf,
        agent_is_system: bool,
    },
    Ready,
    StartupError {
        error: String,
    },
    Response {
        id: u64,
        response: AgentResponse,
    },
    Event(NativeEvent),
    Motion {
        dx: i32,
        dy: i32,
        position: Option<Point>,
        position_updated: bool,
    },
    SecureDesktopChanged {
        secure: bool,
        primary: Option<DisplayRect>,
    },
}

impl AgentToGuiPacket {
    pub(super) fn packet_name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "Hello",
            Self::Ready => "Ready",
            Self::StartupError { .. } => "StartupError",
            Self::Response { .. } => "Response",
            Self::Event(_) => "Event",
            Self::Motion { .. } => "Motion",
            Self::SecureDesktopChanged { .. } => "SecureDesktopChanged",
        }
    }
}

pub(crate) fn write_packet<P>(
    pipe: &mut NativePipe,
    packet: &P,
    timeout: Duration,
) -> Result<()>
where
    P: Serialize,
{
    let bytes = bincode::serialize(packet).context("failed to encode Windows input agent packet")?;
    if bytes.is_empty() || bytes.len() > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid");
    }
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    pipe.write_all(&frame, timeout)
}

pub(crate) fn read_packet<P>(pipe: &mut NativePipe, timeout: Duration) -> Result<P>
where
    P: DeserializeOwned,
{
    let started = Instant::now();
    let mut length_bytes = [0u8; 4];
    pipe.read_exact(&mut length_bytes, timeout)?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 || length > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid: {length}");
    }
    let mut bytes = vec![0u8; length];
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        bail!("Windows input agent packet read timed out");
    }
    pipe.read_exact(&mut bytes, remaining)?;
    bincode::deserialize(&bytes)
        .context("failed to decode Windows input agent packet")
}

pub(crate) fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
            || cause.to_string().contains("timed out")
    })
}
