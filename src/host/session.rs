use crate::app::{
    notification_peer, run_sync_session, run_with_session_notifications, AuthenticatedSession,
    SyncSessionOptions,
};
use crate::host::clipboard_hub::ClipboardHubHandle;
use crate::host::{HostEvent, SessionCapabilityProfile, runtime_options_for_profile};
use crate::input::{InputSocketConnection, InputSocketInbox};
use crate::runtime_control::RuntimeEvent;
use crate::runtime_options::RuntimeOptions;
use crate::system_notification::SystemNotifier;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 输入辅助连接的共享路由表: 输入 session_id -> 会话的接入队列.
#[derive(Default)]
pub struct InputRouteRegistry {
    routes: Mutex<HashMap<Uuid, mpsc::Sender<InputSocketConnection>>>,
}

impl InputRouteRegistry {
    pub fn insert(&self, session_id: Uuid, tx: mpsc::Sender<InputSocketConnection>) {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, tx);
    }

    pub fn remove(&self, session_id: &Uuid) {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    pub fn route(&self, session_id: Uuid, connection: InputSocketConnection) -> bool {
        let Some(tx) = self
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session_id)
            .cloned()
        else {
            return false;
        };
        match tx.try_send(connection) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

/// 为单个已认证连接启动主机会话任务.
///
/// 全量档位的会话会报告 ActiveSession 事件, 会话结束时发送 SessionFinished.
#[allow(clippy::too_many_arguments)]
pub(crate) struct HostSessionTask {
    pub(crate) instance: Uuid,
    pub(crate) task: JoinHandle<()>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_host_session(
    session: AuthenticatedSession,
    options: &RuntimeOptions,
    profile: SessionCapabilityProfile,
    input_routes: Arc<InputRouteRegistry>,
    clipboard_hub: ClipboardHubHandle,
    session_shutdown: CancellationToken,
    events: mpsc::UnboundedSender<HostEvent>,
    notifier: SystemNotifier,
) -> HostSessionTask {
    let device_id = session.remote.device_id;
    let instance = Uuid::new_v4();
    let session_options = runtime_options_for_profile(options, profile);
    let workspace = session_options.workspace.clone();
    let (input_socket_tx, input_socket_rx) = mpsc::channel(4);
    let input_inbox = InputSocketInbox::new(input_socket_rx);
    let (input_session_id_tx, _input_session_id) = watch::channel(None);
    let control = options.control.clone();
    let notifier_for_task = notifier.clone();

    let task = tokio::spawn(async move {
        if profile == SessionCapabilityProfile::Full {
            control.report(RuntimeEvent::ActiveSession { device_id });
        }
        let result = run_with_session_notifications(
            &notifier_for_task,
            notification_peer(&session.remote),
            run_sync_session(
                session,
                &workspace,
                SyncSessionOptions {
                    clipboard_mode: session_options.clipboard_mode,
                    audio_mode: session_options.audio_mode,
                    input_mode: session_options.input_mode,
                    input_options: session_options.input.clone(),
                    input_inbox: Some(input_inbox),
                    input_session_id: Some(input_session_id_tx),
                    input_socket_tx: Some(input_socket_tx),
                    input_routes: Some(input_routes),
                    clipboard_options: &session_options.clipboard,
                    transfer_limits: session_options.transfer_limits,
                    control: control.clone(),
                    clipboard_hub: Some(clipboard_hub),
                    capability_profile: profile,
                    session_shutdown: Some(session_shutdown),
                },
            ),
        )
        .await;
        match result {
            Ok(()) => tracing::info!(%device_id, "同步会话已结束"),
            Err(error) => tracing::warn!(%device_id, error = %error, "同步会话中断"),
        }
        let _ = events.send(HostEvent::SessionFinished { device_id, instance });
    });
    HostSessionTask { instance, task }
}
