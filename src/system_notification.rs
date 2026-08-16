use notify_rust::Notification;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

static NOTIFICATION_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);
// 自动重连通常会在数秒内完成, 窗口内重连成功则取消断连提醒.
const DISCONNECT_NOTIFICATION_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPeer {
    pub display_name: String,
    pub short_device_id: String,
    pub device_id: Uuid,
}

pub trait SessionNotifier {
    fn notify(&self, event: ConnectionEvent, peer: &NotificationPeer);
}

#[derive(Clone, Debug)]
pub struct SystemNotifier {
    tuning: tokio::sync::watch::Receiver<crate::runtime_control::RuntimeTuning>,
    input_active: Arc<AtomicBool>,
    pending: Arc<Mutex<NotificationState>>,
}

#[derive(Debug, Default)]
struct NotificationState {
    next_generation: u64,
    pending: HashMap<Uuid, PendingDisconnect>,
}

#[derive(Debug)]
struct PendingDisconnect {
    cancelled: Arc<AtomicBool>,
    generation: u64,
    disconnected_at: Instant,
}

impl SystemNotifier {
    pub fn new(
        tuning: tokio::sync::watch::Receiver<crate::runtime_control::RuntimeTuning>,
        input_active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tuning,
            input_active,
            pending: Arc::new(Mutex::new(NotificationState::default())),
        }
    }

    fn notify_connected(&self, peer: &NotificationPeer) {
        let suppressed = {
            let mut state = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.pending.remove(&peer.device_id).is_some_and(|pending| {
                pending.cancelled.store(true, Ordering::Release);
                pending.disconnected_at.elapsed() < DISCONNECT_NOTIFICATION_DELAY
            })
        };
        if !suppressed {
            show_session_notification(ConnectionEvent::Connected, peer);
        }
    }

    fn notify_disconnected(&self, peer: &NotificationPeer) {
        let key = peer.device_id;
        if self.input_active.load(Ordering::Acquire) {
            // 鼠标正在对侧时断连会直接影响操作, 需要立即提醒.
            self.cancel_pending(&key);
            show_session_notification(ConnectionEvent::Disconnected, peer);
            return;
        }

        let (cancelled, generation, pending) = {
            let mut state = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(previous) = state.pending.get_mut(&key) {
                previous.cancelled.store(true, Ordering::Release);
            }
            let generation = state.next_generation;
            state.next_generation += 1;
            let cancelled = Arc::new(AtomicBool::new(false));
            state.pending.insert(
                key,
                PendingDisconnect {
                    cancelled: Arc::clone(&cancelled),
                    generation,
                    disconnected_at: Instant::now(),
                },
            );
            (cancelled, generation, Arc::clone(&self.pending))
        };
        let peer_for_thread = peer.clone();
        let key_for_thread = key;
        if let Err(error) = std::thread::Builder::new()
            .name("synly-disconnect-notification-delay".to_string())
            .spawn(move || {
                std::thread::sleep(DISCONNECT_NOTIFICATION_DELAY);
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let should_show = {
                    let mut state = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.pending.get(&key_for_thread).is_some_and(|pending| {
                        pending.generation == generation
                            && !pending.cancelled.load(Ordering::Acquire)
                    }) {
                        state.pending.remove(&key_for_thread);
                        true
                    } else {
                        false
                    }
                };
                if should_show {
                    show_session_notification(
                        ConnectionEvent::Disconnected,
                        &peer_for_thread,
                    );
                }
            })
        {
            tracing::warn!(error = %error, "无法启动断连提醒延迟线程");
            self.cancel_pending(&peer.device_id);
            show_session_notification(ConnectionEvent::Disconnected, peer);
        }
    }

    fn cancel_pending(&self, key: &Uuid) {
        if let Ok(mut state) = self.pending.lock()
            && let Some(pending) = state.pending.remove(key)
        {
            pending.cancelled.store(true, Ordering::Release);
        }
    }
}

pub fn notify_interaction(
    enabled: bool,
    title: String,
    body: String,
    on_open: impl Fn() + Send + Sync + 'static,
) {
    if !enabled {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("synly-interaction-notification".to_string())
        .spawn(move || {
            let mut notification = Notification::new();
            notification
                .appname("Synly")
                .summary(&title)
                .body(&body)
                .action("open", "打开 Synly");
            match notification.show() {
                Ok(handle) => handle.wait_for_action(move |action| {
                    if matches!(action, "open" | "default") {
                        on_open();
                    }
                }),
                Err(err) if !NOTIFICATION_ERROR_REPORTED.swap(true, Ordering::Relaxed) => {
                    tracing::warn!(error = %err, "无法发送系统提醒, 后续错误将不再重复显示");
                }
                Err(_) => {}
            }
        })
    {
        tracing::warn!(error = %error, "无法启动配对提醒线程");
    }
}

impl SessionNotifier for SystemNotifier {
    fn notify(&self, event: ConnectionEvent, peer: &NotificationPeer) {
        if !self.tuning.borrow().notifications_enabled {
            return;
        }

        match event {
            ConnectionEvent::Connected => self.notify_connected(peer),
            ConnectionEvent::Disconnected => self.notify_disconnected(peer),
        }
    }
}

fn show_session_notification(event: ConnectionEvent, peer: &NotificationPeer) {
    let (title, body) = notification_text(event, peer);
    if let Err(error) = std::thread::Builder::new()
        .name("synly-session-notification".to_string())
        .spawn(move || {
            let result = Notification::new()
                .appname("Synly")
                .summary(title)
                .body(&body)
                .show();
            if let Err(err) = result
                && !NOTIFICATION_ERROR_REPORTED.swap(true, Ordering::Relaxed)
            {
                tracing::warn!(error = %err, "无法发送系统提醒, 后续错误将不再重复显示");
            }
        })
    {
        tracing::warn!(error = %error, "无法启动会话提醒线程");
    }
}

fn notification_text(
    event: ConnectionEvent,
    peer: &NotificationPeer,
) -> (&'static str, String) {
    match event {
        ConnectionEvent::Connected => (
            "Synly 已连接",
            format!(
                "已连接到 {} ({})",
                peer.display_name, peer.short_device_id
            ),
        ),
        ConnectionEvent::Disconnected => (
            "Synly 已断开",
            format!(
                "与 {} ({}) 的连接已断开",
                peer.display_name, peer.short_device_id
            ),
        ),
    }
}
