use super::model::{
    AppCommand, AppLifecycle, AppSettings, AppSnapshot, DiscoveredPeerView,
    PendingInteraction,
};
use crate::cli::{cli_from_runtime_config, collect_runtime_options};
use crate::config::{RuntimeConfig, SynlyConfig};
use crate::discovery::{self, DiscoveredPeer};
use crate::protocol::{PROTOCOL_VERSION, RuntimeCapabilities};
use crate::runtime_control::{
    InteractionEnvelope, RuntimeControl, RuntimeControlHandle, RuntimeEvent, RuntimeLifecycle,
    RuntimeTuning,
};
use crate::settings::ConnectionPreference;
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

const COMMAND_CAPACITY: usize = 64;
const SESSION_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AppSupervisorHandle {
    commands: mpsc::Sender<AppCommand>,
    snapshots: watch::Receiver<AppSnapshot>,
}

impl Clone for AppSupervisorHandle {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            snapshots: self.snapshots.clone(),
        }
    }
}

impl AppSupervisorHandle {
    pub fn commands(&self) -> mpsc::Sender<AppCommand> {
        self.commands.clone()
    }

    pub fn snapshots(&self) -> watch::Receiver<AppSnapshot> {
        self.snapshots.clone()
    }
}

pub struct AppSupervisor {
    config: SynlyConfig,
    snapshot: AppSnapshot,
    snapshot_tx: watch::Sender<AppSnapshot>,
    command_rx: mpsc::Receiver<AppCommand>,
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
    internal_rx: mpsc::UnboundedReceiver<InternalEvent>,
    session: Option<SessionHandle>,
    session_pin: Option<String>,
    discovery_task: Option<JoinHandle<()>>,
    discovery_epoch: u64,
    input_backend_generation: u64,
    pending_responses: HashMap<Uuid, oneshot::Sender<crate::runtime_control::InteractionResponse>>,
}

struct SessionHandle {
    id: Uuid,
    shutdown: tokio_util::sync::CancellationToken,
    capabilities: watch::Sender<RuntimeCapabilities>,
    tuning: watch::Sender<RuntimeTuning>,
    task: JoinHandle<()>,
}

enum InternalEvent {
    Discovery {
        epoch: u64,
        result: Result<Vec<DiscoveredPeer>, String>,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    InputElevation(bool),
    Runtime {
        session_id: Uuid,
        event: RuntimeEvent,
    },
    SessionFinished {
        session_id: Uuid,
        result: Result<(), String>,
    },
}

impl AppSupervisor {
    pub fn new(mut config: SynlyConfig) -> (Self, AppSupervisorHandle) {
        config.runtime.normalize_file_sync_options();
        let mut snapshot = AppSnapshot::idle(
            config.runtime.clone(),
            AppSettings::from_config(&config),
        );
        snapshot.trusted_devices = config.trusted_devices.clone();
        let (snapshot_tx, snapshots) = watch::channel(snapshot.clone());
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        (
            Self {
                config,
                snapshot,
                snapshot_tx,
                command_rx,
                internal_tx,
                internal_rx,
                session: None,
                session_pin: None,
                discovery_task: None,
                discovery_epoch: 0,
                input_backend_generation: 0,
                pending_responses: HashMap::new(),
            },
            AppSupervisorHandle {
                commands,
                snapshots,
            },
        )
    }

