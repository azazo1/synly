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

#[derive(Clone, Copy, Debug)]
pub struct SystemNotifier {
    enabled: bool,
}

impl SystemNotifier {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl SessionNotifier for SystemNotifier {
    fn notify(&self, event: ConnectionEvent, peer: &NotificationPeer) {
        if !self.enabled {
            return;
        }

        let (title, body) = notification_text(event, peer);
        tokio::task::spawn_blocking(move || {
            let result = Notification::new()
                .appname("Synly")
                .summary(title)
                .body(&body)
                .show();
            if let Err(err) = result
                && !NOTIFICATION_ERROR_REPORTED.swap(true, Ordering::Relaxed)
            {
                eprintln!("无法发送系统提醒, 后续提醒错误将不再重复显示: {err}");
            }
        });
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
