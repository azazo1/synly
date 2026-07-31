use crate::clipboard::{
    ClipboardRuntimeOptions, ClipboardSync, ClipboardWatcherHandle, payload_signature,
};
use crate::protocol::ClipboardPayload;
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};
use uuid::Uuid;

const INGEST_CAPACITY: usize = 64;
const SESSION_OUTBOUND_CAPACITY: usize = 16;
const MIN_APPLY_INTERVAL: Duration = Duration::from_millis(200);
const SIGNATURE_DEDUPE_TTL: Duration = Duration::from_secs(10);
const PER_SOURCE_MAX_EVENTS: usize = 10;
const PER_SOURCE_WINDOW: Duration = Duration::from_secs(10);

/// 全 host 共享的剪贴板中枢.
///
/// 只启动一个 OS 剪贴板 watcher, 串行应用远端载荷并广播给所有允许接收的
/// 会话, 同时通过共享的 ClipboardSyncState 抑制 watcher 回音.
pub struct ClipboardHub {
    handle: ClipboardHubHandle,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct ClipboardHubHandle {
    tx: mpsc::Sender<HubEvent>,
}

enum HubEvent {
    Ingest {
        source: Uuid,
        payload: ClipboardPayload,
    },
    Subscribe {
        device_id: Uuid,
        tx: mpsc::Sender<ClipboardPayload>,
    },
    Unsubscribe {
        device_id: Uuid,
    },
    SetReceiveEnabled {
        device_id: Uuid,
        enabled: bool,
    },
    UpdateOptions {
        options: ClipboardRuntimeOptions,
    },
    Local {
        payload: ClipboardPayload,
    },
}

trait ClipboardSink: Send + Sync {
    fn start_watcher(
        &self,
        tx: mpsc::UnboundedSender<ClipboardPayload>,
    ) -> Result<ClipboardWatcherHandle>;
    fn apply_remote(
        &self,
        payload: &ClipboardPayload,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn read_current(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ClipboardPayload>>> + Send + '_>>;
    fn update_options(&self, options: ClipboardRuntimeOptions) -> Result<()>;
    fn note_local(&self, payload: &ClipboardPayload) -> bool;
}

struct SyncSink {
    sync: ClipboardSync,
}

impl ClipboardSink for SyncSink {
    fn start_watcher(
        &self,
        tx: mpsc::UnboundedSender<ClipboardPayload>,
    ) -> Result<ClipboardWatcherHandle> {
        self.sync.start_local_watcher(tx)
    }

    fn apply_remote(
        &self,
        payload: &ClipboardPayload,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(self.sync.apply_remote_payload(payload.clone()))
    }

    fn read_current(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ClipboardPayload>>> + Send + '_>> {
        Box::pin(self.sync.read_local_payload())
    }

    fn update_options(&self, options: ClipboardRuntimeOptions) -> Result<()> {
        self.sync.update_options(options)
    }

    fn note_local(&self, payload: &ClipboardPayload) -> bool {
        self.sync.note_local_payload(payload).unwrap_or(false)
    }
}

struct Subscriber {
    tx: mpsc::Sender<ClipboardPayload>,
    receive_enabled: bool,
    snapshot_sent: bool,
}

struct HubTask {
    sink: Arc<dyn ClipboardSink>,
    events: mpsc::Receiver<HubEvent>,
    local_tx: mpsc::UnboundedSender<ClipboardPayload>,
    local_rx: mpsc::UnboundedReceiver<ClipboardPayload>,
    watcher: Option<ClipboardWatcherHandle>,
    subscribers: HashMap<Uuid, Subscriber>,
    policy: HubPolicy,
    next_allowed: Instant,
    pending: Option<HubEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SourceKey {
    Local,
    Remote(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropReason {
    Duplicate,
    RateLimited,
}

#[derive(Default)]
struct HubPolicy {
    seen_signatures: VecDeque<(String, Instant)>,
    per_source: HashMap<SourceKey, VecDeque<Instant>>,
}

impl HubPolicy {
    fn accept(
        &mut self,
        source: SourceKey,
        payload: &ClipboardPayload,
        now: Instant,
    ) -> Result<(), DropReason> {
        self.prune_signatures(now);
        let signature = payload_signature(payload);
        if self
            .seen_signatures
            .iter()
            .any(|(seen, _)| seen == &signature)
        {
            return Err(DropReason::Duplicate);
        }
        let events = self.per_source.entry(source).or_default();
        while let Some(at) = events.front() {
            if now.saturating_duration_since(*at) > PER_SOURCE_WINDOW {
                events.pop_front();
            } else {
                break;
            }
        }
        if events.len() >= PER_SOURCE_MAX_EVENTS {
            return Err(DropReason::RateLimited);
        }
        Ok(())
    }

    fn record(&mut self, source: SourceKey, payload: &ClipboardPayload, now: Instant) {
        self.prune_signatures(now);
        self.seen_signatures
            .push_back((payload_signature(payload), now));
        self.per_source.entry(source).or_default().push_back(now);
    }

    fn prune_signatures(&mut self, now: Instant) {
        while let Some((_, at)) = self.seen_signatures.front() {
            if now.saturating_duration_since(*at) > SIGNATURE_DEDUPE_TTL {
                self.seen_signatures.pop_front();
            } else {
                break;
            }
        }
    }
}

impl ClipboardHub {
    pub fn new(clipboard_options: ClipboardRuntimeOptions) -> Self {
        let sink: Arc<dyn ClipboardSink> = Arc::new(SyncSink {
            sync: ClipboardSync::new(&clipboard_options),
        });
        let (event_tx, event_rx) = mpsc::channel(INGEST_CAPACITY);
        let (local_tx, local_rx) = mpsc::unbounded_channel();
        let handle = ClipboardHubHandle { tx: event_tx };
        let task = tokio::spawn(HubTask {
            sink,
            events: event_rx,
            local_tx: local_tx.clone(),
            local_rx,
            watcher: None,
            subscribers: HashMap::new(),
            policy: HubPolicy::default(),
            next_allowed: Instant::now(),
            pending: None,
        }
        .run());
        Self { handle, task }
    }

    pub fn handle(&self) -> ClipboardHubHandle {
        self.handle.clone()
    }

    pub fn abort(self) {
        self.task.abort();
    }
}

impl ClipboardHubHandle {
    pub fn subscribe(&self, device_id: Uuid) -> mpsc::Receiver<ClipboardPayload> {
        let (tx, rx) = mpsc::channel(SESSION_OUTBOUND_CAPACITY);
        self.send(HubEvent::Subscribe { device_id, tx });
        rx
    }

    pub fn set_receive_enabled(&self, device_id: Uuid, enabled: bool) {
        self.send(HubEvent::SetReceiveEnabled { device_id, enabled });
    }

    pub fn unsubscribe(&self, device_id: Uuid) {
        self.send(HubEvent::Unsubscribe { device_id });
    }

    pub fn ingest(&self, source: Uuid, payload: ClipboardPayload) {
        self.send(HubEvent::Ingest { source, payload });
    }

    pub fn update_options(&self, options: ClipboardRuntimeOptions) {
        self.send(HubEvent::UpdateOptions { options });
    }

    #[cfg(test)]
    pub(crate) fn inject_local(&self, payload: ClipboardPayload) {
        self.send(HubEvent::Local { payload });
    }

    fn send(&self, event: HubEvent) {
        if let Err(error) = self.tx.try_send(event) {
            tracing::warn!(error = %error, "剪贴板中枢事件队列已满, 已丢弃事件");
        }
    }
}

impl HubTask {
    async fn run(mut self) {
        loop {
            let deadline = self.pending.as_ref().map(|_| self.next_allowed);
            let event = tokio::select! {
                event = self.events.recv() => event,
                payload = self.local_rx.recv() => payload.map(|payload| HubEvent::Local { payload }),
                _ = time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    if let Some(event) = self.pending.take() {
                        self.next_allowed = self.process(event).await;
                    }
                    continue;
                }
            };
            let Some(event) = event else { break };
            if self.pending.is_some() {
                self.pending = Some(event);
                continue;
            }
            let now = Instant::now();
            if now < self.next_allowed {
                self.pending = Some(event);
            } else {
                self.next_allowed = self.process(event).await;
            }
        }
    }

    async fn process(&mut self, event: HubEvent) -> Instant {
        match event {
            HubEvent::Subscribe { device_id, tx } => {
                self.subscribers.insert(
                    device_id,
                    Subscriber {
                        tx,
                        receive_enabled: false,
                        snapshot_sent: false,
                    },
                );
                self.ensure_watcher();
            }
            HubEvent::Unsubscribe { device_id } => {
                self.subscribers.remove(&device_id);
                self.maybe_stop_watcher();
            }
            HubEvent::SetReceiveEnabled { device_id, enabled } => {
                let mut push_snapshot = false;
                if let Some(subscriber) = self.subscribers.get_mut(&device_id) {
                    if enabled && !subscriber.receive_enabled {
                        subscriber.receive_enabled = true;
                        push_snapshot = !subscriber.snapshot_sent;
                        subscriber.snapshot_sent = true;
                    } else {
                        subscriber.receive_enabled = enabled;
                    }
                }
                if push_snapshot {
                    match self.sink.read_current().await {
                        Ok(Some(payload)) => self.send_to(device_id, payload),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "读取当前剪贴板快照失败");
                        }
                    }
                }
            }
            HubEvent::UpdateOptions { options } => {
                if let Err(error) = self.sink.update_options(options) {
                    tracing::warn!(error = %error, "更新剪贴板中枢选项失败");
                }
            }
            HubEvent::Ingest { source, payload } => {
                let now = Instant::now();
                match self.policy.accept(SourceKey::Remote(source), &payload, now) {
                    Err(DropReason::Duplicate) => {
                        tracing::debug!(%source, "剪贴板载荷与近期内容重复, 已丢弃");
                    }
                    Err(DropReason::RateLimited) => {
                        tracing::warn!(%source, "剪贴板单源限速触发, 已丢弃载荷");
                    }
                    Ok(()) => {
                        self.policy.record(SourceKey::Remote(source), &payload, now);
                        if let Err(error) = self.sink.apply_remote(&payload).await {
                            tracing::warn!(error = %error, %source, "应用远端剪贴板内容失败");
                        }
                        self.broadcast(payload, Some(source));
                        return now + MIN_APPLY_INTERVAL;
                    }
                }
            }
            HubEvent::Local { payload } => {
                let now = Instant::now();
                match self.policy.accept(SourceKey::Local, &payload, now) {
                    Err(DropReason::Duplicate) => {
                        tracing::debug!("本机剪贴板载荷与近期内容重复, 已丢弃");
                    }
                    Err(DropReason::RateLimited) => {
                        tracing::warn!("本机剪贴板更新过于频繁, 已丢弃载荷");
                    }
                    Ok(()) => {
                        if self.sink.note_local(&payload) {
                            self.policy.record(SourceKey::Local, &payload, now);
                            self.broadcast(payload, None);
                            return now + MIN_APPLY_INTERVAL;
                        }
                    }
                }
            }
        }
        self.next_allowed
    }

    fn broadcast(&mut self, payload: ClipboardPayload, exclude: Option<Uuid>) {
        let mut closed = Vec::new();
        for (device_id, subscriber) in &self.subscribers {
            if Some(*device_id) == exclude || !subscriber.receive_enabled {
                continue;
            }
            match subscriber.tx.try_send(payload.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(%device_id, "会话剪贴板出站队列已满, 已丢弃一次广播");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => closed.push(*device_id),
            }
        }
        for device_id in closed {
            self.subscribers.remove(&device_id);
        }
    }

    fn send_to(&mut self, device_id: Uuid, payload: ClipboardPayload) {
        let Some(subscriber) = self.subscribers.get(&device_id) else {
            return;
        };
        if let Err(error) = subscriber.tx.try_send(payload) {
            tracing::warn!(%device_id, error = %error, "剪贴板快照发送失败");
        }
    }

    fn ensure_watcher(&mut self) {
        if self.watcher.is_some() {
            return;
        }
        match self.sink.start_watcher(self.local_tx.clone()) {
            Ok(watcher) => self.watcher = Some(watcher),
            Err(error) => {
                tracing::warn!(error = %error, "无法启动剪贴板监听, 本次仅接收远端更新");
            }
        }
    }

    fn maybe_stop_watcher(&mut self) {
        if self.subscribers.is_empty() {
            self.watcher = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn text_payload(text: &str) -> ClipboardPayload {
        ClipboardPayload {
            text: Some(text.to_string()),
            rich_text: None,
            html: None,
            image: None,
            files: Vec::new(),
        }
    }

    #[derive(Default)]
    struct FakeSink {
        current: Mutex<Option<ClipboardPayload>>,
        applied: Mutex<Vec<ClipboardPayload>>,
    }

    impl ClipboardSink for FakeSink {
        fn start_watcher(
            &self,
            _tx: mpsc::UnboundedSender<ClipboardPayload>,
        ) -> Result<ClipboardWatcherHandle> {
            anyhow::bail!("fake sink has no watcher")
        }

        fn apply_remote(
            &self,
            payload: &ClipboardPayload,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            let payload = payload.clone();
            Box::pin(async move {
                self.applied.lock().unwrap().push(payload.clone());
                *self.current.lock().unwrap() = Some(payload);
                Ok(())
            })
        }

        fn read_current(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<Option<ClipboardPayload>>> + Send + '_>> {
            Box::pin(async move { Ok(self.current.lock().unwrap().clone()) })
        }

        fn update_options(&self, _options: ClipboardRuntimeOptions) -> Result<()> {
            Ok(())
        }

        fn note_local(&self, _payload: &ClipboardPayload) -> bool {
            true
        }
    }

    fn new_hub() -> (ClipboardHubHandle, JoinHandle<()>) {
        let sink: Arc<dyn ClipboardSink> = Arc::new(FakeSink::default());
        let (event_tx, event_rx) = mpsc::channel(INGEST_CAPACITY);
        let (local_tx, local_rx) = mpsc::unbounded_channel();
        let handle = ClipboardHubHandle { tx: event_tx };
        let task = tokio::spawn(HubTask {
            sink,
            events: event_rx,
            local_tx,
            local_rx,
            watcher: None,
            subscribers: HashMap::new(),
            policy: HubPolicy::default(),
            next_allowed: Instant::now(),
            pending: None,
        }
        .run());
        (handle, task)
    }

    async fn recv_payload(rx: &mut mpsc::Receiver<ClipboardPayload>) -> Option<ClipboardPayload> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn broadcast_excludes_origin_and_honors_receive_enabled() {
        let (hub, task) = new_hub();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut first_rx = hub.subscribe(first);
        let mut second_rx = hub.subscribe(second);
        hub.set_receive_enabled(first, true);
        hub.set_receive_enabled(second, true);

        hub.ingest(first, text_payload("from-first"));

        assert_eq!(recv_payload(&mut first_rx).await, None);
        assert_eq!(
            recv_payload(&mut second_rx).await,
            Some(text_payload("from-first"))
        );
        task.abort();
    }

    #[tokio::test]
    async fn local_payload_reaches_all_enabled_subscribers() {
        let (hub, task) = new_hub();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut first_rx = hub.subscribe(first);
        let mut second_rx = hub.subscribe(second);
        hub.set_receive_enabled(first, true);
        hub.set_receive_enabled(second, true);

        hub.inject_local(text_payload("local"));

        assert_eq!(
            recv_payload(&mut first_rx).await,
            Some(text_payload("local"))
        );
        assert_eq!(
            recv_payload(&mut second_rx).await,
            Some(text_payload("local"))
        );
        task.abort();
    }

    #[tokio::test]
    async fn disabled_subscriber_skips_broadcast() {
        let (hub, task) = new_hub();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut first_rx = hub.subscribe(first);
        let mut second_rx = hub.subscribe(second);
        hub.set_receive_enabled(first, true);

        hub.inject_local(text_payload("local"));

        assert_eq!(
            recv_payload(&mut first_rx).await,
            Some(text_payload("local"))
        );
        assert_eq!(recv_payload(&mut second_rx).await, None);
        task.abort();
    }

    #[tokio::test]
    async fn enable_transition_pushes_current_snapshot_once() {
        let (hub, task) = new_hub();
        let device = Uuid::new_v4();
        let mut rx = hub.subscribe(device);
        hub.ingest(Uuid::new_v4(), text_payload("seed"));
        hub.set_receive_enabled(device, true);

        assert_eq!(recv_payload(&mut rx).await, Some(text_payload("seed")));
        assert_eq!(recv_payload(&mut rx).await, None);
        task.abort();
    }

    #[tokio::test]
    async fn duplicate_signature_is_dropped_within_ttl() {
        let (hub, task) = new_hub();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut first_rx = hub.subscribe(first);
        let mut second_rx = hub.subscribe(second);
        hub.set_receive_enabled(first, true);
        hub.set_receive_enabled(second, true);

        hub.ingest(first, text_payload("same"));
        assert_eq!(
            recv_payload(&mut second_rx).await,
            Some(text_payload("same"))
        );
        hub.ingest(second, text_payload("same"));
        assert_eq!(recv_payload(&mut first_rx).await, None);
        task.abort();
    }

    #[tokio::test]
    async fn per_source_rate_limit_drops_excess_payloads() {
        let (hub, task) = new_hub();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let _first_rx = hub.subscribe(first);
        let mut second_rx = hub.subscribe(second);
        hub.set_receive_enabled(first, true);
        hub.set_receive_enabled(second, true);

        for index in 0..PER_SOURCE_MAX_EVENTS {
            hub.ingest(first, text_payload(&format!("burst-{index}")));
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        hub.ingest(first, text_payload("burst-extra"));

        let mut received = 0;
        while recv_payload(&mut second_rx).await.is_some() {
            received += 1;
        }
        assert_eq!(received, PER_SOURCE_MAX_EVENTS);
        task.abort();
    }

    #[test]
    fn policy_dedupe_expires_after_ttl() {
        let mut policy = HubPolicy::default();
        let now = Instant::now();
        let payload = text_payload("dedupe");
        assert_eq!(policy.accept(SourceKey::Local, &payload, now), Ok(()));
        policy.record(SourceKey::Local, &payload, now);
        assert_eq!(
            policy.accept(SourceKey::Local, &payload, now + Duration::from_secs(1)),
            Err(DropReason::Duplicate)
        );
        assert_eq!(
            policy.accept(
                SourceKey::Local,
                &payload,
                now + SIGNATURE_DEDUPE_TTL + Duration::from_secs(1)
            ),
            Ok(())
        );
    }

    #[test]
    fn policy_rate_limit_is_per_source() {
        let mut policy = HubPolicy::default();
        let now = Instant::now();
        let source = SourceKey::Remote(Uuid::new_v4());
        for index in 0..PER_SOURCE_MAX_EVENTS {
            assert_eq!(
                policy.accept(source, &text_payload(&format!("rate-{index}")), now),
                Ok(())
            );
            policy.record(source, &text_payload(&format!("rate-{index}")), now);
        }
        assert_eq!(
            policy.accept(source, &text_payload("rate-extra"), now),
            Err(DropReason::RateLimited)
        );
        assert_eq!(
            policy.accept(
                SourceKey::Local,
                &text_payload("rate-extra"),
                now
            ),
            Ok(())
        );
    }
}