    pub async fn run(mut self) {
        if !self.snapshot.settings.ui.first_run_completed {
            self.snapshot.settings.ui.first_run_completed = true;
            self.config.ui.first_run_completed = true;
            self.save_config();
            self.publish();
        }
        self.spawn_discovery_loop();
        self.spawn_input_permission_monitor();
        if self.snapshot.settings.ui.resume_last_session
            && self.snapshot.desired.connection.is_some()
        {
            self.start_session().await;
        }

        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    if self.handle_command(command).await {
                        break;
                    }
                }
                event = self.internal_rx.recv() => {
                    let Some(event) = event else { break };
                    self.handle_internal_event(event).await;
                }
            }
        }

        self.stop_session().await;
        if let Some(task) = self.discovery_task.take() {
            task.abort();
        }
        self.snapshot.lifecycle = AppLifecycle::Stopping;
        self.publish();
    }

    async fn handle_command(&mut self, command: AppCommand) -> bool {
        match command {
            AppCommand::ApplySettings {
                mut runtime,
                mut settings,
                session_pin,
            } => {
                runtime.normalize_file_sync_options();
                settings.device_name = settings.device_name.trim().to_string();
                let mut candidate = self.config.clone();
                apply_settings_to_config(&mut candidate, &runtime, &settings);
                let cli = cli_from_runtime_config(&runtime, session_pin.clone(), false);
                if let Err(error) = validate_settings(&candidate)
                    .and_then(|_| collect_runtime_options(cli, &candidate).map(|_| ()))
                {
                    self.set_error(error.to_string());
                    self.publish();
                    return false;
                }
                if settings.ui.launch_at_login
                    != self.snapshot.settings.ui.launch_at_login
                    && let Err(error) = crate::autostart::apply(settings.ui.launch_at_login)
                {
                    self.set_error(format!("无法更新登录启动设置: {error:#}"));
                    self.publish();
                    return false;
                }
                if settings.ui.log_level != self.snapshot.settings.ui.log_level
                    && let Err(error) =
                        crate::tracing_utils::set_log_level(settings.ui.log_level.as_filter())
                {
                    self.set_error(format!("无法更新日志级别: {error:#}"));
                    self.publish();
                    return false;
                }
                let discovery_changed = settings.discovery != self.snapshot.settings.discovery;
                let infrastructure_changed = settings.transfer != self.snapshot.settings.transfer
                    || session_pin != self.session_pin;
                let reconnect = self.session.is_some()
                    && (requires_reconnect(&self.snapshot.desired, &runtime)
                        || infrastructure_changed);
                self.snapshot.desired = runtime.clone();
                self.snapshot.settings = *settings;
                self.config = candidate;
                self.session_pin = session_pin;
                self.save_config();
                if discovery_changed {
                    self.spawn_discovery_loop();
                }
                if reconnect {
                    self.snapshot.pending = Some(self.snapshot.desired.clone());
                    self.snapshot.lifecycle = AppLifecycle::Reconfiguring;
                    self.publish();
                    self.stop_session().await;
                    self.start_session().await;
                } else {
                    let capability_change = self.session.is_some()
                        && capability_fields_changed(
                            self.snapshot.applied.as_ref(),
                            &self.snapshot.desired,
                        );
                    self.update_capabilities();
                    self.update_tuning();
                    if self.session.is_some() {
                        let mut applied = self.snapshot.desired.clone();
                        if capability_change
                            && let Some(previous) = self.snapshot.applied.as_ref()
                        {
                            applied.clipboard_mode = previous.clipboard_mode;
                            applied.audio_mode = previous.audio_mode;
                            applied.input_mode = previous.input_mode;
                        }
                        self.snapshot.applied = Some(applied);
                        self.snapshot.pending = capability_change
                            .then(|| self.snapshot.desired.clone());
                    }
                    self.publish();
                }
            }
            AppCommand::Start => self.start_session().await,
            AppCommand::StartHosting => {
                self.snapshot.desired.connection = Some(ConnectionPreference::Host);
                self.config.runtime = self.snapshot.desired.clone();
                self.save_config();
                self.restart_session().await;
            }
            AppCommand::RefreshDiscovery => {
                tracing::info!("用户请求刷新设备发现");
                self.spawn_discovery_loop();
            }
            AppCommand::ConnectPeer(peer) => {
                self.snapshot.desired.connection = Some(ConnectionPreference::Join);
                self.snapshot.desired.peer_query = peer;
                self.config.runtime = self.snapshot.desired.clone();
                self.save_config();
                self.restart_session().await;
            }
            AppCommand::SetClipboardMode(mode) => {
                self.snapshot.desired.clipboard_mode = mode;
                self.config.runtime.clipboard_mode = mode;
                self.save_config();
                if self.session.is_some() {
                    self.snapshot.pending = Some(self.snapshot.desired.clone());
                }
                self.update_capabilities();
                self.update_tuning();
                self.publish();
            }
            AppCommand::SetAudioMode(mode) => {
                self.snapshot.desired.audio_mode = mode;
                self.config.runtime.audio_mode = mode;
                self.save_config();
                if self.session.is_some() {
                    self.snapshot.pending = Some(self.snapshot.desired.clone());
                }
                self.update_capabilities();
                self.update_tuning();
                self.publish();
            }
            AppCommand::SetInputMode(mode) => {
                self.snapshot.desired.input_mode = mode;
                self.config.runtime.input_mode = mode;
                self.save_config();
                if self.session.is_some() {
                    self.snapshot.pending = Some(self.snapshot.desired.clone());
                }
                self.update_capabilities();
                self.update_tuning();
                self.publish();
            }
            AppCommand::SelectPaths(paths) => {
                self.snapshot.desired.paths = paths;
                self.config.runtime = self.snapshot.desired.clone();
                self.save_config();
                if self.session.is_some() {
                    self.restart_session().await;
                } else {
                    self.publish();
                }
            }
            AppCommand::Disconnect => self.stop_session().await,
            AppCommand::RespondInteraction { request_id, response } => {
                if let Some(sender) = self.pending_responses.remove(&request_id) {
                    let _ = sender.send(response);
                }
                if self
                    .snapshot
                    .interaction
                    .as_ref()
                    .is_some_and(|pending| pending.request.request_id() == request_id)
                {
                    self.snapshot.interaction = None;
                    self.publish();
                }
            }
            AppCommand::RevokeTrust(device_id) => {
                if self.config.revoke_trusted_device(device_id) {
                    self.save_config();
                    self.snapshot.trusted_devices = self.config.trusted_devices.clone();
                    if self.snapshot.current_peer.as_deref() == Some(&device_id.to_string()) {
                        self.stop_session().await;
                    }
                    self.publish();
                }
            }
            AppCommand::RequestInputElevation => {
                #[cfg(windows)]
                {
                    match crate::windows_input_agent::request_elevation() {
                        Ok(()) => {
                            self.snapshot.input_elevation_ready = true;
                            self.restart_input_backend_if_active();
                        }
                        Err(error) => self.set_error(error.to_string()),
                    }
                    self.publish();
                }
                #[cfg(not(windows))]
                {
                    self.snapshot.input_elevation_ready = true;
                    self.publish();
                }
            }
            AppCommand::SaveWindowState { width, height } => {
                self.snapshot.settings.ui.window_width = width;
                self.snapshot.settings.ui.window_height = height;
                self.config.ui.window_width = width;
                self.config.ui.window_height = height;
                self.save_config();
                self.publish();
            }
            AppCommand::Shutdown => return true,
        }
        false
    }

    async fn handle_internal_event(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::Discovery {
                epoch,
                result: Ok(peers),
            } if epoch == self.discovery_epoch => {
                self.snapshot.discovered_peers = peers
                    .into_iter()
                    .filter(|peer| peer.device_id != self.config.device.device_id.to_string())
                    .map(|peer| peer_view(peer, &self.config))
                    .collect();
                self.publish();
            }
            InternalEvent::Discovery {
                epoch,
                result: Err(error),
            } if epoch == self.discovery_epoch => {
                tracing::warn!(error = %error, "设备发现刷新失败");
            }
            InternalEvent::Discovery { .. } => {}
            InternalEvent::InputElevation(ready) => {
                if self.snapshot.input_elevation_ready == ready {
                    return;
                }
                self.snapshot.input_elevation_ready = ready;
                self.restart_input_backend_if_active();
                self.publish();
            }
            InternalEvent::Runtime { session_id, event } => {
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == session_id)
                {
                    self.handle_runtime_event(event);
                }
            }
            InternalEvent::SessionFinished { session_id, result } => {
                if self
                    .session
                    .as_ref()
                    .is_none_or(|session| session.id != session_id)
                {
                    return;
                }
                self.session = None;
                self.snapshot.applied = None;
                self.snapshot.pending = None;
                self.snapshot.current_peer = None;
                self.snapshot.interaction = None;
                self.snapshot.actual_capabilities = None;
                self.snapshot.remote_capabilities = None;
                self.snapshot.capability_epoch = None;
                self.snapshot.capabilities_acknowledged = true;
                self.pending_responses.clear();
                match result {
                    Ok(()) => {
                        if self.snapshot.lifecycle != AppLifecycle::Stopping {
                            self.snapshot.lifecycle = AppLifecycle::Idle;
                        }
                    }
                    Err(error) => self.set_error(error),
                }
                self.publish();
            }
        }
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Lifecycle(lifecycle) => {
                self.snapshot.lifecycle = map_lifecycle(lifecycle);
            }
            RuntimeEvent::Connected(peer) => {
                self.snapshot.lifecycle = AppLifecycle::Connected;
                self.snapshot.current_peer = Some(peer.device_id.to_string());
            }
            RuntimeEvent::Disconnected => {
                self.snapshot.lifecycle = AppLifecycle::Idle;
                self.snapshot.current_peer = None;
            }
            RuntimeEvent::Interaction(envelope) => self.handle_interaction(envelope),
            RuntimeEvent::Capabilities {
                local,
                remote,
                epoch,
                acknowledged,
            } => {
                self.snapshot.actual_capabilities = Some(local);
                self.snapshot.remote_capabilities = Some(remote);
                self.snapshot.capability_epoch = Some(epoch);
                self.snapshot.capabilities_acknowledged = acknowledged;
                if let Some(applied) = self.snapshot.applied.as_mut() {
                    applied.clipboard_mode = local.clipboard_mode;
                    applied.audio_mode = local.audio_mode;
                    applied.input_mode = local.input_mode;
                }
                if acknowledged
                    && local.clipboard_mode == self.snapshot.desired.clipboard_mode
                    && local.audio_mode == self.snapshot.desired.audio_mode
                    && local.input_mode == self.snapshot.desired.input_mode
                {
                    self.snapshot.pending = None;
                }
            }
            RuntimeEvent::Error(error) => self.set_error(error),
        }
        self.publish();
    }

    fn handle_interaction(&mut self, envelope: InteractionEnvelope) {
        let request_id = envelope.request.request_id();
        if let Some(response) = envelope.response {
            self.pending_responses.insert(request_id, response);
        }
        if matches!(envelope.request, crate::runtime_control::InteractionRequest::Clear { .. }) {
            self.snapshot.interaction = None;
        } else {
            self.snapshot.lifecycle = AppLifecycle::Pairing;
            self.snapshot.interaction = Some(PendingInteraction {
                request: envelope.request,
            });
        }
    }

    async fn restart_session(&mut self) {
        self.stop_session().await;
        self.start_session().await;
    }

    async fn start_session(&mut self) {
        if self.session.is_some() {
            return;
        }
        let cli = cli_from_runtime_config(
            &self.snapshot.desired,
            self.session_pin.clone(),
            false,
        );
        let mut options = match collect_runtime_options(cli, &self.config) {
            Ok(options) => options,
            Err(error) => {
                self.set_error(error.to_string());
                self.publish();
                return;
            }
        };
        let capabilities = self.current_capabilities();
        let tuning = tuning_from_options(&options, self.input_backend_generation);
        let (control, control_handle) = RuntimeControl::new(capabilities, tuning);
        if let Err(error) = crate::input::ensure_platform_supported(options.input_mode) {
            self.set_error(error.to_string());
            self.publish();
            return;
        }
        options.control = control;
        self.snapshot.lifecycle = match options.connection {
            ConnectionPreference::Host => AppLifecycle::Hosting,
            ConnectionPreference::Join => AppLifecycle::Connecting,
        };
        self.snapshot.applied = Some(self.snapshot.desired.clone());
        self.snapshot.pending = None;
        self.snapshot.last_error = None;
        self.publish();

        let session_id = Uuid::new_v4();
        let mut session_config = self.config.clone();
        let internal = self.internal_tx.clone();
        let task = tokio::spawn(async move {
            let result = crate::app::run(&mut session_config, options)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = internal.send(InternalEvent::SessionFinished {
                session_id,
                result,
            });
        });
        let RuntimeControlHandle {
            shutdown,
            capabilities,
            tuning,
            events,
        } = control_handle;
        self.session = Some(SessionHandle {
            id: session_id,
            shutdown,
            capabilities,
            tuning,
            task,
        });
        self.spawn_runtime_event_forwarder(session_id, events, self.internal_tx.clone());
    }

    async fn stop_session(&mut self) {
        let Some(mut session) = self.session.take() else {
            self.snapshot.lifecycle = AppLifecycle::Idle;
            self.snapshot.applied = None;
            self.publish();
            return;
        };
        self.snapshot.lifecycle = AppLifecycle::Stopping;
        self.publish();
        session.shutdown.cancel();
        if tokio::time::timeout(SESSION_STOP_TIMEOUT, &mut session.task)
            .await
            .is_err()
        {
            session.task.abort();
            let _ = session.task.await;
        }
        self.snapshot.lifecycle = AppLifecycle::Idle;
        self.snapshot.applied = None;
        self.snapshot.current_peer = None;
        self.snapshot.interaction = None;
        self.snapshot.actual_capabilities = None;
        self.snapshot.remote_capabilities = None;
        self.snapshot.capability_epoch = None;
        self.snapshot.capabilities_acknowledged = true;
        self.pending_responses.clear();
        self.publish();
    }

    fn update_capabilities(&mut self) {
        let capabilities = self.current_capabilities();
        if let Some(session) = &self.session {
            let _ = session.capabilities.send(capabilities);
        }
    }

    fn current_capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            clipboard_mode: self.snapshot.desired.clipboard_mode,
            audio_mode: self.snapshot.desired.audio_mode,
            input_mode: self.snapshot.desired.input_mode,
        }
    }

    fn update_tuning(&mut self) {
        let Some(tuning) = self.session.as_ref().map(|session| session.tuning.clone()) else {
            return;
        };
        let cli = cli_from_runtime_config(
            &self.snapshot.desired,
            self.session_pin.clone(),
            false,
        );
        match collect_runtime_options(cli, &self.config) {
            Ok(options) => {
                let _ = tuning.send(tuning_from_options(
                    &options,
                    self.input_backend_generation,
                ));
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn restart_input_backend_if_active(&mut self) {
        if self.session.is_none()
            || self.snapshot.desired.input_mode == crate::input::InputMode::Off
        {
            return;
        }
        self.input_backend_generation = self.input_backend_generation.saturating_add(1);
        tracing::trace!(generation = self.input_backend_generation, elevated = self.snapshot.input_elevation_ready, "Windows 输入 backend generation 已更新"); // to remove
        self.update_tuning();
    }

    fn spawn_discovery_loop(&mut self) {
        if let Some(task) = self.discovery_task.take() {
            task.abort();
        }
        self.discovery_epoch = self.discovery_epoch.saturating_add(1);
        let epoch = self.discovery_epoch;
        let discovery_config = self.config.discovery.clone();
        let internal = self.internal_tx.clone();
        self.discovery_task = Some(tokio::spawn(async move {
            let mut peers = discovery::continuous_browse(discovery_config);
            if internal
                .send(InternalEvent::Discovery {
                    epoch,
                    result: Ok(peers.borrow().clone()),
                })
                .is_err()
            {
                return;
            }
            while peers.changed().await.is_ok() {
                if internal
                    .send(InternalEvent::Discovery {
                        epoch,
                        result: Ok(peers.borrow().clone()),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn spawn_input_permission_monitor(&self) {
        #[cfg(windows)]
        {
            let internal = self.internal_tx.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(1));
                let mut previous = None;
                loop {
                    ticker.tick().await;
                    let ready = crate::input::windows_input_agent_ready();
                    if previous != Some(ready) {
                        previous = Some(ready);
                        if internal.send(InternalEvent::InputElevation(ready)).is_err() {
                            break;
                        }
                    }
                }
            });
        }
    }

    fn spawn_runtime_event_forwarder(
        &self,
        session_id: Uuid,
        mut events: mpsc::UnboundedReceiver<RuntimeEvent>,
        internal: mpsc::UnboundedSender<InternalEvent>,
    ) {
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if internal
                    .send(InternalEvent::Runtime { session_id, event })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    fn save_config(&mut self) {
        if let Ok(latest) = SynlyConfig::load_or_create() {
            self.config.trusted_devices = latest.trusted_devices;
        }
        if let Err(error) = self.config.save() {
            self.set_error(format!("无法保存设置: {error:#}"));
        }
        self.snapshot.trusted_devices = self.config.trusted_devices.clone();
    }

    fn set_error(&mut self, error: String) {
        tracing::error!(error = %error, "应用状态进入错误");
        self.snapshot.lifecycle = AppLifecycle::Error;
        self.snapshot.last_error = Some(error);
    }

    fn publish(&self) {
        self.snapshot_tx.send_replace(self.snapshot.clone());
    }
}

fn apply_settings_to_config(
    config: &mut SynlyConfig,
    runtime: &RuntimeConfig,
    settings: &AppSettings,
) {
    config.device.device_name = settings.device_name.trim().to_string();
    config.clipboard = settings.clipboard.clone();
    config.transfer = settings.transfer.clone();
    config.discovery = settings.discovery.clone();
    config.ui = settings.ui.clone();
    config.notifications.enabled = settings.notifications_enabled;
    config.runtime = runtime.clone();
}

fn validate_settings(config: &SynlyConfig) -> Result<()> {
    if config.device.device_name.trim().is_empty() {
        anyhow::bail!("设备名不能为空");
    }
    if config.clipboard.max_file_bytes == 0 {
        anyhow::bail!("剪贴板单文件限制必须大于 0");
    }
    if config.clipboard.max_cache_bytes == Some(0) {
        anyhow::bail!("剪贴板缓存限制必须大于 0, 或留空表示不限制");
    }
    config.transfer.to_limits()?;
    discovery::validate_config(&config.discovery)
}

fn tuning_from_options(
    options: &crate::cli::RuntimeOptions,
    input_backend_generation: u64,
) -> RuntimeTuning {
    let mut tuning = options.control.tuning().borrow().clone();
    tuning.input_backend_generation = input_backend_generation;
    tuning
}

fn requires_reconnect(previous: &RuntimeConfig, next: &RuntimeConfig) -> bool {
    previous.connection != next.connection
        || previous.peer_query != next.peer_query
        || previous.port != next.port
        || previous.file_sync_mode != next.file_sync_mode
        || previous.paths != next.paths
        || previous.initial != next.initial
        || previous.max_folder_depth != next.max_folder_depth
        || previous.trusted_only != next.trusted_only
}

fn capability_fields_changed(
    applied: Option<&RuntimeConfig>,
    desired: &RuntimeConfig,
) -> bool {
    applied.is_some_and(|applied| {
        applied.clipboard_mode != desired.clipboard_mode
            || applied.audio_mode != desired.audio_mode
            || applied.input_mode != desired.input_mode
    })
}

fn map_lifecycle(lifecycle: RuntimeLifecycle) -> AppLifecycle {
    match lifecycle {
        RuntimeLifecycle::Idle => AppLifecycle::Idle,
        RuntimeLifecycle::Hosting => AppLifecycle::Hosting,
        RuntimeLifecycle::Discovering => AppLifecycle::Discovering,
        RuntimeLifecycle::Connecting => AppLifecycle::Connecting,
        RuntimeLifecycle::Pairing => AppLifecycle::Pairing,
        RuntimeLifecycle::Connected => AppLifecycle::Connected,
        RuntimeLifecycle::Reconfiguring => AppLifecycle::Reconfiguring,
        RuntimeLifecycle::Error => AppLifecycle::Error,
        RuntimeLifecycle::Stopping => AppLifecycle::Stopping,
    }
}

fn peer_view(peer: DiscoveredPeer, config: &SynlyConfig) -> DiscoveredPeerView {
    let device_id = Uuid::parse_str(&peer.device_id).ok();
    let display_name = peer.display_name();
    DiscoveredPeerView {
        trusted: device_id
            .as_ref()
            .is_some_and(|device_id| config.trusted_device(device_id).is_some()),
        device_id: peer.device_id,
        display_name,
        addresses: peer
            .addresses
            .iter()
            .map(|address| format!("{address}:{}", peer.port))
            .collect(),
        source: peer.source.label().to_string(),
        protocol_version: peer.protocol_version,
        compatible: peer.protocol_version == PROTOCOL_VERSION,
        file_mode: peer.file_sync_mode.label().to_string(),
        clipboard_mode: peer.clipboard_mode.label().to_string(),
        audio_mode: peer.audio_mode.label().to_string(),
        input_mode: peer.input_mode.label().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ClipboardConfig, DeviceConfig, DiscoveryConfig, NotificationConfig, TransferConfig,
        UiConfig,
    };
    use crate::input::InputMode;
    use crate::protocol::CapabilityEpoch;
    use crate::settings::{AudioMode, ClipboardMode, FileSyncMode};

    #[test]
    fn reconnect_classification_keeps_hot_fields_in_session() {
        let current = RuntimeConfig::default();
        let mut hot = current.clone();
        hot.clipboard_mode = ClipboardMode::Both;
        hot.audio_mode = AudioMode::Receive;
        hot.input_mode = InputMode::Receive;
        hot.interval_secs += 1;
        hot.sync_delete = !hot.sync_delete;
        hot.instance_name = "desk-b".to_string();
        assert!(!requires_reconnect(&current, &hot));

        let mut workspace = current.clone();
        workspace.file_sync_mode = FileSyncMode::Receive;
        assert!(requires_reconnect(&current, &workspace));

        let mut peer = current.clone();
        peer.peer_query = "peer-b".to_string();
        assert!(requires_reconnect(&current, &peer));
    }

    #[test]
    fn acknowledged_capabilities_promote_pending_settings() {
        let (mut supervisor, _) = AppSupervisor::new(test_config());
        let mut desired = supervisor.snapshot.desired.clone();
        desired.clipboard_mode = ClipboardMode::Both;
        supervisor.snapshot.desired = desired.clone();
        supervisor.snapshot.applied = Some(RuntimeConfig::default());
        supervisor.snapshot.pending = Some(desired);

        let capabilities = RuntimeCapabilities {
            clipboard_mode: ClipboardMode::Both,
            audio_mode: AudioMode::Off,
            input_mode: InputMode::Off,
        };
        supervisor.handle_runtime_event(RuntimeEvent::Capabilities {
            local: capabilities,
            remote: capabilities,
            epoch: CapabilityEpoch {
                host_generation: 1,
                client_generation: 1,
            },
            acknowledged: true,
        });

        assert!(supervisor.snapshot.pending.is_none());
        assert_eq!(
            supervisor.snapshot.applied.as_ref().unwrap().clipboard_mode,
            ClipboardMode::Both
        );
        assert!(supervisor.snapshot.capabilities_acknowledged);
    }

    #[test]
    fn unavailable_elevated_agent_does_not_disable_base_input() {
        let (mut supervisor, _) = AppSupervisor::new(test_config());
        supervisor.snapshot.input_elevation_ready = false;
        supervisor.snapshot.desired.input_mode = InputMode::Receive;

        assert_eq!(
            supervisor.current_capabilities().input_mode,
            InputMode::Receive
        );
    }

    #[tokio::test]
    async fn stale_session_finish_does_not_clear_current_session() {
        let (mut supervisor, _) = AppSupervisor::new(test_config());
        let current_session_id = Uuid::new_v4();
        let stale_session_id = Uuid::new_v4();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_shutdown.cancelled().await;
        });
        let capabilities = RuntimeCapabilities {
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: InputMode::Off,
        };
        let (capabilities, _) = watch::channel(capabilities);
        let (tuning, _) = watch::channel(RuntimeTuning {
            interval_secs: 3,
            sync_delete: false,
            notifications_enabled: true,
            input_backend_generation: 0,
            device_name: "test-device".to_string(),
            instance_name: None,
            discovery: DiscoveryConfig::default(),
            input: crate::input::InputRuntimeOptions {
                mode: InputMode::Off,
                edge: crate::input::ScreenEdge::Right,
                hotkey: crate::input::Hotkey::DEFAULT.parse().unwrap(),
            },
            clipboard: crate::clipboard::ClipboardRuntimeOptions {
                max_file_bytes: 1,
                max_cache_bytes: None,
                cache_dir: std::path::PathBuf::from("."),
            },
        });
        supervisor.session = Some(SessionHandle {
            id: current_session_id,
            shutdown: shutdown.clone(),
            capabilities,
            tuning,
            task,
        });
        supervisor.snapshot.applied = Some(RuntimeConfig::default());

        supervisor
            .handle_internal_event(InternalEvent::SessionFinished {
                session_id: stale_session_id,
                result: Ok(()),
            })
            .await;

        assert_eq!(
            supervisor.session.as_ref().map(|session| session.id),
            Some(current_session_id)
        );
        assert!(supervisor.snapshot.applied.is_some());
        shutdown.cancel();
        supervisor.session.take().unwrap().task.await.unwrap();
    }

    fn test_config() -> SynlyConfig {
        SynlyConfig {
            device: DeviceConfig {
                device_id: Uuid::nil(),
                device_name: "test-device".to_string(),
                identity_private_key: None,
                identity_public_key: None,
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: UiConfig::default(),
            runtime: RuntimeConfig::default(),
            trusted_devices: Vec::new(),
        }
    }
}
