use super::platform::{self, NativeEvent};
use super::{DesktopLayout, DisplayRect, Hotkey, InputMode, Point, ScreenEdge};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};

const MOTION_INTERVAL: Duration = Duration::from_micros(8_333);
const HEALTH_INTERVAL: Duration = Duration::from_millis(250);
const GUI_INTERVAL: Duration = MOTION_INTERVAL;
const JUMP_ZONE_SIZE: i32 = 1;
const EDGE_INSET: i32 = 8;

#[derive(Clone, Debug)]
pub struct MacosMockOptions {
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
    pub width: i32,
    pub height: i32,
}

pub fn run_macos_mock(options: MacosMockOptions) -> Result<()> {
    if options.width <= EDGE_INSET * 2 || options.height <= EDGE_INSET * 2 {
        bail!("mock 虚拟屏幕尺寸过小")
    }
    let edge = CString::new(options.edge.as_arg())?;
    let prepared = unsafe {
        synly_input_mock_gui_prepare(options.width, options.height, edge.as_ptr())
    };
    if prepared != 0 {
        bail!("无法创建 macOS mock GUI")
    }

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("synly-input-macos-mock".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("无法创建 macOS 输入 mock 运行时")?;
            runtime.block_on(run_mock_worker(options, worker_stop))
        })
        .context("无法启动 macOS 输入 mock 线程")?;

    unsafe { synly_input_mock_gui_run() };
    stop.store(true, Ordering::Release);
    match worker.join() {
        Ok(result) => result,
        Err(_) => bail!("macOS 输入 mock 线程异常退出"),
    }
}

async fn run_mock_worker(options: MacosMockOptions, stop: Arc<AtomicBool>) -> Result<()> {
    let mut platform = match platform::start(InputMode::Send, options.hotkey) {
        Ok(platform) => platform,
        Err(error) => {
            update_gui(&GuiUpdate {
                active: false,
                cursor: Point::default(),
                delta: Point::default(),
                key_count: 0,
                button_mask: 0,
                wheel: Point::default(),
                event: &format!("启动失败: {error}"),
            });
            wait_for_shutdown(&stop).await;
            return Err(error);
        }
    };
    let local_layout = platform.backend.layout()?;
    let mock_layout = DesktopLayout::new(vec![DisplayRect {
        x: 0,
        y: 0,
        width: options.width,
        height: options.height,
    }])?;
    let mut session = MockSession::new(
        Arc::clone(&platform.backend),
        local_layout,
        mock_layout,
        options.edge,
    );
    update_gui(&GuiUpdate {
        active: false,
        cursor: Point::default(),
        delta: Point::default(),
        key_count: 0,
        button_mask: 0,
        wheel: Point::default(),
        event: &format!("等待从 {} 边缘接入", options.edge.as_arg()),
    });
    tracing::info!(edge = options.edge.as_arg(), "macOS 输入 mock 已启动");

    let result = run_mock_loop(&mut platform, &mut session, &stop).await;
    session.restore_local();
    unsafe { synly_input_mock_gui_stop() };
    result
}

async fn run_mock_loop(
    platform: &mut platform::PlatformHandle,
    session: &mut MockSession,
    stop: &AtomicBool,
) -> Result<()> {
    let mut motion_tick = time::interval(MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut health_tick = time::interval(HEALTH_INTERVAL);
    health_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut gui_tick = time::interval(GUI_INTERVAL);
    gui_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_delta = Point::default();
    let mut keys = BTreeSet::new();
    let mut buttons = BTreeSet::new();
    let mut wheel = Point::default();
    let mut last_event = "等待输入".to_string();

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("无法监听 Ctrl-C")?;
                tracing::info!("收到 Ctrl-C, 正在恢复 macOS 光标");
                break;
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Key { usage, modifiers, down, repeat } => {
                        if down { keys.insert(usage); } else { keys.remove(&usage); }
                        last_event = format!(
                            "按键 hid=0x{usage:04x}, down={down}, repeat={repeat}, modifiers=0x{:02x}",
                            modifiers.bits(),
                        );
                    }
                    NativeEvent::Button { button, down } => {
                        if down { buttons.insert(button); } else { buttons.remove(&button); }
                        last_event = format!("鼠标按钮 button={button}, down={down}");
                    }
                    NativeEvent::Wheel { x, y } => {
                        wheel.x = wheel.x.saturating_add(x);
                        wheel.y = wheel.y.saturating_add(y);
                        last_event = format!("滚轮 x={x}, y={y}");
                    }
                    NativeEvent::Emergency => {
                        last_event = "紧急热键已恢复本机".to_string();
                        session.restore_local();
                    }
                    NativeEvent::ReliableQueueOverflow => bail!("macOS 输入事件队列已满"),
                    NativeEvent::Failed(message) => bail!("macOS 输入捕获失败: {message}"),
                }
            }
            _ = motion_tick.tick() => {
                let (dx, dy) = platform.motion.take();
                if dx != 0 || dy != 0 {
                    last_delta = Point { x: dx, y: dy };
                    if let Some(event) = session.motion(dx, dy)? {
                        last_event = event;
                    }
                }
            }
            _ = health_tick.tick() => {
                platform.backend.health_check()?;
                if platform.failed.load(Ordering::Acquire) {
                    bail!("macOS 输入后端已失败")
                }
                if platform.overflowed.load(Ordering::Acquire) {
                    bail!("macOS 输入事件队列已满")
                }
            }
            _ = gui_tick.tick() => {
                if stop.load(Ordering::Acquire) || !unsafe { synly_input_mock_gui_is_running() } {
                    break;
                }
                update_gui(&GuiUpdate {
                    active: session.active,
                    cursor: session.remote_cursor,
                    delta: last_delta,
                    key_count: keys.len() as u32,
                    button_mask: button_mask(&buttons),
                    wheel,
                    event: &last_event,
                });
            }
        }
    }
    Ok(())
}

