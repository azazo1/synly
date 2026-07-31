pub(crate) mod active_slot;
pub(crate) mod clipboard_hub;
pub(crate) mod session;

pub(crate) use active_slot::{ActiveSlot, ActiveSlotReserver, SlotReservation};
pub(crate) use clipboard_hub::ClipboardHub;
pub(crate) use session::InputRouteRegistry;

use crate::app::{
    PairingThrottle, configure_session_socket, finish_ctrl_c, handle_incoming_connection,
    identity_display_name, print_host_ready, refresh_runtime_options, run_advertisement_updates,
};
use crate::config::SynlyConfig;
use crate::discovery::{self, Advertisement};
use crate::input;
use crate::protocol::{
    ControlMessage, Frame, FrameWriter, PROTOCOL_VERSION, RuntimeCapabilities, TransferLimits,
};
use crate::runtime_control::{RuntimeCommand, RuntimeEvent, RuntimeLifecycle, RuntimePeerSummary};
use crate::runtime_options::RuntimeOptions;
use crate::settings::AudioMode;
use crate::system_notification::SystemNotifier;
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_HOST_SESSIONS: usize = 8;
const PROMOTION_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 会话能力档位: 全量会话承载文件/音频/输入, 仅剪贴板会话只同步剪贴板.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionCapabilityProfile {
    Full,
    ClipboardOnly,
}

impl SessionCapabilityProfile {
    pub(crate) fn apply(self, capabilities: RuntimeCapabilities) -> RuntimeCapabilities {
        match self {
            Self::Full => capabilities,
            Self::ClipboardOnly => RuntimeCapabilities {
                clipboard_mode: capabilities.clipboard_mode,
                audio_mode: AudioMode::Off,
                input_mode: input::InputMode::Off,
            },
        }
    }
}

async fn write_pairing_busy_error(
    socket: &mut tokio::net::TcpStream,
    transfer_limits: TransferLimits,
) -> Result<()> {
    FrameWriter::with_limits(socket, transfer_limits)
        .write_frame(Frame::Control(ControlMessage::Error {
            message: "host 正在处理其他配对请求, 请稍后再试".to_string(),
        }))
        .await
}

pub(crate) fn runtime_options_for_profile(
    options: &RuntimeOptions,
    profile: SessionCapabilityProfile,
) -> RuntimeOptions {
    let mut filtered = options.clone();
    if profile == SessionCapabilityProfile::ClipboardOnly {
        filtered.workspace = WorkspaceSpec::for_off();
        filtered.audio_mode = AudioMode::Off;
        filtered.input_mode = input::InputMode::Off;
    }
    filtered
}

pub(crate) enum HostEvent {
    PairingComplete {
        session: Box<crate::app::AuthenticatedSession>,
        reservation: SlotReservation,
    },
    SessionFinished {
        device_id: Uuid,
        instance: Uuid,
    },
}

