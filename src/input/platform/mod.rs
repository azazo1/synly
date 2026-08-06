use super::{DesktopLayout, DisplayRect, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Result, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
pub mod windows;

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
        source: ScrollSource,
    },
    Emergency,
    SecureDesktop {
        active: bool,
        primary: Option<DisplayRect>,
    },
    ReliableQueueOverflow,
    Failed(String),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScrollSource {
    MouseWheel,
    Trackpad,
}

pub trait InputBackend: Send + Sync {
    fn health_check(&self) -> Result<()> {
        Ok(())
    }

    /// 当前平台是否处于安全输入模式, 如 macOS Secure Input.
    /// 该状态可能随时间变化, 输入运行时应当暂停同步而不是终止会话.
    fn secure_input_state(&self) -> bool {
        false
    }

    fn layout(&self) -> Result<DesktopLayout>;
    /// 当前是否处于安全桌面, 以及安全桌面期间光标应钳制的主屏矩形.
    /// 仅在 Windows SYSTEM 输入代理下有意义, 其他平台默认不处于安全桌面.
    fn secure_desktop_state(&self) -> (bool, Option<DisplayRect>) {
        (false, None)
    }
    fn cursor_position(&self) -> Result<Point>;
    fn snapshot(&self) -> KeySnapshot;
    /// 按平台能力校准本机真实按下状态, 并返回校准后的快照.
    /// 默认实现不校准, 只返回当前事件累计快照.
    fn refresh_pressed_state(&self) -> Result<KeySnapshot> {
        Ok(self.snapshot())
    }
    fn set_capture(&self, active: bool) -> Result<()>;
    /// 仅监听键盘事件而不进入完整捕获: 不隐藏光标, 不分离鼠标, 不吞掉按键.
    /// 用于需要读取本机方向键但不想改变光标状态的工具, 默认不做任何事.
    #[allow(dead_code)]
    fn set_keyboard_capture(&self, _active: bool) -> Result<()> {
        Ok(())
    }
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
    fn inject_motion(&self, _dx: i32, _dy: i32) -> Result<()> {
        bail!("相对光标注入在当前平台不可用")
    }
    fn inject_wheel(&self, x: i32, y: i32) -> Result<()>;
    fn release_all(&self) -> Result<()>;
}

/// 前台窗口是否处于"光标捕获"状态(隐藏/锁定光标并只读取相对移动, 类似 MC 的 3D 光标).
///
/// 用于接收端自动切换游戏光标模式, 全屏本身不算, 只有系统级光标捕获信号才算.
pub fn foreground_cursor_captured() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::foreground_cursor_captured()
    }
    #[cfg(windows)]
    {
        windows::foreground_cursor_captured()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

#[derive(Default)]
pub struct MotionAccumulator {
    dx: AtomicI32,
    dy: AtomicI32,
    #[cfg(any(
        windows,
        test,
        all(target_os = "macos", feature = "input-screen-mock")
    ))]
    observed_dx: AtomicI32,
    #[cfg(any(
        windows,
        test,
        all(target_os = "macos", feature = "input-screen-mock")
    ))]
    observed_dy: AtomicI32,
    position: AtomicU64,
    position_valid: AtomicBool,
    position_updated: AtomicBool,
    #[cfg(any(
        windows,
        test,
        all(target_os = "macos", feature = "input-screen-mock")
    ))]
    observed_position_updated: AtomicBool,
}

impl MotionAccumulator {
    pub fn add(&self, dx: i32, dy: i32) {
        self.dx.fetch_add(dx, Ordering::Relaxed);
        self.dy.fetch_add(dy, Ordering::Relaxed);
        #[cfg(any(
            windows,
            test,
            all(target_os = "macos", feature = "input-screen-mock")
        ))]
        {
            self.observed_dx.fetch_add(dx, Ordering::Relaxed);
            self.observed_dy.fetch_add(dy, Ordering::Relaxed);
        }
    }

    pub fn add_at(&self, dx: i32, dy: i32, position: Point) {
        self.position.store(pack_point(position), Ordering::Relaxed);
        self.position_valid.store(true, Ordering::Release);
        self.position_updated.store(true, Ordering::Release);
        #[cfg(any(
            windows,
            test,
            all(target_os = "macos", feature = "input-screen-mock")
        ))]
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

    #[cfg(any(
        windows,
        test,
        all(target_os = "macos", feature = "input-screen-mock")
    ))]
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
    pub filter_app_events: Arc<AtomicBool>,
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

#[allow(dead_code)]
pub fn start(mode: InputMode, hotkey: Hotkey) -> Result<PlatformHandle> {
    start_with_filter(mode, hotkey, false)
}

pub fn start_with_filter(
    mode: InputMode,
    hotkey: Hotkey,
    filter_app_events: bool,
) -> Result<PlatformHandle> {
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let motion = Arc::new(MotionAccumulator::default());
    let capture_active = Arc::new(AtomicBool::new(false));
    let overflowed = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let filter_app_events = Arc::new(AtomicBool::new(filter_app_events));
    let context = CaptureContext {
        mode,
        hotkey,
        events: events_tx,
        motion: Arc::clone(&motion),
        capture_active,
        overflowed: Arc::clone(&overflowed),
        failed: Arc::clone(&failed),
        filter_app_events,
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
