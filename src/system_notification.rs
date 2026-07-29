use notify_rust::Notification;
use std::sync::atomic::{AtomicBool, Ordering};

static NOTIFICATION_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPeer {
    pub display_name: String,
    pub short_device_id: String,
}

pub trait SessionNotifier {
    fn notify(&self, event: ConnectionEvent, peer: &NotificationPeer);
}

#[derive(Clone, Debug)]
pub struct SystemNotifier {
    tuning: tokio::sync::watch::Receiver<crate::runtime_control::RuntimeTuning>,
}

impl SystemNotifier {
    pub fn new(
        tuning: tokio::sync::watch::Receiver<crate::runtime_control::RuntimeTuning>,
    ) -> Self {
        Self { tuning }
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
            }
        ) {
            tracing::warn!(error = %error, "无法启动会话提醒线程");
        }
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