async fn wait_for_shutdown(stop: &AtomicBool) {
    let mut tick = time::interval(GUI_INTERVAL);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                unsafe { synly_input_mock_gui_stop() };
                break;
            }
            _ = tick.tick() => {
                if stop.load(Ordering::Acquire) || !unsafe { synly_input_mock_gui_is_running() } {
                    break;
                }
            }
        }
    }
}

struct MockSession {
    backend: Arc<dyn platform::InputBackend>,
    local_layout: DesktopLayout,
    mock_layout: DesktopLayout,
    source_edge: ScreenEdge,
    active: bool,
    edge_position: f32,
    remote_cursor: Point,
}

impl MockSession {
    fn new(
        backend: Arc<dyn platform::InputBackend>,
        local_layout: DesktopLayout,
        mock_layout: DesktopLayout,
        source_edge: ScreenEdge,
    ) -> Self {
        Self {
            backend,
            local_layout,
            mock_layout,
            source_edge,
            active: false,
            edge_position: 0.5,
            remote_cursor: Point::default(),
        }
    }

    fn motion(&mut self, dx: i32, dy: i32) -> Result<Option<String>> {
        if !self.active {
            let point = self.backend.cursor_position()?;
            if !self.local_layout.is_jump_zone_point(
                self.source_edge,
                point,
                JUMP_ZONE_SIZE,
            ) {
                return Ok(None);
            }
            self.edge_position = self
                .local_layout
                .normalized_edge_position(self.source_edge, point);
            self.remote_cursor = self.mock_layout.point_inside_edge(
                self.source_edge.opposite(),
                self.edge_position,
                EDGE_INSET,
            );
            self.backend.set_capture(true)?;
            self.active = true;
            tracing::info!(
                edge = self.source_edge.as_arg(),
                position = self.edge_position,
                "鼠标已接入 mock 虚拟屏幕"
            );
            return Ok(Some(format!(
                "接入 mock, entry={:.3}, x={}, y={}",
                self.edge_position, self.remote_cursor.x, self.remote_cursor.y,
            )));
        }

        let return_edge = self.source_edge.opposite();
        if let Some(position) = self.mock_layout.crossed_outer_edge_position(
            return_edge,
            self.remote_cursor,
            dx,
            dy,
        ) {
            self.edge_position = position;
            self.restore_local();
            tracing::info!(position, "鼠标已从 mock 返回本机");
            return Ok(Some(format!("从 mock 返回本机, position={position:.3}")));
        }
        self.remote_cursor = self
            .mock_layout
            .move_within_layout(self.remote_cursor, dx, dy);
        Ok(None)
    }

    fn restore_local(&mut self) {
        if !self.active {
            return;
        }
        let target = self.local_layout.point_inside_edge(
            self.source_edge,
            self.edge_position,
            EDGE_INSET,
        );
        let warp_result = self.backend.warp_cursor(target);
        let capture_result = self.backend.set_capture(false);
        if let Err(error) = warp_result {
            tracing::error!(error = %error, "恢复 macOS 本机光标位置失败");
        }
        if let Err(error) = capture_result {
            tracing::error!(error = %error, "恢复 macOS 本机光标显示失败");
        }
        self.active = false;
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        self.restore_local();
    }
}

fn button_mask(buttons: &BTreeSet<u8>) -> u32 {
    buttons.iter().fold(0u32, |mask, button| {
        mask | 1u32.checked_shl(u32::from(button.saturating_sub(1))).unwrap_or(0)
    })
}

struct GuiUpdate<'a> {
    active: bool,
    cursor: Point,
    delta: Point,
    key_count: u32,
    button_mask: u32,
    wheel: Point,
    event: &'a str,
}

fn update_gui(update: &GuiUpdate<'_>) {
    let event = CString::new(update.event.replace('\0', " "))
        .unwrap_or_else(|_| CString::new("无法显示事件").unwrap());
    unsafe {
        synly_input_mock_gui_update(
            update.active,
            update.cursor.x,
            update.cursor.y,
            update.delta.x,
            update.delta.y,
            update.key_count,
            update.button_mask,
            update.wheel.x,
            update.wheel.y,
            event.as_ptr(),
        );
    }
}

#[link(name = "macos_audio", kind = "static")]
unsafe extern "C" {
    fn synly_input_mock_gui_prepare(width: i32, height: i32, source_edge: *const std::ffi::c_char) -> i32;
    fn synly_input_mock_gui_run();
    fn synly_input_mock_gui_is_running() -> bool;
    fn synly_input_mock_gui_stop();
    fn synly_input_mock_gui_update(
        active: bool,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        key_count: u32,
        button_mask: u32,
        wheel_x: i32,
        wheel_y: i32,
        event: *const std::ffi::c_char,
    );
}