struct HostSessionEntry {
    device_id: Uuid,
    display_name: String,
    order: u64,
    instance: Uuid,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

pub(crate) async fn run_host_runtime(
    config: SynlyConfig,
    mut options: RuntimeOptions,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
) -> Result<()> {
    let device = config.device.clone();
    let notifier = SystemNotifier::new(options.control.tuning());
    let shutdown = options.control.shutdown().clone();
    let mut runtime_capabilities = options.control.capabilities();
    let mut runtime_tuning = options.control.tuning();
    options
        .control
        .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Hosting));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let listener = tokio::select! {
        result = tokio::net::TcpListener::bind(("0.0.0.0", options.pairing.port.unwrap_or(0))) => {
            result.context("failed to bind TCP listener")?
        }
        signal_result = &mut ctrl_c => return finish_ctrl_c(signal_result),
        _ = shutdown.cancelled() => return Ok(()),
    };
    let port = listener.local_addr()?.port();
    let mut advertised_capabilities = options.control.capabilities();
    let initial_capabilities = *advertised_capabilities.borrow_and_update();
    let advertisement_details = Advertisement {
        protocol_version: PROTOCOL_VERSION,
        port,
        device: device.clone(),
        file_sync_mode: options.file_sync_mode,
        clipboard_mode: initial_capabilities.clipboard_mode,
        audio_mode: initial_capabilities.audio_mode,
        input_mode: initial_capabilities.input_mode,
        instance_name: options.instance_name.clone(),
    };
    let advertisement = tokio::select! {
        result = discovery::advertise(&advertisement_details, &options.discovery) => result?,
        signal_result = &mut ctrl_c => return finish_ctrl_c(signal_result),
        _ = shutdown.cancelled() => return Ok(()),
    };
    let advertisement_shutdown = CancellationToken::new();
    let mut advertisement_task = tokio::spawn(run_advertisement_updates(
        advertisement_details,
        options.discovery.clone(),
        advertised_capabilities,
        options.control.tuning(),
        advertisement,
        advertisement_shutdown.clone(),
    ));

    print_host_ready(&device, &options, port);

    let config = Arc::new(Mutex::new(config));
    let pairing_slot = Arc::new(Semaphore::new(1));
    let pairing_throttle = Arc::new(Mutex::new(PairingThrottle::default()));
    let active_slot = Arc::new(std::sync::Mutex::new(ActiveSlot::new()));
    let input_routes = Arc::new(InputRouteRegistry::default());
    let clipboard_hub = ClipboardHub::new(options.clipboard.clone());
    let (host_events_tx, mut host_events_rx) = mpsc::unbounded_channel::<HostEvent>();
    let mut sessions: HashMap<Uuid, HostSessionEntry> = HashMap::new();
    let mut next_order: u64 = 0;
    let mut promotion_ticker = time::interval(PROMOTION_CHECK_INTERVAL);
    promotion_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result = tokio::select! {
        result = async {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, address) = accepted?;
                        configure_session_socket(&socket)?;
                        let mut first_byte = [0u8; 1];
                        let is_input = matches!(
                            time::timeout(Duration::from_millis(100), socket.peek(&mut first_byte)).await,
                            Ok(Ok(1)) if first_byte[0] == b'S'
                        );
                        if is_input {
                            match time::timeout(Duration::from_secs(1), input::read_input_preamble(&mut socket)).await {
                                Ok(Ok(incoming_session_id)) => {
                                    let connection = input::InputSocketConnection::new(
                                        incoming_session_id,
                                        socket,
                                    );
                                    if !input_routes.route(incoming_session_id, connection) {
                                        tracing::warn!(
                                            %address,
                                            %incoming_session_id,
                                            "输入辅助连接没有对应的会话路由, 已拒绝"
                                        );
                                    }
                                }
                                Ok(Err(error)) => {
                                    tracing::warn!(%address, error = %error, "输入辅助连接前导验证失败");
                                }
                                Err(_) => {
                                    tracing::warn!(%address, "输入辅助连接前导读取超时");
                                }
                            }
                            continue;
                        }
                        if sessions.len() >= MAX_HOST_SESSIONS {
                            tracing::warn!(
                                %address,
                                session_count = sessions.len(),
                                "已超过 host 会话上限, 拒绝新连接"
                            );
                            continue;
                        }
                        refresh_runtime_options(
                            &mut *config.lock().await,
                            &mut options,
                            &mut runtime_capabilities,
                            &mut runtime_tuning,
                        );
                        let pairing_options = options.clone();
                        let config_shared = Arc::clone(&config);
                        let throttle_shared = Arc::clone(&pairing_throttle);
                        let slot_shared = Arc::clone(&active_slot);
                        let permit = Arc::clone(&pairing_slot);
                        let events_tx = host_events_tx.clone();
                        tokio::spawn(async move {
                            let mut first_byte = [0u8; 1];
                            let is_tls =
                                match time::timeout(Duration::from_secs(1), socket.peek(&mut first_byte)).await {
                                    Ok(Ok(read)) if read >= 1 => first_byte[0] == 0x16,
                                    Ok(Ok(_)) => return,
                                    Ok(Err(error)) => {
                                        tracing::warn!(%address, error = %error, "读取连接首字节失败");
                                        return;
                                    }
                                    // 首字节未在超时内到达, 按可信 mTLS 连接排队处理, 避免误拒.
                                    Err(_) => true,
                                };
                            let _permit = if is_tls {
                                match permit.acquire_owned().await {
                                    Ok(permit) => permit,
                                    Err(_) => return,
                                }
                            } else {
                                match permit.try_acquire_owned() {
                                    Ok(permit) => permit,
                                    Err(tokio::sync::TryAcquireError::Closed) => return,
                                    Err(tokio::sync::TryAcquireError::NoPermits) => {
                                        tracing::info!(%address, "已有配对或连接处理中, 拒绝新的配对请求");
                                        let _ = write_pairing_busy_error(
                                            &mut socket,
                                            pairing_options.transfer_limits,
                                        )
                                        .await;
                                        return;
                                    }
                                }
                            };
                            let reserver = ActiveSlotReserver::new(slot_shared);
                            let mut config_guard = config_shared.lock().await;
                            let mut throttle = throttle_shared.lock().await;
                            let result = handle_incoming_connection(
                                socket,
                                address,
                                &mut throttle,
                                &mut config_guard,
                                &pairing_options,
                                &reserver,
                            )
                            .await;
                            match result {
                                Ok(Some((session, reservation))) => {
                                    let _ = events_tx.send(HostEvent::PairingComplete {
                                        session: Box::new(session),
                                        reservation,
                                    });
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::warn!(%address, error = %error, "配对流程失败");
                                }
                            }
                        });
                    }
                    event = host_events_rx.recv() => {
                        let Some(event) = event else { break };
                        match event {
                            HostEvent::PairingComplete { session, reservation } => {
                                let device_id = session.remote.device_id;
                                let display_name = identity_display_name(&session.remote);
                                if let Some(existing) = sessions.get(&device_id) {
                                    if !existing.shutdown.is_cancelled() {
                                        tracing::warn!(%device_id, "该设备已有在线会话, 拒绝重复连接");
                                        continue;
                                    }
                                    tracing::info!(%device_id, "旧会话正在退出, 等待结束后接管连接");
                                    if let Some(mut old) = sessions.remove(&device_id) {
                                        let _ = tokio::time::timeout(
                                            Duration::from_secs(3),
                                            &mut old.task,
                                        )
                                        .await;
                                    }
                                }
                                let demote = reservation.claim();
                                if let Some(old) = demote {
                                    tracing::info!(%old, "新活跃会话已建立, 正在降级旧活跃会话");
                                    if let Some(entry) = sessions.get(&old) {
                                        entry.shutdown.cancel();
                                    }
                                }
                                let profile = session.capability_profile;
                                let order = next_order;
                                next_order += 1;
                                let session_shutdown = CancellationToken::new();
                                let host_task = session::spawn_host_session(
                                    *session,
                                    &options,
                                    profile,
                                    Arc::clone(&input_routes),
                                    clipboard_hub.handle(),
                                    session_shutdown.clone(),
                                    host_events_tx.clone(),
                                    notifier.clone(),
                                );
                                sessions.insert(
                                    device_id,
                                    HostSessionEntry {
                                        device_id,
                                        display_name,
                                        order,
                                        instance: host_task.instance,
                                        shutdown: session_shutdown,
                                        task: host_task.task,
                                    },
                                );
                                tracing::info!(%device_id, order, "主机会话已建立");
                            }
                            HostEvent::SessionFinished { device_id, instance } => {
                                let is_current = sessions
                                    .get(&device_id)
                                    .is_some_and(|entry| entry.instance == instance);
                                if !is_current {
                                    continue;
                                }
                                if let Some(entry) = sessions.remove(&device_id) {
                                    let _ = entry.task.await;
                                    let trusted_candidates = {
                                        let config_guard = config.lock().await;
                                        let mut candidates: Vec<(Uuid, u64)> = sessions
                                            .values()
                                            .filter_map(|candidate| {
                                                config_guard
                                                    .trusted_device(&candidate.device_id)
                                                    .map(|_| {
                                                        (candidate.device_id, candidate.order)
                                                    })
                                            })
                                            .collect();
                                        candidates.sort_by_key(|(_, order)| *order);
                                        candidates
                                            .into_iter()
                                            .map(|(device_id, _)| device_id)
                                            .collect::<Vec<_>>()
                                    };
                                    let promoted = active_slot
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .on_session_end(device_id, &trusted_candidates);
                                    if let Some(promoted) = promoted {
                                        tracing::info!(
                                            %promoted,
                                            "活跃会话已断开, 正在提升新的活跃会话"
                                        );
                                        if let Some(entry) = sessions.get(&promoted) {
                                            entry.shutdown.cancel();
                                        }
                                    }
                                    options.control.report(RuntimeEvent::Disconnected(
                                        RuntimePeerSummary {
                                            device_id,
                                            display_name: entry.display_name,
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break };
                        match command {
                            RuntimeCommand::DisconnectPeer(device_id) => {
                                if let Some(entry) = sessions.get(&device_id) {
                                    tracing::info!(%device_id, "收到断开设备请求");
                                    entry.shutdown.cancel();
                                }
                            }
                            RuntimeCommand::SwitchActiveSession(device_id) => {
                                let mut slot = active_slot
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if slot.active() != Some(device_id)
                                    && sessions.contains_key(&device_id)
                                {
                                    slot.request_switch(device_id);
                                    drop(slot);
                                    tracing::info!(%device_id, "收到切换活跃会话请求");
                                    if let Some(entry) = sessions.get(&device_id) {
                                        entry.shutdown.cancel();
                                    }
                                }
                            }
                        }
                    }
                    _ = promotion_ticker.tick() => {
                        let expired = active_slot
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .pending_expired(std::time::Instant::now());
                        if expired {
                            tracing::info!("活跃会话提升等待超时, 槽位已开放");
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            Result::<()>::Ok(())
        } => result,
        result = &mut advertisement_task => {
            result
                .context("发现信息更新任务异常结束")?
                .context("发现信息更新失败")
        },
        signal_result = &mut ctrl_c => finish_ctrl_c(signal_result),
        _ = shutdown.cancelled() => Ok(()),
    };

    advertisement_shutdown.cancel();
    if !advertisement_task.is_finished()
        && tokio::time::timeout(Duration::from_secs(3), &mut advertisement_task)
            .await
            .is_err()
    {
        advertisement_task.abort();
        let _ = advertisement_task.await;
    }
    for entry in sessions.values() {
        entry.shutdown.cancel();
    }
    for entry in sessions.values_mut() {
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut entry.task).await;
    }
    clipboard_hub.abort();
    options
        .control
        .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Idle));
    result
}
