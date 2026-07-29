use super::{DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
pub(in crate::input) mod windows;

const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    fn health_check(&self) -> Result<()> {
        Ok(())
    }

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
    fn inject_cursor(&self, point: Point) -> Result<()>;
    fn inject_wheel(&self, x: i32, y: i32) -> Result<()>;
    fn release_all(&self) -> Result<()>;
}

#[derive(Default)]
pub struct MotionAccumulator {
    dx: AtomicI32,
    dy: AtomicI32,
    observed_dx: AtomicI32,
    observed_dy: AtomicI32,
    position: AtomicU64,
    position_valid: AtomicBool,
    position_updated: AtomicBool,
    observed_position_updated: AtomicBool,
}

impl MotionAccumulator {
    pub fn add(&self, dx: i32, dy: i32) {
        self.dx.fetch_add(dx, Ordering::Relaxed);
        self.dy.fetch_add(dy, Ordering::Relaxed);
        self.observed_dx.fetch_add(dx, Ordering::Relaxed);
        self.observed_dy.fetch_add(dy, Ordering::Relaxed);
    }

    pub fn add_at(&self, dx: i32, dy: i32, position: Point) {
        self.position.store(pack_point(position), Ordering::Relaxed);
        self.position_valid.store(true, Ordering::Release);
        self.position_updated.store(true, Ordering::Release);
        self.observed_position_updated.store(true, Ordering::Release);
        self.add(dx, dy);
    }

    pub fn take(&self) -> MotionSample {
        let dx = self.dx.swap(0, Ordering::Relaxed);
        let dy = self.dy.swap(0, Ordering::Relaxed);
        let position_updated = self.position_updated.swap(false, Ordering::AcqRel);
        let position = self
            .position_valid
            .load(Ordering::Acquire)
            .then(|| unpack_point(self.position.load(Ordering::Relaxed)));
        MotionSample { dx, dy, position, position_updated }
    }

    pub fn take_observed(&self) -> MotionSample {
        let position_updated = self.observed_position_updated.swap(false, Ordering::AcqRel);
        let position = self
            .position_valid
            .load(Ordering::Acquire)
            .then(|| unpack_point(self.position.load(Ordering::Relaxed)));
        MotionSample {
            dx: self.observed_dx.swap(0, Ordering::Relaxed),
            dy: self.observed_dy.swap(0, Ordering::Relaxed),
            position,
            position_updated,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MotionSample {
    pub dx: i32,
    pub dy: i32,
    pub position: Option<Point>,
    pub position_updated: bool,
}

fn pack_point(point: Point) -> u64 {
    (u64::from(point.x as u32) << 32) | u64::from(point.y as u32)
}

fn unpack_point(value: u64) -> Point {
    Point {
        x: (value >> 32) as u32 as i32,
        y: value as u32 as i32,
    }
}

pub struct PlatformHandle {
    pub backend: Arc<dyn InputBackend>,
    pub events: mpsc::Receiver<NativeEvent>,
    pub motion: Arc<MotionAccumulator>,
    pub overflowed: Arc<AtomicBool>,
    pub failed: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct CaptureContext {
    pub mode: InputMode,
    pub hotkey: Hotkey,
    pub events: mpsc::Sender<NativeEvent>,
    pub motion: Arc<MotionAccumulator>,
    pub capture_active: Arc<AtomicBool>,
    pub overflowed: Arc<AtomicBool>,
    pub failed: Arc<AtomicBool>,
}

impl CaptureContext {
    pub fn emit_reliable(&self, event: NativeEvent) {
        if matches!(&event, NativeEvent::Failed(_)) {
            self.failed.store(true, Ordering::Release);
        }
        if self.events.try_send(event).is_err() {
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
    let failed = Arc::new(AtomicBool::new(false));
    let context = CaptureContext {
        mode,
        hotkey,
        events: events_tx,
        motion: Arc::clone(&motion),
        capture_active,
        overflowed: Arc::clone(&overflowed),
        failed: Arc::clone(&failed),
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
        failed,
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

#[cfg(test)]
mod tests {
    use super::MotionAccumulator;
    use crate::input::Point;

    #[test]
    fn motion_sample_keeps_latest_position_and_accumulated_delta() {
        let motion = MotionAccumulator::default();
        motion.add_at(3, -2, Point { x: -1200, y: 80 });
        motion.add_at(5, 4, Point { x: -1195, y: 84 });

        let sample = motion.take();
        assert_eq!((sample.dx, sample.dy), (8, 2));
        assert_eq!(sample.position, Some(Point { x: -1195, y: 84 }));
        assert!(sample.position_updated);
        assert!(!motion.take().position_updated);
        let observed = motion.take_observed();
        assert_eq!((observed.dx, observed.dy), (8, 2));
        assert_eq!(observed.position, Some(Point { x: -1195, y: 84 }));
        assert!(observed.position_updated);
    }
}
