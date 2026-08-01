use crate::protocol::{CapabilityEpoch, RuntimeCapabilities};
use crate::clipboard::ClipboardRuntimeOptions;
use crate::input::InputRuntimeOptions;
use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeLifecycle {
    #[default]
    Idle,
    Hosting,
    Discovering,
    Connecting,
    Pairing,
    Connected,
    Reconfiguring,
    Error,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeTuning {
    pub interval_secs: u64,
    pub sync_delete: bool,
    pub notifications_enabled: bool,
    pub input_backend_generation: u64,
    pub device_name: String,
    pub instance_name: Option<String>,
    pub discovery: crate::config::DiscoveryConfig,
    pub input: InputRuntimeOptions,
    pub clipboard: ClipboardRuntimeOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimePeerSummary {
    pub device_id: Uuid,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeCommand {
    DisconnectPeer(Uuid),
    SwitchActiveSession(Uuid),
    ClearPreferredActive,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractionRequest {
    ShowHostPin {
        request_id: Uuid,
        remote_label: String,
        bootstrap_short: String,
        bootstrap_randomart: String,
        session_short: String,
        session_randomart: String,
        pin: String,
        fixed_pin: bool,
    },
    EnterPin {
        request_id: Uuid,
        bootstrap_short: String,
        bootstrap_randomart: String,
        session_short: String,
        session_randomart: String,
    },
    AcceptPeer {
        request_id: Uuid,
        display_name: String,
        device_id: Uuid,
        summary: Vec<String>,
        default_trust: bool,
    },
    ConfirmTrust {
        request_id: Uuid,
        display_name: String,
        device_id: Uuid,
    },
    Clear {
        request_id: Uuid,
    },
}

impl InteractionRequest {
    pub fn request_id(&self) -> Uuid {
        match self {
            Self::ShowHostPin { request_id, .. }
            | Self::EnterPin { request_id, .. }
            | Self::AcceptPeer { request_id, .. }
            | Self::ConfirmTrust { request_id, .. }
            | Self::Clear { request_id } => *request_id,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractionResponse {
    Pin(String),
    Decision { accepted: bool, trust: bool },
    Confirm(bool),
    Cancel,
}

#[derive(Debug)]
pub struct InteractionEnvelope {
    pub request: InteractionRequest,
    pub response: Option<oneshot::Sender<InteractionResponse>>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum RuntimeEvent {
    Lifecycle(RuntimeLifecycle),
    Connected(RuntimePeerSummary),
    Disconnected(RuntimePeerSummary),
    ActiveSession {
        device_id: Uuid,
    },
    /// 首选活跃设备身份变化, 供上层持久化.
    PreferredActiveChanged {
        device_id: Option<Uuid>,
    },
    Interaction(InteractionEnvelope),
    Capabilities {
        peer: RuntimePeerSummary,
        local: RuntimeCapabilities,
        remote: RuntimeCapabilities,
        epoch: CapabilityEpoch,
        acknowledged: bool,
    },
    Error(String),
}

#[derive(Clone)]
pub struct RuntimeControl {
    shutdown: CancellationToken,
    capabilities: watch::Receiver<RuntimeCapabilities>,
    tuning: watch::Receiver<RuntimeTuning>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    commands: mpsc::UnboundedSender<RuntimeCommand>,
}

pub struct RuntimeControlHandle {
    pub shutdown: CancellationToken,
    pub capabilities: watch::Sender<RuntimeCapabilities>,
    pub tuning: watch::Sender<RuntimeTuning>,
    pub events: mpsc::UnboundedReceiver<RuntimeEvent>,
    pub commands: mpsc::UnboundedReceiver<RuntimeCommand>,
}

impl std::fmt::Debug for RuntimeControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeControl")
            .field("cancelled", &self.shutdown.is_cancelled())
            .field("capabilities", &*self.capabilities.borrow())
            .field("tuning", &*self.tuning.borrow())
            .finish_non_exhaustive()
    }
}

impl RuntimeControl {
    pub fn new(
        initial: RuntimeCapabilities,
        initial_tuning: RuntimeTuning,
    ) -> (Self, RuntimeControlHandle) {
        let shutdown = CancellationToken::new();
        let (capabilities_tx, capabilities) = watch::channel(initial);
        let (tuning_tx, tuning) = watch::channel(initial_tuning);
        let (events, events_rx) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        (
            Self {
                shutdown: shutdown.clone(),
                capabilities,
                tuning,
                events,
                commands,
            },
            RuntimeControlHandle {
                shutdown,
                capabilities: capabilities_tx,
                tuning: tuning_tx,
                events: events_rx,
                commands: commands_rx,
            },
        )
    }

    pub fn detached(initial: RuntimeCapabilities, initial_tuning: RuntimeTuning) -> Self {
        Self::new(initial, initial_tuning).0
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub fn capabilities(&self) -> watch::Receiver<RuntimeCapabilities> {
        self.capabilities.clone()
    }

    pub fn tuning(&self) -> watch::Receiver<RuntimeTuning> {
        self.tuning.clone()
    }

    pub fn report(&self, event: RuntimeEvent) {
        let _ = self.events.send(event);
    }

    pub fn commands(&self) -> mpsc::UnboundedSender<RuntimeCommand> {
        self.commands.clone()
    }

    pub fn notify_interaction(&self, request: InteractionRequest) {
        self.report(RuntimeEvent::Interaction(InteractionEnvelope {
            request,
            response: None,
        }));
    }

    pub async fn request_interaction(
        &self,
        request: InteractionRequest,
    ) -> Result<InteractionResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.events
            .send(RuntimeEvent::Interaction(InteractionEnvelope {
                request,
                response: Some(response_tx),
            }))
            .map_err(|_| anyhow::anyhow!("GUI interaction channel is closed"))?;
        response_rx
            .await
            .context("GUI interaction response channel is closed")
    }
}
