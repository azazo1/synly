use crate::cli::normalize_pin;
use crate::config::{
    ClipboardConfig, DiscoveryConfig, LndDiscoveryConfig, RuntimeConfig, SynlyConfig,
    TransferConfig, UiConfig,
};
use crate::core::{AppCommand, AppSettings, AppSnapshot, AppSupervisor};
use crate::input::{InputMode, ScreenEdge};
use crate::runtime_control::{InteractionRequest, InteractionResponse};
use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode,
    LogLevel,
};
use anyhow::{Context, Result};
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, VecModel};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

mod single_instance;

slint::include_modules!();

pub fn run(config: SynlyConfig) -> Result<()> {
    let single_instance = single_instance::SingleInstance::acquire(config.device.device_id)?;
    let listener = match single_instance {
        single_instance::SingleInstance::Primary(listener) => listener,
        single_instance::SingleInstance::ActivatedExisting => return Ok(()),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("synly-runtime")
        .build()
        .context("failed to create Tokio runtime")?;
    let (supervisor, handle) = AppSupervisor::new(config.clone());
    runtime.spawn(supervisor.run());

    let window = AppWindow::new().context("failed to create Slint main window")?;
    window.window().set_size(slint::PhysicalSize::new(
        config.ui.window_width,
        config.ui.window_height,
    ));
    let _single_instance_guard =
        single_instance::SingleInstanceGuard::start(listener, window.as_weak())?;
    let tray = AppTray::new().context("failed to create Slint tray")?;
    apply_settings_to_window(
        &window,
        &config.runtime,
        &AppSettings::from_config(&config),
    );
    apply_snapshot(&window, &tray, &handle.snapshots().borrow(), None);

    let current_interaction = Arc::new(Mutex::new(None::<Uuid>));
    wire_window_callbacks(&window, &handle, Arc::clone(&current_interaction));
    wire_tray_callbacks(&window, &tray, &handle);
    wire_close_to_tray(&window, &handle);
    spawn_snapshot_presenter(
        &runtime,
        &window,
        &tray,
        handle.snapshots(),
        Arc::clone(&current_interaction),
    );
    spawn_log_presenter(&runtime, &window);

    tray.show().context("failed to show Slint tray")?;
    if !config.ui.first_run_completed || !config.ui.start_hidden {
        window.show().context("failed to show Slint window")?;
    }

    slint::run_event_loop().context("Slint event loop failed")?;
    save_window_state(&window, &handle.commands());
    let _ = handle.commands().try_send(AppCommand::Shutdown);
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    Ok(())
}

fn wire_window_callbacks(
    window: &AppWindow,
    handle: &crate::core::AppSupervisorHandle,
    current_interaction: Arc<Mutex<Option<Uuid>>>,
) {
    let commands = handle.commands();
    window.on_start_hosting(move || send_command(&commands, AppCommand::StartHosting));

    let commands = handle.commands();
    window.on_connect_peer(move |peer| {
        send_command(&commands, AppCommand::ConnectPeer(peer.to_string()))
    });

    let commands = handle.commands();
    let weak = window.as_weak();
    window.on_connect_query(move || {
        if let Some(window) = weak.upgrade() {
            send_command(
                &commands,
                AppCommand::ConnectPeer(window.get_peer_query().to_string()),
            );
        }
    });

    let commands = handle.commands();
    window.on_disconnect(move || send_command(&commands, AppCommand::Disconnect));

    let commands = handle.commands();
    let snapshots = handle.snapshots();
    let weak = window.as_weak();
    window.on_apply_settings(move || {
        if let Some(window) = weak.upgrade() {
            match settings_from_window(&window) {
                Ok((runtime, settings, session_pin)) => {
                    let enabling_delete = runtime.sync_delete
                        && !snapshots.borrow().desired.sync_delete;
                    if enabling_delete
                        && rfd::MessageDialog::new()
                            .set_level(rfd::MessageLevel::Warning)
                            .set_title("确认启用删除同步")
                            .set_description(
                                "启用后, 对侧删除可能会删除本机工作区中的对应文件. 保存后将立即重新扫描.",
                            )
                            .set_buttons(rfd::MessageButtons::YesNo)
                            .show()
                            != rfd::MessageDialogResult::Yes
                    {
                        window.set_sync_delete(false);
                        return;
                    }
                    send_command(
                        &commands,
                        AppCommand::ApplySettings {
                            runtime,
                            settings: Box::new(settings),
                            session_pin,
                        },
                    );
                }
                Err(error) => tracing::error!(error = %error, "GUI 设置校验失败"),
            }
        }
    });

    let weak = window.as_weak();
    window.on_choose_path(move || {
        let Some(window) = weak.upgrade() else { return };
        let selected = if window.get_file_mode_index() == 1 {
            rfd::FileDialog::new()
                .pick_files()
                .unwrap_or_default()
        } else {
            rfd::FileDialog::new()
                .pick_folder()
                .into_iter()
                .collect()
        };
        if !selected.is_empty() {
            window.set_path_text(
                selected
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
            );
        }
    });

    let commands = handle.commands();
    window.on_set_clipboard_mode(move |index| {
        send_command(
            &commands,
            AppCommand::SetClipboardMode(clipboard_mode_from_index(index)),
        )
    });

    let commands = handle.commands();
    window.on_set_audio_mode(move |index| {
        send_command(
            &commands,
            AppCommand::SetAudioMode(audio_mode_from_index(index)),
        )
    });

    let commands = handle.commands();
    window.on_set_input_mode(move |index| {
        send_command(
            &commands,
            AppCommand::SetInputMode(input_mode_from_index(index)),
        )
    });

    let commands = handle.commands();
    window.on_request_input_elevation(move || {
        send_command(&commands, AppCommand::RequestInputElevation)
    });

    let commands = handle.commands();
    window.on_revoke_trust(move |device_id| match Uuid::parse_str(device_id.as_str()) {
        Ok(device_id) => send_command(&commands, AppCommand::RevokeTrust(device_id)),
        Err(error) => tracing::warn!(error = %error, "忽略无效的可信设备 ID"),
    });

    let commands = handle.commands();
    let weak = window.as_weak();
    let interaction = Arc::clone(&current_interaction);
    window.on_interaction_submit_pin(move || {
        let Some(window) = weak.upgrade() else { return };
        if let Some(request_id) = interaction.lock().ok().and_then(|guard| *guard) {
            send_command(
                &commands,
                AppCommand::RespondInteraction {
                    request_id,
                    response: InteractionResponse::Pin(window.get_entered_pin().to_string()),
                },
            );
        }
    });

    let commands = handle.commands();
    let weak = window.as_weak();
    let interaction = Arc::clone(&current_interaction);
    window.on_interaction_accept(move || {
        let Some(window) = weak.upgrade() else { return };
        if let Some(request_id) = interaction.lock().ok().and_then(|guard| *guard) {
            let response = if window.get_interaction_kind() == 3 {
                InteractionResponse::Confirm(true)
            } else {
                InteractionResponse::Decision {
                    accepted: true,
                    trust: window.get_interaction_trust(),
                }
            };
            send_command(
                &commands,
                AppCommand::RespondInteraction {
                    request_id,
                    response,
                },
            );
        }
    });

    let commands = handle.commands();
    let weak = window.as_weak();
    window.on_interaction_reject(move || {
        if let Some(window) = weak.upgrade()
            && let Some(request_id) = current_interaction.lock().ok().and_then(|guard| *guard)
        {
            let response = match window.get_interaction_kind() {
                0 => {
                    send_command(&commands, AppCommand::Disconnect);
                    return;
                }
                1 => InteractionResponse::Cancel,
                3 => InteractionResponse::Confirm(false),
                _ => InteractionResponse::Decision {
                    accepted: false,
                    trust: false,
                },
            };
            send_command(
                &commands,
                AppCommand::RespondInteraction {
                    request_id,
                    response,
                },
            );
        }
    });
}

fn wire_tray_callbacks(
    window: &AppWindow,
    tray: &AppTray,
    handle: &crate::core::AppSupervisorHandle,
) {
    let weak = window.as_weak();
    tray.on_open_window(move || {
        if let Some(window) = weak.upgrade() {
            let _ = window.show();
        }
    });

    let commands = handle.commands();
    let snapshots = handle.snapshots();
    tray.on_connect_or_disconnect(move || {
        let snapshot = snapshots.borrow();
        if snapshot.applied.is_some() {
            send_command(&commands, AppCommand::Disconnect);
        } else {
            send_command(&commands, AppCommand::Start);
        }
    });

    let commands = handle.commands();
    let snapshots = handle.snapshots();
    tray.on_toggle_clipboard(move || {
        let mode = if snapshots.borrow().desired.clipboard_mode == ClipboardMode::Off {
            ClipboardMode::Both
        } else {
            ClipboardMode::Off
        };
        send_command(&commands, AppCommand::SetClipboardMode(mode));
    });

    let commands = handle.commands();
    let snapshots = handle.snapshots();
    tray.on_toggle_audio(move || {
        let mode = if snapshots.borrow().desired.audio_mode == AudioMode::Off {
            AudioMode::Receive
        } else {
            AudioMode::Off
        };
        send_command(&commands, AppCommand::SetAudioMode(mode));
    });

    let commands = handle.commands();
    let snapshots = handle.snapshots();
    tray.on_toggle_input(move || {
        let mode = if snapshots.borrow().desired.input_mode == InputMode::Off {
            InputMode::Receive
        } else {
            InputMode::Off
        };
        send_command(&commands, AppCommand::SetInputMode(mode));
    });

    let commands = handle.commands();
    let weak = window.as_weak();
    tray.on_quit(move || {
        if let Some(window) = weak.upgrade() {
            save_window_state(&window, &commands);
        }
        send_command(&commands, AppCommand::Shutdown);
        let _ = slint::quit_event_loop();
    });
}

fn wire_close_to_tray(
    window: &AppWindow,
    handle: &crate::core::AppSupervisorHandle,
) {
    let weak = window.as_weak();
    let commands = handle.commands();
    window.window().on_close_requested(move || {
        let Some(window) = weak.upgrade() else {
            return CloseRequestResponse::HideWindow;
        };
        save_window_state(&window, &commands);
        if window.get_close_to_tray() {
            let _ = window.hide();
            CloseRequestResponse::KeepWindowShown
        } else {
            send_command(&commands, AppCommand::Shutdown);
            let _ = slint::quit_event_loop();
            CloseRequestResponse::HideWindow
        }
    });
}

fn spawn_snapshot_presenter(
    runtime: &tokio::runtime::Runtime,
    window: &AppWindow,
    tray: &AppTray,
    mut snapshots: tokio::sync::watch::Receiver<AppSnapshot>,
    current_interaction: Arc<Mutex<Option<Uuid>>>,
) {
    let window = window.as_weak();
    let tray = tray.as_weak();
    runtime.spawn(async move {
        let mut previous_runtime = snapshots.borrow().desired.clone();
        let mut previous_settings = snapshots.borrow().settings.clone();
        let mut previous_interaction = snapshots
            .borrow()
            .interaction
            .as_ref()
            .map(|interaction| interaction.request.request_id());
        loop {
            if snapshots.changed().await.is_err() {
                break;
            }
            let snapshot = snapshots.borrow().clone();
            let settings_changed = runtime_form_fields_changed(
                &previous_runtime,
                &snapshot.desired,
            ) || snapshot.settings != previous_settings;
            previous_runtime = snapshot.desired.clone();
            previous_settings = snapshot.settings.clone();
            let interaction_id = snapshot
                .interaction
                .as_ref()
                .map(|interaction| interaction.request.request_id());
            let new_interaction = interaction_id.is_some() && interaction_id != previous_interaction;
            previous_interaction = interaction_id;
            let interaction = Arc::clone(&current_interaction);
            let window_weak = window.clone();
            let tray_weak = tray.clone();
            if slint::invoke_from_event_loop(move || {
                let (Some(window), Some(tray)) = (window_weak.upgrade(), tray_weak.upgrade()) else {
                    return;
                };
                apply_snapshot(&window, &tray, &snapshot, Some(&interaction));
                if new_interaction
                    && !window.window().is_visible()
                    && let Some(interaction) = snapshot.interaction.as_ref()
                {
                    let (title, body) = interaction_notification_text(&interaction.request);
                    let window = window.as_weak();
                    crate::system_notification::notify_interaction(
                        snapshot.settings.notifications_enabled,
                        title,
                        body,
                        move || {
                            let window = window.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(window) = window.upgrade() {
                                    let _ = window.show();
                                }
                            });
                        },
                    );
                }
                if settings_changed {
                    apply_settings_to_window(
                        &window,
                        &snapshot.desired,
                        &snapshot.settings,
                    );
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}

fn interaction_notification_text(request: &InteractionRequest) -> (String, String) {
    match request {
        InteractionRequest::ShowHostPin {
            remote_label,
            bootstrap_short,
            ..
        } => (
            "Synly 等待配对".to_string(),
            format!("{remote_label} 正在配对, bootstrap {bootstrap_short}"),
        ),
        InteractionRequest::EnterPin {
            bootstrap_short,
            ..
        } => (
            "Synly 需要输入 PIN".to_string(),
            format!("请核对 bootstrap {bootstrap_short} 并输入 PIN"),
        ),
        InteractionRequest::AcceptPeer {
            display_name,
            device_id,
            ..
        } => (
            "Synly 需要确认同步请求".to_string(),
            format!("{display_name} ({device_id}) 请求建立同步"),
        ),
        InteractionRequest::ConfirmTrust {
            display_name,
            device_id,
            ..
        } => (
            "Synly 需要确认信任".to_string(),
            format!("确认是否信任 {display_name} ({device_id})"),
        ),
        InteractionRequest::Clear { .. } => (String::new(), String::new()),
    }
}

fn apply_snapshot(
    window: &AppWindow,
    tray: &AppTray,
    snapshot: &AppSnapshot,
    current_interaction: Option<&Arc<Mutex<Option<Uuid>>>>,
) {
    let active = snapshot.applied.is_some();
    let peer_label = snapshot
        .current_peer
        .as_deref()
        .and_then(|device_id| {
            snapshot
                .discovered_peers
                .iter()
                .find(|peer| peer.device_id == device_id)
                .map(|peer| peer.display_name.as_str())
        })
        .or(snapshot.current_peer.as_deref())
        .unwrap_or("未连接");
    window.set_lifecycle_text(snapshot.lifecycle.label().into());
    window.set_session_active(active);
    window.set_current_peer(peer_label.into());
    window.set_input_permission_ready(snapshot.input_permission_ready);
    window.set_clipboard_mode_index(clipboard_mode_index(snapshot.desired.clipboard_mode));
    window.set_audio_mode_index(audio_mode_index(snapshot.desired.audio_mode));
    window.set_input_mode_index(input_mode_index(snapshot.desired.input_mode));
    window.set_desired_summary(runtime_capability_summary(&snapshot.desired).into());
    window.set_applied_summary(
        snapshot
            .applied
            .as_ref()
            .map(runtime_capability_summary)
            .unwrap_or_else(|| "未建立会话".to_string())
            .into(),
    );
    window.set_pending_summary(
        snapshot
            .pending
            .as_ref()
            .map(runtime_capability_summary)
            .unwrap_or_else(|| "无".to_string())
            .into(),
    );
    window.set_remote_summary(
        snapshot
            .remote_capabilities
            .map(capability_summary)
            .unwrap_or_else(|| "未协商".to_string())
            .into(),
    );
    window.set_epoch_summary(
        snapshot
            .capability_epoch
            .map(|epoch| {
                format!(
                    "host {}, client {}",
                    epoch.host_generation, epoch.client_generation
                )
            })
            .unwrap_or_else(|| "未建立".to_string())
            .into(),
    );
    window.set_status_detail(
        snapshot
            .last_error
            .as_deref()
            .unwrap_or(if !snapshot.capabilities_acknowledged {
                "能力变更等待对侧确认"
            } else if snapshot.pending.is_some() {
                "设置等待应用"
            } else {
                "运行配置已同步"
            })
            .into(),
    );
    let peers = snapshot
        .discovered_peers
        .iter()
        .map(|peer| PeerRow {
            device_id: peer.device_id.clone().into(),
            title: peer.display_name.clone().into(),
            subtitle: format!(
                "{} | 协议 {} | 文件 {} | 剪贴板 {} | 音频 {} | 输入 {} | {}",
                peer.source,
                peer.protocol_version,
                peer.file_mode,
                peer.clipboard_mode,
                peer.audio_mode,
                peer.input_mode,
                peer.addresses.join(", ")
            )
            .into(),
            compatible: peer.compatible,
            trusted: peer.trusted,
        })
        .collect::<Vec<_>>();
    window.set_peers(ModelRc::new(VecModel::from(peers)));
    let trusted = snapshot
        .trusted_devices
        .iter()
        .map(|device| TrustedRow {
            device_id: device.device_id.to_string().into(),
            title: device.device_name.clone().into(),
            subtitle: format!(
                "{} | 成功会话 {}",
                device.device_id, device.successful_sessions
            )
            .into(),
        })
        .collect::<Vec<_>>();
    window.set_trusted_devices(ModelRc::new(VecModel::from(trusted)));
    tray.set_status_text(format!("Synly {}", snapshot.lifecycle.label()).into());
    tray.set_connected(active);
    tray.set_clipboard_enabled(snapshot.desired.clipboard_mode != ClipboardMode::Off);
    tray.set_audio_enabled(snapshot.desired.audio_mode != AudioMode::Off);
    tray.set_input_enabled(snapshot.desired.input_mode != InputMode::Off);

    apply_interaction(window, snapshot, current_interaction);
}

fn spawn_log_presenter(runtime: &tokio::runtime::Runtime, window: &AppWindow) {
    let window = window.as_weak();
    runtime.spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            ticker.tick().await;
            let logs = crate::tracing_utils::recent_logs();
            let window = window.clone();
            if slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    window.set_log_text(logs.into());
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}

fn apply_interaction(
    window: &AppWindow,
    snapshot: &AppSnapshot,
    current_interaction: Option<&Arc<Mutex<Option<Uuid>>>>,
) {
    let Some(interaction) = snapshot.interaction.as_ref() else {
        window.set_interaction_visible(false);
        if let Some(current) = current_interaction
            && let Ok(mut current) = current.lock()
        {
            *current = None;
        }
        return;
    };
    let request = &interaction.request;
    if let Some(current) = current_interaction
        && let Ok(mut current) = current.lock()
    {
        *current = Some(request.request_id());
    }
    window.set_interaction_visible(true);
    window.set_entered_pin("".into());
    match request {
        InteractionRequest::ShowHostPin {
            remote_label,
            bootstrap_short,
            bootstrap_randomart,
            session_short,
            session_randomart,
            pin,
            ..
        } => {
            window.set_interaction_kind(0);
            window.set_interaction_title("等待对侧输入 PIN".into());
            window.set_interaction_detail(
                format!(
                    "对侧: {remote_label}\nBootstrap: {bootstrap_short}\n会话: {session_short}"
                )
                .into(),
            );
            window.set_interaction_art(
                format!("{bootstrap_randomart}\n\n{session_randomart}").into(),
            );
            window.set_interaction_pin(pin.clone().into());
        }
        InteractionRequest::EnterPin {
            bootstrap_short,
            bootstrap_randomart,
            session_short,
            session_randomart,
            ..
        } => {
            window.set_interaction_kind(1);
            window.set_interaction_title("核对指纹并输入 PIN".into());
            window.set_interaction_detail(
                format!("Bootstrap: {bootstrap_short}\n会话: {session_short}").into(),
            );
            window.set_interaction_art(
                format!("{bootstrap_randomart}\n\n{session_randomart}").into(),
            );
            window.set_interaction_pin("".into());
        }
        InteractionRequest::AcceptPeer {
            display_name,
            device_id,
            summary,
            default_trust,
            ..
        } => {
            window.set_interaction_kind(2);
            window.set_interaction_title("接受同步请求".into());
            window.set_interaction_detail(
                format!("{display_name} ({device_id})\n{}", summary.join("\n")).into(),
            );
            window.set_interaction_art("".into());
            window.set_interaction_pin("".into());
            window.set_interaction_trust(*default_trust);
        }
        InteractionRequest::ConfirmTrust {
            display_name,
            device_id,
            ..
        } => {
            window.set_interaction_kind(3);
            window.set_interaction_title("确认长期信任".into());
            window.set_interaction_detail(
                format!("信任 {display_name} ({device_id})").into(),
            );
            window.set_interaction_art("".into());
            window.set_interaction_pin("".into());
        }
        InteractionRequest::Clear { .. } => window.set_interaction_visible(false),
    }
}

fn apply_settings_to_window(
    window: &AppWindow,
    runtime: &RuntimeConfig,
    settings: &AppSettings,
) {
    window.set_connection_index(match runtime.connection {
        Some(ConnectionPreference::Join) => 1,
        _ => 0,
    });
    window.set_instance_name(runtime.instance_name.clone().into());
    window.set_peer_query(runtime.peer_query.clone().into());
    window.set_path_text(
        runtime
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    window.set_port_text(runtime.port.map(|port| port.to_string()).unwrap_or_default().into());
    window.set_file_mode_index(file_mode_index(runtime.file_sync_mode));
    window.set_initial_index(matches!(runtime.initial, Some(InitialSyncMode::Other)) as i32);
    window.set_sync_delete(runtime.sync_delete);
    window.set_interval_secs(runtime.interval_secs.clamp(1, i32::MAX as u64) as i32);
    window.set_max_depth(
        runtime
            .max_folder_depth
            .map(|depth| depth.min(i32::MAX as usize) as i32)
            .unwrap_or(-1),
    );
    window.set_clipboard_mode_index(clipboard_mode_index(runtime.clipboard_mode));
    window.set_audio_mode_index(audio_mode_index(runtime.audio_mode));
    window.set_input_mode_index(input_mode_index(runtime.input_mode));
    window.set_input_edge_index(input_edge_index(runtime.input_edge));
    window.set_input_hotkey(runtime.input_hotkey.clone().into());
    window.set_accept_untrusted(runtime.accept);
    window.set_trust_device(runtime.trust_device);
    window.set_trusted_only(runtime.trusted_only);
    window.set_device_name(settings.device_name.clone().into());
    window.set_clipboard_max_file_text(settings.clipboard.max_file_bytes.to_string().into());
    window.set_clipboard_max_cache_text(
        settings
            .clipboard
            .max_cache_bytes
            .map(|value| value.to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_clipboard_cache_dir(
        settings
            .clipboard
            .cache_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_transfer_meta_text(settings.transfer.max_meta_bytes.to_string().into());
    window.set_transfer_frame_text(settings.transfer.max_frame_data_bytes.to_string().into());
    window.set_transfer_clipboard_text(
        settings.transfer.max_clipboard_bytes.to_string().into(),
    );
    window.set_mdns_enabled(settings.discovery.mdns_enabled);
    window.set_lnd_enabled(settings.discovery.lnd.is_some());
    window.set_lnd_server_url(
        settings
            .discovery
            .lnd
            .as_ref()
            .map(|config| config.server_url.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_lnd_bearer_token(
        settings
            .discovery
            .lnd
            .as_ref()
            .map(|config| config.bearer_token.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_lnd_discovery_domain(
        settings
            .discovery
            .lnd
            .as_ref()
            .and_then(|config| config.discovery_domain.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_notifications_enabled(settings.notifications_enabled);
    window.set_start_hidden(settings.ui.start_hidden);
    window.set_close_to_tray(settings.ui.close_to_tray);
    window.set_launch_at_login(settings.ui.launch_at_login);
    window.set_resume_last_session(settings.ui.resume_last_session);
    window.set_log_level_index(log_level_index(settings.ui.log_level));
}

fn settings_from_window(window: &AppWindow) -> Result<(RuntimeConfig, AppSettings, Option<String>)> {
    let port_text = window.get_port_text().trim().to_string();
    let port = if port_text.is_empty() {
        None
    } else {
        Some(port_text.parse().context("监听端口不是有效数字")?)
    };
    let paths = window
        .get_path_text()
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect();
    let runtime = RuntimeConfig {
        connection: Some(if window.get_connection_index() == 1 {
            ConnectionPreference::Join
        } else {
            ConnectionPreference::Host
        }),
        instance_name: window.get_instance_name().trim().to_string(),
        peer_query: window.get_peer_query().trim().to_string(),
        port,
        file_sync_mode: file_mode_from_index(window.get_file_mode_index()),
        paths,
        initial: Some(if window.get_initial_index() == 1 {
            InitialSyncMode::Other
        } else {
            InitialSyncMode::This
        }),
        sync_delete: window.get_sync_delete(),
        clipboard_mode: clipboard_mode_from_index(window.get_clipboard_mode_index()),
        audio_mode: audio_mode_from_index(window.get_audio_mode_index()),
        input_mode: input_mode_from_index(window.get_input_mode_index()),
        input_edge: input_edge_from_index(window.get_input_edge_index()),
        input_hotkey: window.get_input_hotkey().trim().to_string(),
        interval_secs: window.get_interval_secs().max(1) as u64,
        max_folder_depth: (window.get_max_depth() >= 0)
            .then_some(window.get_max_depth() as usize),
        accept: window.get_accept_untrusted(),
        trust_device: window.get_trust_device(),
        trusted_only: window.get_trusted_only(),
    };
    let device_name = window.get_device_name().trim().to_string();
    if device_name.is_empty() {
        anyhow::bail!("设备名不能为空");
    }
    let clipboard = ClipboardConfig {
        max_file_bytes: parse_required_u64(
            window.get_clipboard_max_file_text().as_str(),
            "剪贴板单文件限制",
        )?,
        max_cache_bytes: parse_optional_u64(
            window.get_clipboard_max_cache_text().as_str(),
            "剪贴板缓存限制",
        )?,
        cache_dir: optional_path(window.get_clipboard_cache_dir().as_str()),
    };
    let transfer = TransferConfig {
        max_meta_bytes: parse_required_u64(
            window.get_transfer_meta_text().as_str(),
            "元数据帧限制",
        )?,
        max_frame_data_bytes: parse_required_u64(
            window.get_transfer_frame_text().as_str(),
            "传输帧限制",
        )?,
        max_clipboard_bytes: parse_required_u64(
            window.get_transfer_clipboard_text().as_str(),
            "剪贴板帧限制",
        )?,
    };
    let lnd = if window.get_lnd_enabled() {
        let server_url = window.get_lnd_server_url().trim().to_string();
        if server_url.is_empty() {
            anyhow::bail!("启用 LND 时必须填写服务 URL");
        }
        Some(LndDiscoveryConfig {
            server_url,
            bearer_token: window.get_lnd_bearer_token().trim().to_string(),
            discovery_domain: optional_text(window.get_lnd_discovery_domain().as_str()),
        })
    } else {
        None
    };
    let discovery = DiscoveryConfig {
        mdns_enabled: window.get_mdns_enabled(),
        lnd,
    };
    let ui = UiConfig {
        first_run_completed: true,
        start_hidden: window.get_start_hidden(),
        close_to_tray: window.get_close_to_tray(),
        launch_at_login: window.get_launch_at_login(),
        resume_last_session: window.get_resume_last_session(),
        log_level: log_level_from_index(window.get_log_level_index()),
        window_width: window.window().size().width,
        window_height: window.window().size().height,
    };
    let settings = AppSettings {
        device_name,
        clipboard,
        transfer,
        discovery,
        ui,
        notifications_enabled: window.get_notifications_enabled(),
    };
    let session_pin = optional_text(window.get_fixed_pin().as_str())
        .map(|pin| normalize_pin(&pin))
        .transpose()?;
    Ok((runtime, settings, session_pin))
}

fn save_window_state(
    window: &AppWindow,
    commands: &tokio::sync::mpsc::Sender<AppCommand>,
) {
    let size = window.window().size();
    send_command(
        commands,
        AppCommand::SaveWindowState {
            width: size.width,
            height: size.height,
        },
    );
}

fn parse_required_u64(value: &str, label: &str) -> Result<u64> {
    let value = value.trim();
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("{label}不是有效的非负整数"))?;
    if parsed == 0 {
        anyhow::bail!("{label}必须大于 0");
    }
    Ok(parsed)
}

fn parse_optional_u64(value: &str, label: &str) -> Result<Option<u64>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    parse_required_u64(value, label).map(Some)
}

fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn optional_path(value: &str) -> Option<PathBuf> {
    optional_text(value).map(PathBuf::from)
}

fn runtime_form_fields_changed(previous: &RuntimeConfig, next: &RuntimeConfig) -> bool {
    let mut previous = previous.clone();
    previous.clipboard_mode = next.clipboard_mode;
    previous.audio_mode = next.audio_mode;
    previous.input_mode = next.input_mode;
    &previous != next
}

fn runtime_capability_summary(runtime: &RuntimeConfig) -> String {
    let connection = match runtime.connection {
        Some(ConnectionPreference::Host) => "监听",
        Some(ConnectionPreference::Join) => "连接",
        None => "未选择角色",
    };
    format!(
        "{}, 文件 {}, 剪贴板 {}, 音频 {}, 输入 {}",
        connection,
        runtime.file_sync_mode.label(),
        runtime.clipboard_mode.label(),
        runtime.audio_mode.label(),
        runtime.input_mode.label()
    )
}

fn capability_summary(capabilities: crate::protocol::RuntimeCapabilities) -> String {
    format!(
        "剪贴板 {}, 音频 {}, 输入 {}",
        capabilities.clipboard_mode.label(),
        capabilities.audio_mode.label(),
        capabilities.input_mode.label()
    )
}

fn send_command(commands: &tokio::sync::mpsc::Sender<AppCommand>, command: AppCommand) {
    if let Err(error) = commands.try_send(command) {
        tracing::warn!(error = %error, "GUI 命令队列已满或关闭");
    }
}

fn file_mode_index(mode: FileSyncMode) -> i32 {
    match mode {
        FileSyncMode::Off => 0,
        FileSyncMode::Send => 1,
        FileSyncMode::Receive => 2,
        FileSyncMode::Both => 3,
        FileSyncMode::Auto => 4,
    }
}

fn file_mode_from_index(index: i32) -> FileSyncMode {
    match index {
        1 => FileSyncMode::Send,
        2 => FileSyncMode::Receive,
        3 => FileSyncMode::Both,
        4 => FileSyncMode::Auto,
        _ => FileSyncMode::Off,
    }
}

fn clipboard_mode_index(mode: ClipboardMode) -> i32 {
    match mode {
        ClipboardMode::Off => 0,
        ClipboardMode::Send => 1,
        ClipboardMode::Receive => 2,
        ClipboardMode::Both => 3,
    }
}

fn clipboard_mode_from_index(index: i32) -> ClipboardMode {
    match index {
        1 => ClipboardMode::Send,
        2 => ClipboardMode::Receive,
        3 => ClipboardMode::Both,
        _ => ClipboardMode::Off,
    }
}

fn audio_mode_index(mode: AudioMode) -> i32 {
    match mode {
        AudioMode::Off => 0,
        AudioMode::Send => 1,
        AudioMode::Receive => 2,
    }
}

fn audio_mode_from_index(index: i32) -> AudioMode {
    match index {
        1 => AudioMode::Send,
        2 => AudioMode::Receive,
        _ => AudioMode::Off,
    }
}

fn input_mode_index(mode: InputMode) -> i32 {
    match mode {
        InputMode::Off => 0,
        InputMode::Send => 1,
        InputMode::Receive => 2,
    }
}

fn input_mode_from_index(index: i32) -> InputMode {
    match index {
        1 => InputMode::Send,
        2 => InputMode::Receive,
        _ => InputMode::Off,
    }
}

fn input_edge_index(edge: ScreenEdge) -> i32 {
    match edge {
        ScreenEdge::Left => 0,
        ScreenEdge::Right => 1,
        ScreenEdge::Top => 2,
        ScreenEdge::Bottom => 3,
    }
}

fn input_edge_from_index(index: i32) -> ScreenEdge {
    match index {
        0 => ScreenEdge::Left,
        2 => ScreenEdge::Top,
        3 => ScreenEdge::Bottom,
        _ => ScreenEdge::Right,
    }
}

fn log_level_index(level: LogLevel) -> i32 {
    match level {
        LogLevel::Error => 0,
        LogLevel::Warn => 1,
        LogLevel::Info => 2,
        LogLevel::Debug => 3,
        LogLevel::Trace => 4,
    }
}

fn log_level_from_index(index: i32) -> LogLevel {
    match index {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        3 => LogLevel::Debug,
        4 => LogLevel::Trace,
        _ => LogLevel::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::interaction_notification_text;
    use crate::runtime_control::InteractionRequest;
    use uuid::Uuid;

    #[test]
    fn hidden_pairing_notification_does_not_expose_pin() {
        let request = InteractionRequest::ShowHostPin {
            request_id: Uuid::nil(),
            remote_label: "peer".to_string(),
            bootstrap_short: "ABCD".to_string(),
            bootstrap_randomart: String::new(),
            session_short: "EFGH".to_string(),
            session_randomart: String::new(),
            pin: "123456".to_string(),
            fixed_pin: true,
        };

        let (title, body) = interaction_notification_text(&request);

        assert!(!title.contains("123456"));
        assert!(!body.contains("123456"));
        assert!(body.contains("ABCD"));
    }
}
