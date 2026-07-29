use super::{DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub enum NativeEvent {
    Key {
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    },
    Button {
        button: u8,
        down: bool,
    },
    Wheel {
        x: i32,
        y: i32,
    },
    Emergency,
    ReliableQueueOverflow,
    Failed(String),
}

pub trait InputBackend: Send + Sync {
    fn layout(&self) -> Result<DesktopLayout>;
    fn cursor_position(&self) -> Result<Point>;
    fn snapshot(&self) -> KeySnapshot;
    fn set_capture(&self, active: bool) -> Result<()>;
    fn warp_cursor(&self, point: Point) -> Result<()>;
    fn inject_key(
        &self,
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    ) -> Result<()>;
    fn inject_button(&self, button: u8, down: bool) -> Result<()>;
    fn inject_motion(&self, dx: i32, dy: i32) -> Result<()>;
    fn inject_wheel(&self, x: i32, y: i32) -> Result<()>;
    fn release_all(&self) -> Result<()>;
}

#[derive(Default)]
pub struct MotionAccumulator {
    dx: AtomicI32,
    dy: AtomicI32,
}

impl MotionAccumulator {
    pub fn add(&self, dx: i32, dy: i32) {
        self.dx.fetch_add(dx, Ordering::Relaxed);
        self.dy.fetch_add(dy, Ordering::Relaxed);
    }

    pub fn take(&self) -> (i32, i32) {
        (
            self.dx.swap(0, Ordering::Relaxed),
            self.dy.swap(0, Ordering::Relaxed),
        )
    }
}

pub struct PlatformHandle {
    pub backend: Arc<dyn InputBackend>,
    pub events: mpsc::Receiver<NativeEvent>,
    pub motion: Arc<MotionAccumulator>,
    pub overflowed: Arc<AtomicBool>,
}

pub struct CaptureContext {
    pub mode: InputMode,
    pub hotkey: Hotkey,
    pub events: mpsc::Sender<NativeEvent>,
    pub motion: Arc<MotionAccumulator>,
    pub capture_active: Arc<AtomicBool>,
    pub overflowed: Arc<AtomicBool>,
}

impl CaptureContext {
    pub fn emit_reliable(&self, event: NativeEvent) {
        if self.events.try_send(event).is_err() {
            self.capture_active.store(false, Ordering::Release);
            self.overflowed.store(true, Ordering::Release);
            let _ = self.events.try_send(NativeEvent::ReliableQueueOverflow);
        }
    }
}

pub fn start(mode: InputMode, hotkey: Hotkey) -> Result<PlatformHandle> {
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let motion = Arc::new(MotionAccumulator::default());
    let capture_active = Arc::new(AtomicBool::new(false));
    let overflowed = Arc::new(AtomicBool::new(false));
    let context = CaptureContext {
        mode,
        hotkey,
        events: events_tx,
        motion: Arc::clone(&motion),
        capture_active,
        overflowed: Arc::clone(&overflowed),
    };
    #[cfg(target_os = "macos")]
    let backend = macos::start(context)?;
    #[cfg(windows)]
    let backend = windows::start(context)?;
    #[cfg(not(any(target_os = "macos", windows)))]
    let backend = unsupported::start(context)?;
    Ok(PlatformHandle {
        backend,
        events: events_rx,
        motion,
        overflowed,
    })
}

impl Drop for PlatformHandle {
    fn drop(&mut self) {
        let _ = self.backend.set_capture(false);
        let _ = self.backend.release_all();
    }
}

pub fn ensure_permissions(mode: InputMode) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::ensure_permissions(mode);
    #[cfg(windows)]
    return windows::ensure_permissions(mode);
    #[cfg(not(any(target_os = "macos", windows)))]
    return unsupported::ensure_permissions(mode);
}
