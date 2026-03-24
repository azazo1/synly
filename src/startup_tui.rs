#![allow(clippy::too_many_arguments)]
#![allow(clippy::single_match)]
use crate::cli::{
    AudioMode, Cli, ClipboardMode, ClipboardRuntimeOptions, ConnectionPreference, InitialSyncMode,
    PairingRuntimeOptions, RuntimeOptions, SyncMode, normalize_pin,
};
use crate::config::SynlyConfig;
use crate::path_expand::expand_path_string;
use crate::protocol::TransferLimits;
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Tabs, Wrap},
};
use std::fs;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tui_textarea::TextArea;

const DEFAULT_INTERVAL_SECS: u64 = 3;
const DEFAULT_DISCOVERY_SECS: u64 = 3;
const LOG_LIMIT: usize = 48;
const TICK_RATE: Duration = Duration::from_millis(180);
const MIN_WIDTH: u16 = 96;
const MIN_HEIGHT: u16 = 30;

pub fn collect_runtime_options_tui(cli: Cli, config: &SynlyConfig) -> Result<RuntimeOptions> {
    let context = StartupContext::from_config(config)?;
    let mut app = StartupApp::from_cli(cli, context);
    let options = app.run()?;
    println!(
        "{}",
        equivalent_command_from_options(&options, &app.context.cwd)
    );
    Ok(options)
}

#[derive(Clone)]
struct StartupContext {
    device_label: String,
    cwd: PathBuf,
    clipboard: ClipboardRuntimeOptions,
    transfer_limits: TransferLimits,
}

impl StartupContext {
    fn from_config(config: &SynlyConfig) -> Result<Self> {
        Ok(Self {
            device_label: format!(
                "{} ({})",
                config.device.device_name,
                config.device.short_id()
            ),
            cwd: std::env::current_dir().context("failed to determine current directory")?,
            clipboard: ClipboardRuntimeOptions {
                max_file_bytes: config.clipboard.max_file_bytes,
                max_cache_bytes: config.clipboard.max_cache_bytes,
                cache_dir: config.clipboard_cache_dir()?,
            },
            transfer_limits: config.transfer.to_limits()?,
        })
    }
}

struct StartupApp {
    context: StartupContext,
    flow: FlowDraft,
    workspace: WorkspaceDraft,
    pairing: PairingDraft,
    tab: StartupTab,
    focus_by_tab: [usize; 3],
    editing_input: bool,
    ui_state: UiState,
    logs: Vec<String>,
}

struct FlowDraft {
    connection: ConnectionPreference,
    mode: SyncMode,
    initial_sync: Option<InitialSyncMode>,
    sync_delete: bool,
    clipboard_mode: ClipboardMode,
    audio_mode: AudioMode,
}

struct WorkspaceDraft {
    path: TextArea<'static>,
    max_folder_depth: TextArea<'static>,
    interval_secs: TextArea<'static>,
}

struct PairingDraft {
    instance_name: TextArea<'static>,
    peer_query: TextArea<'static>,
    port: TextArea<'static>,
    pin: TextArea<'static>,
    accept: bool,
    trust_device: bool,
    trusted_only: bool,
    discovery_secs: TextArea<'static>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupTab {
    Flow,
    Workspace,
    Pairing,
}

impl StartupTab {
    const ALL: [Self; 3] = [Self::Flow, Self::Workspace, Self::Pairing];

    fn index(self) -> usize {
        match self {
            Self::Flow => 0,
            Self::Workspace => 1,
            Self::Pairing => 2,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Flow => "Flow",
            Self::Workspace => "Workspace",
            Self::Pairing => "Pairing",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Flow => "切换连接方式、同步方向和基础能力开关。",
            Self::Workspace => "补齐目录、发送路径与文件同步节奏。",
            Self::Pairing => "设置目标设备、PIN、端口与可信设备策略。",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldId {
    Connection,
    Mode,
    InitialSync,
    SyncDelete,
    ClipboardMode,
    AudioMode,
    WorkspacePath,
    MaxFolderDepth,
    IntervalSecs,
    InstanceName,
    PeerQuery,
    Port,
    Pin,
    Accept,
    TrustDevice,
    TrustedOnly,
    DiscoverySecs,
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    panel: Color,
    text: Color,
    muted: Color,
    tag_text: Color,
    border: Color,
    primary: Color,
    primary_soft: Color,
    success: Color,
    warning: Color,
    danger: Color,
}

fn palette() -> Palette {
    Palette {
        background: Color::Rgb(10, 16, 24),
        panel: Color::Rgb(19, 28, 39),
        text: Color::Rgb(235, 241, 247),
        muted: Color::Rgb(132, 149, 168),
        tag_text: Color::Rgb(184, 198, 214),
        border: Color::Rgb(52, 69, 90),
        primary: Color::Rgb(92, 188, 255),
        primary_soft: Color::Rgb(34, 49, 68),
        success: Color::Rgb(104, 214, 160),
        warning: Color::Rgb(255, 191, 109),
        danger: Color::Rgb(255, 121, 121),
    }
}

struct PreviewModel {
    summary_lines: Vec<String>,
    notes: Vec<String>,
    errors: Vec<String>,
    command_line: String,
    command_is_valid: bool,
}

struct WorkspacePreview {
    lines: Vec<String>,
    can_receive: bool,
    notes: Vec<String>,
}

#[derive(Default)]
struct UiState {
    tab_areas: Vec<(StartupTab, Rect)>,
    field_areas: Vec<(FieldId, Rect)>,
    selector_areas: Vec<(FieldId, usize, Rect)>,
    launch_button_area: Option<Rect>,
}

impl UiState {
    fn clear(&mut self) {
        self.tab_areas.clear();
        self.field_areas.clear();
        self.selector_areas.clear();
        self.launch_button_area = None;
    }
}

impl StartupApp {
    fn from_cli(cli: Cli, context: StartupContext) -> Self {
        let connection = if cli.host {
            ConnectionPreference::Host
        } else if cli.join {
            ConnectionPreference::Join
        } else {
            ConnectionPreference::Host
        };

        let mode = cli.fs.unwrap_or(SyncMode::Off);

        let workspace_value = if mode == SyncMode::Off {
            String::new()
        } else {
            cli.paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };

        let initial_tab = if mode != SyncMode::Off && workspace_value.trim().is_empty() {
            StartupTab::Workspace
        } else {
            StartupTab::Flow
        };
        let workspace_missing = mode != SyncMode::Off && workspace_value.trim().is_empty();

        let mut app = Self {
            context,
            flow: FlowDraft {
                connection,
                mode,
                initial_sync: cli.initial,
                sync_delete: cli.sync_delete,
                clipboard_mode: cli.clipboard.unwrap_or(ClipboardMode::Off),
                audio_mode: cli.audio.unwrap_or(AudioMode::Off),
            },
            workspace: WorkspaceDraft {
                path: single_line_textarea(workspace_value, ""),
                max_folder_depth: single_line_textarea(
                    cli.max_folder_depth
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    "留空表示不限制",
                ),
                interval_secs: single_line_textarea(
                    cli.interval_secs.to_string(),
                    "留空恢复默认 3 秒",
                ),
            },
            pairing: PairingDraft {
                instance_name: single_line_textarea(
                    cli.name.unwrap_or_default(),
                    "留空表示沿用设备名，仅作用于本次运行",
                ),
                peer_query: single_line_textarea(
                    cli.peer.unwrap_or_default(),
                    "留空则在启动后搜索并选择设备",
                ),
                port: single_line_textarea(
                    cli.port.map(|value| value.to_string()).unwrap_or_default(),
                    "Host 模式生效；留空随机分配",
                ),
                pin: single_line_textarea(cli.pin.unwrap_or_default(), "留空则运行时再输入或显示"),
                accept: cli.accept,
                trust_device: cli.trust_device,
                trusted_only: cli.trusted_only,
                discovery_secs: single_line_textarea(
                    cli.discovery_secs.to_string(),
                    "留空恢复默认 3 秒",
                ),
            },
            tab: initial_tab,
            focus_by_tab: [0, 0, 0],
            editing_input: false,
            ui_state: UiState::default(),
            logs: Vec::new(),
        };

        app.push_log("缺少完整启动参数，已切换到 Launchpad。");
        app.push_log("Flow / Workspace / Pairing 三个分区都可以继续编辑。");
        if workspace_missing {
            app.push_log("Workspace 目前为空，请前往 Workspace 分区填写目录或文件。");
        }
        app.push_log("按 Ctrl+S 启动，按 q 退出；输入框内按 Esc 返回浏览。");
        app
    }

    fn run(&mut self) -> Result<RuntimeOptions> {
        let mut terminal = ratatui::try_init().context("failed to initialize startup TUI")?;
        if let Err(err) = execute!(stdout(), EnableMouseCapture) {
            ratatui::restore();
            return Err(err).context("failed to enable mouse capture");
        }
        let result = self.run_loop(&mut terminal);
        let _ = execute!(stdout(), DisableMouseCapture);
        let _ = terminal.show_cursor();
        ratatui::restore();
        result
    }

    fn run_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<RuntimeOptions> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;

            if !event::poll(TICK_RATE)? {
                continue;
            }

            match event::read()? {
                Event::Key(key) => {
                    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        continue;
                    }

                    if let Some(options) = self.handle_key(key)? {
                        return Ok(options);
                    }
                }
                Event::Mouse(mouse) => {
                    if let Some(options) = self.handle_mouse(mouse)? {
                        return Ok(options);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<Option<RuntimeOptions>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => bail!("已取消启动"),
                KeyCode::Char('s') => return self.try_launch(),
                KeyCode::Left => {
                    if self.is_editing_input() {
                        self.handle_active_textarea_key(key);
                    } else {
                        self.switch_tab(-1);
                    }
                    return Ok(None);
                }
                KeyCode::Right => {
                    if self.is_editing_input() {
                        self.handle_active_textarea_key(key);
                    } else {
                        self.switch_tab(1);
                    }
                    return Ok(None);
                }
                _ => {}
            }
        }

        if self.is_editing_input() {
            return self.handle_key_while_editing(key);
        }

        let key = remap_navigation_key(key);
        match key.code {
            KeyCode::Char('q') => bail!("已取消启动"),
            KeyCode::F(1) => {
                self.set_tab(StartupTab::Flow);
                Ok(None)
            }
            KeyCode::F(2) => {
                self.set_tab(StartupTab::Workspace);
                Ok(None)
            }
            KeyCode::F(3) => {
                self.set_tab(StartupTab::Pairing);
                Ok(None)
            }
            KeyCode::Char('[') => {
                self.switch_tab(-1);
                Ok(None)
            }
            KeyCode::Char(']') => {
                self.switch_tab(1);
                Ok(None)
            }
            KeyCode::Tab | KeyCode::Down => {
                self.move_focus(1);
                Ok(None)
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_focus(-1);
                Ok(None)
            }
            KeyCode::Enter => {
                if self.current_field_accepts_text() {
                    self.editing_input = true;
                } else {
                    self.handle_field_input(key);
                }
                Ok(None)
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.current_field_accepts_text()
                    && !ch.is_control() =>
            {
                self.editing_input = true;
                self.handle_active_textarea_key(key);
                Ok(None)
            }
            _ => {
                self.handle_field_input(key);
                Ok(None)
            }
        }
    }

    fn handle_key_while_editing(&mut self, key: KeyEvent) -> Result<Option<RuntimeOptions>> {
        match key.code {
            KeyCode::Esc => {
                self.editing_input = false;
                self.push_log("已退出输入编辑。");
                Ok(None)
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.editing_input = false;
                self.move_focus(1);
                Ok(None)
            }
            KeyCode::BackTab => {
                self.editing_input = false;
                self.move_focus(-1);
                Ok(None)
            }
            _ => {
                self.handle_active_textarea_key(key);
                Ok(None)
            }
        }
    }

    fn handle_field_input(&mut self, key: KeyEvent) {
        match self.current_field() {
            FieldId::Connection => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.connection = match self.flow.connection {
                        ConnectionPreference::Host => ConnectionPreference::Join,
                        ConnectionPreference::Join => ConnectionPreference::Host,
                    };
                    self.push_log(format!(
                        "连接方式已切换为{}。",
                        connection_label(self.flow.connection)
                    ));
                }
            }
            FieldId::Mode => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.mode = cycle_mode(self.flow.mode, matches!(key.code, KeyCode::Left));
                    self.push_log(format!("文件同步模式已切换为{}。", self.flow.mode.label()));
                    self.clamp_focus_current_tab();
                }
            }
            FieldId::InitialSync => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.initial_sync = cycle_initial_sync(
                        self.flow.initial_sync,
                        matches!(key.code, KeyCode::Left),
                    );
                    self.push_log(match self.flow.initial_sync {
                        Some(mode) => format!("初始状态来源已切换为{}。", mode.label()),
                        None => "已清除初始状态来源选择。".to_string(),
                    });
                }
            }
            FieldId::SyncDelete => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.sync_delete = !self.flow.sync_delete;
                    self.push_log(format!(
                        "删除同步已{}。",
                        enabled_label(self.flow.sync_delete)
                    ));
                }
            }
            FieldId::ClipboardMode => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.clipboard_mode = cycle_clipboard_mode(
                        self.flow.clipboard_mode,
                        matches!(key.code, KeyCode::Left),
                    );
                    self.push_log(format!(
                        "剪贴板同步已切换为{}。",
                        self.flow.clipboard_mode.label()
                    ));
                }
            }
            FieldId::AudioMode => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.flow.audio_mode =
                        cycle_audio_mode(self.flow.audio_mode, matches!(key.code, KeyCode::Left));
                    self.push_log(format!(
                        "音频同步已切换为{}。",
                        self.flow.audio_mode.label()
                    ));
                }
            }
            FieldId::WorkspacePath => {
                let _ = key;
            }
            FieldId::MaxFolderDepth => {
                let _ = key;
            }
            FieldId::IntervalSecs => {
                let _ = key;
            }
            FieldId::PeerQuery => {
                let _ = key;
            }
            FieldId::InstanceName => {
                let _ = key;
            }
            FieldId::Port => {
                let _ = key;
            }
            FieldId::Pin => {
                let _ = key;
            }
            FieldId::Accept => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.pairing.accept = !self.pairing.accept;
                    self.push_log(format!(
                        "认证通过后的自动接受已{}。",
                        enabled_label(self.pairing.accept)
                    ));
                }
            }
            FieldId::TrustDevice => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.pairing.trust_device = !self.pairing.trust_device;
                    self.push_log(format!(
                        "可信设备绑定倾向已{}。",
                        enabled_label(self.pairing.trust_device)
                    ));
                }
            }
            FieldId::TrustedOnly => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')
                ) {
                    self.pairing.trusted_only = !self.pairing.trusted_only;
                    self.push_log(format!(
                        "仅可信设备模式已{}。",
                        enabled_label(self.pairing.trusted_only)
                    ));
                }
            }
            FieldId::DiscoverySecs => {
                let _ = key;
            }
        }
    }

    fn handle_active_textarea_key(&mut self, key: KeyEvent) {
        if let Some(textarea) = self.active_textarea_mut() {
            let _ = textarea.input(key);
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<Option<RuntimeOptions>> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_left_click(mouse.column, mouse.row)
            }
            _ => Ok(None),
        }
    }

    fn handle_left_click(&mut self, column: u16, row: u16) -> Result<Option<RuntimeOptions>> {
        if let Some(rect) = self.ui_state.launch_button_area
            && rect_contains(rect, column, row)
        {
            return self.try_launch();
        }

        if let Some(tab) = self.tab_at(column, row) {
            self.set_tab(tab);
            return Ok(None);
        }

        if let Some(field) = self.field_at(column, row) {
            self.select_field(field);
            if self.field_is_text(field) {
                self.editing_input = true;
                return Ok(None);
            }
            self.editing_input = false;
            self.handle_field_click(field, column, row);
            return Ok(None);
        }

        self.editing_input = false;
        Ok(None)
    }

    fn try_launch(&mut self) -> Result<Option<RuntimeOptions>> {
        match self.build_runtime_options() {
            Ok(options) => {
                self.push_log("配置校验通过，准备进入同步主流程。");
                Ok(Some(options))
            }
            Err(err) => {
                self.push_log(format!("启动前校验失败: {err}"));
                Ok(None)
            }
        }
    }

    fn handle_field_click(&mut self, field: FieldId, column: u16, row: u16) {
        match field {
            FieldId::Connection => {
                if let Some(index) = self.selector_choice_at(field, column, row) {
                    self.flow.connection = if index == 0 {
                        ConnectionPreference::Host
                    } else {
                        ConnectionPreference::Join
                    };
                    self.push_log(format!(
                        "连接方式已切换为{}。",
                        connection_label(self.flow.connection)
                    ));
                }
            }
            FieldId::Mode => {
                if let Some(index) = self.selector_choice_at(field, column, row) {
                    self.flow.mode = match index {
                        0 => SyncMode::Off,
                        1 => SyncMode::Send,
                        2 => SyncMode::Receive,
                        3 => SyncMode::Both,
                        _ => SyncMode::Auto,
                    };
                    self.push_log(format!("文件同步模式已切换为{}。", self.flow.mode.label()));
                    self.clamp_focus_current_tab();
                }
            }
            FieldId::InitialSync if self.field_is_focusable(field) => {
                if let Some(index) = self.selector_choice_at(field, column, row) {
                    self.flow.initial_sync = match index {
                        0 => None,
                        1 => Some(InitialSyncMode::This),
                        _ => Some(InitialSyncMode::Other),
                    };
                    self.push_log(match self.flow.initial_sync {
                        Some(mode) => format!("初始状态来源已切换为{}。", mode.label()),
                        None => "已清除初始状态来源选择。".to_string(),
                    });
                }
            }
            FieldId::SyncDelete if self.field_is_focusable(field) => {
                self.flow.sync_delete = !self.flow.sync_delete;
                self.push_log(format!(
                    "删除同步已{}。",
                    enabled_label(self.flow.sync_delete)
                ));
            }
            FieldId::ClipboardMode => {
                if let Some(index) = self.selector_choice_at(field, column, row) {
                    self.flow.clipboard_mode = match index {
                        0 => ClipboardMode::Off,
                        1 => ClipboardMode::Send,
                        2 => ClipboardMode::Receive,
                        _ => ClipboardMode::Both,
                    };
                }
                self.push_log(format!(
                    "剪贴板同步已切换为{}。",
                    self.flow.clipboard_mode.label()
                ));
            }
            FieldId::AudioMode => {
                if let Some(index) = self.selector_choice_at(field, column, row) {
                    self.flow.audio_mode = match index {
                        0 => AudioMode::Off,
                        1 => AudioMode::Send,
                        _ => AudioMode::Receive,
                    };
                    self.push_log(format!(
                        "音频同步已切换为{}。",
                        self.flow.audio_mode.label()
                    ));
                }
            }
            FieldId::InstanceName => {}
            FieldId::Accept => {
                self.pairing.accept = !self.pairing.accept;
                self.push_log(format!(
                    "认证通过后的自动接受已{}。",
                    enabled_label(self.pairing.accept)
                ));
            }
            FieldId::TrustDevice => {
                self.pairing.trust_device = !self.pairing.trust_device;
                self.push_log(format!(
                    "可信设备绑定倾向已{}。",
                    enabled_label(self.pairing.trust_device)
                ));
            }
            FieldId::TrustedOnly => {
                self.pairing.trusted_only = !self.pairing.trusted_only;
                self.push_log(format!(
                    "仅可信设备模式已{}。",
                    enabled_label(self.pairing.trusted_only)
                ));
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let colors = palette();
        let area = frame.area();
        let preview = self.preview_model();
        self.ui_state.clear();
        frame.render_widget(
            Block::default().style(Style::default().bg(colors.background)),
            area,
        );

        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            let block = rounded_block(
                " Synly Launchpad ",
                colors.primary,
                colors.panel,
                colors.text,
            );
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::from("终端窗口太小，暂时无法显示完整配置界面。"),
                    Line::from(format!(
                        "至少需要 {}x{}，当前约为 {}x{}。",
                        MIN_WIDTH, MIN_HEIGHT, area.width, area.height
                    )),
                    Line::from("把窗口拉大后，这个界面会自动恢复。"),
                ]))
                .alignment(Alignment::Center)
                .style(Style::default().fg(colors.text)),
                inner,
            );
            return;
        }

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(18),
                Constraint::Length(2),
            ])
            .split(area);

        self.render_header(frame, outer[0], colors, &preview);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(outer[1]);

        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(2),
                Constraint::Min(12),
            ])
            .split(body[0]);

        self.render_tabs(frame, left[0], colors);
        self.render_tab_description(frame, left[1], colors);
        self.render_form(frame, left[2], colors);

        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(10),
                Constraint::Length(6),
                Constraint::Min(8),
            ])
            .split(body[1]);

        self.render_preview(frame, right[0], colors, &preview);
        self.render_command(frame, right[1], colors, &preview);
        self.render_output(frame, right[2], colors, &preview);
        self.render_footer(frame, outer[2], colors);
    }

    fn render_header(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        colors: Palette,
        preview: &PreviewModel,
    ) {
        let accent = if preview.errors.is_empty() {
            colors.success
        } else {
            colors.warning
        };
        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(18),
            ])
            .split(area);

        let block = rounded_block(
            " Synly Launchpad ",
            colors.primary,
            colors.panel,
            colors.text,
        );
        let inner = block.inner(sections[0]);
        frame.render_widget(block, sections[0]);

        let readiness = if preview.errors.is_empty() {
            ("READY", colors.success)
        } else {
            ("EDITING", colors.warning)
        };

        let top = Line::from(vec![
            chip("Session Builder", colors.primary, colors.primary_soft),
            Span::raw("  "),
            Span::styled(
                self.context.device_label.clone(),
                Style::default()
                    .fg(colors.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            chip(readiness.0, readiness.1, colors.panel),
        ]);
        let bottom = Line::from(vec![
            chip(
                connection_label(self.flow.connection),
                colors.text,
                colors.primary_soft,
            ),
            Span::raw(" "),
            chip(self.flow.mode.label(), colors.text, colors.primary_soft),
            Span::raw(" "),
            chip(
                self.flow.clipboard_mode.label(),
                colors.text,
                colors.primary_soft,
            ),
            Span::raw(" "),
            chip(
                self.flow.audio_mode.label(),
                colors.text,
                colors.primary_soft,
            ),
        ]);

        frame.render_widget(
            Paragraph::new(Text::from(vec![top, bottom]))
                .style(Style::default().fg(colors.text).bg(colors.panel)),
            inner,
        );

        self.ui_state.launch_button_area = Some(sections[2]);
        frame.render_widget(
            Paragraph::new(Text::from(vec![Line::from("启动"), Line::from("Ctrl+S")]))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(accent)
                        .bg(colors.panel)
                        .add_modifier(Modifier::BOLD),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .style(Style::default().bg(colors.panel))
                        .border_style(Style::default().fg(accent)),
                ),
            sections[2],
        );
    }

    fn render_tabs(&mut self, frame: &mut Frame, area: Rect, colors: Palette) {
        let titles = StartupTab::ALL
            .iter()
            .map(|tab| tab_title_line(*tab))
            .collect::<Vec<_>>();

        let block = rounded_block(" 分区 ", colors.border, colors.panel, colors.tag_text);
        self.ui_state.tab_areas = tab_click_areas(block.inner(area));

        let tabs = Tabs::new(titles)
            .select(self.tab.index())
            .block(block)
            .style(Style::default().fg(colors.tag_text))
            .highlight_style(
                Style::default()
                    .fg(colors.primary)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, area);
    }

    fn render_tab_description(&self, frame: &mut Frame, area: Rect, colors: Palette) {
        frame.render_widget(
            Paragraph::new(self.tab.description())
                .style(Style::default().fg(colors.muted))
                .block(
                    Block::default()
                        .border_type(BorderType::Rounded)
                        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                        .style(Style::default().bg(colors.panel))
                        .border_style(Style::default().fg(colors.border)),
                ),
            area,
        );
    }

    fn render_form(&mut self, frame: &mut Frame, area: Rect, colors: Palette) {
        let block = rounded_block(" 会话参数 ", colors.border, colors.panel, colors.tag_text);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_fields = self.visible_fields();
        let mut constraints = vec![Constraint::Length(3); visible_fields.len()];
        constraints.push(Constraint::Min(0));
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);
        let current = self.current_field();
        self.ui_state.field_areas = visible_fields
            .iter()
            .enumerate()
            .map(|(index, field)| (*field, areas[index]))
            .collect();

        for (index, field) in visible_fields.iter().enumerate() {
            self.render_field(frame, areas[index], *field, *field == current, colors);
        }
    }

    fn render_field(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        field: FieldId,
        focused: bool,
        colors: Palette,
    ) {
        let editing = focused && self.is_editing_input();
        match field {
            FieldId::Connection => self.render_selector_field(
                frame,
                area,
                field,
                "连接方式",
                &["等待连接", "主动连接"],
                match self.flow.connection {
                    ConnectionPreference::Host => 0,
                    ConnectionPreference::Join => 1,
                },
                true,
                focused,
                colors,
            ),
            FieldId::Mode => self.render_selector_field(
                frame,
                area,
                field,
                "文件同步模式",
                &["关闭", "发送", "接收", "双向", "自动"],
                match self.flow.mode {
                    SyncMode::Off => 0,
                    SyncMode::Send => 1,
                    SyncMode::Receive => 2,
                    SyncMode::Both => 3,
                    SyncMode::Auto => 4,
                },
                true,
                focused,
                colors,
            ),
            FieldId::InitialSync => self.render_selector_field(
                frame,
                area,
                field,
                "初始状态来源",
                &["未选", "本机目录", "对端目录"],
                match self.flow.initial_sync {
                    None => 0,
                    Some(InitialSyncMode::This) => 1,
                    Some(InitialSyncMode::Other) => 2,
                },
                self.field_is_focusable(field),
                focused,
                colors,
            ),
            FieldId::SyncDelete => self.render_toggle_field(
                frame,
                area,
                "删除同步",
                self.flow.sync_delete,
                self.flow.mode.can_receive(),
                "接收方会镜像对端删除结果",
                focused,
                colors,
            ),
            FieldId::ClipboardMode => self.render_selector_field(
                frame,
                area,
                field,
                "剪贴板同步",
                &["关闭", "发送", "接收", "双向"],
                match self.flow.clipboard_mode {
                    ClipboardMode::Off => 0,
                    ClipboardMode::Send => 1,
                    ClipboardMode::Receive => 2,
                    ClipboardMode::Both => 3,
                },
                true,
                focused,
                colors,
            ),
            FieldId::AudioMode => self.render_selector_field(
                frame,
                area,
                field,
                "音频同步",
                &["关闭", "发送", "接收"],
                match self.flow.audio_mode {
                    AudioMode::Off => 0,
                    AudioMode::Send => 1,
                    AudioMode::Receive => 2,
                },
                true,
                focused,
                colors,
            ),
            FieldId::WorkspacePath => {
                let title = match self.flow.mode {
                    SyncMode::Off => "文件同步已关闭",
                    SyncMode::Send => "发送路径",
                    SyncMode::Receive => "接收目录",
                    SyncMode::Both => "双向目录",
                    SyncMode::Auto => "共享目录",
                };
                let placeholder = match self.flow.mode {
                    SyncMode::Off => "当前模式不会使用这个分区",
                    SyncMode::Send => "多个路径用英文逗号分隔；输入 . 使用当前目录",
                    _ => "请输入目录；输入 . 使用当前目录，支持 ~ 和环境变量",
                };
                apply_textarea_theme(
                    &mut self.workspace.path,
                    title,
                    placeholder,
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.workspace.path, area);
            }
            FieldId::MaxFolderDepth => {
                apply_textarea_theme(
                    &mut self.workspace.max_folder_depth,
                    "最大目录深度",
                    "留空表示不限制，0 表示只发顶层内容",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.workspace.max_folder_depth, area);
            }
            FieldId::IntervalSecs => {
                apply_textarea_theme(
                    &mut self.workspace.interval_secs,
                    "兜底重扫间隔（秒）",
                    "留空恢复默认 3 秒",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.workspace.interval_secs, area);
            }
            FieldId::InstanceName => {
                apply_textarea_theme(
                    &mut self.pairing.instance_name,
                    "当前实例名",
                    "留空表示沿用设备名，仅作用于本次运行",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.pairing.instance_name, area);
            }
            FieldId::PeerQuery => {
                apply_textarea_theme(
                    &mut self.pairing.peer_query,
                    "目标设备",
                    "可填实例名、设备名、设备 ID 前缀或 IPv4；留空则启动后选择",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.pairing.peer_query, area);
            }
            FieldId::Port => {
                apply_textarea_theme(
                    &mut self.pairing.port,
                    "固定监听端口",
                    "Host 模式生效；留空随机分配",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.pairing.port, area);
            }
            FieldId::Pin => {
                apply_textarea_theme(
                    &mut self.pairing.pin,
                    "固定 PIN",
                    "6 位数字；留空则运行时再输入或显示",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.pairing.pin, area);
            }
            FieldId::Accept => self.render_toggle_field(
                frame,
                area,
                "认证后自动接受",
                self.pairing.accept,
                true,
                "未受信任设备也不再二次确认",
                focused,
                colors,
            ),
            FieldId::TrustDevice => self.render_toggle_field(
                frame,
                area,
                "倾向建立信任",
                self.pairing.trust_device,
                true,
                "成功配对后优先保存长期身份绑定",
                focused,
                colors,
            ),
            FieldId::TrustedOnly => self.render_toggle_field(
                frame,
                area,
                "仅可信设备",
                self.pairing.trusted_only,
                true,
                "未建立信任时直接拒绝，不回退到 PIN",
                focused,
                colors,
            ),
            FieldId::DiscoverySecs => {
                apply_textarea_theme(
                    &mut self.pairing.discovery_secs,
                    "设备发现等待（秒）",
                    "留空恢复默认 3 秒",
                    focused,
                    editing,
                    colors,
                );
                frame.render_widget(&self.pairing.discovery_secs, area);
            }
        }
    }

    fn render_selector_field(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        field: FieldId,
        title: &str,
        labels: &[&str],
        selected: usize,
        interactive: bool,
        focused: bool,
        colors: Palette,
    ) {
        let border = if focused && interactive {
            colors.primary
        } else if interactive {
            colors.border
        } else {
            colors.muted
        };
        let spans = labels
            .iter()
            .enumerate()
            .flat_map(|(index, label)| {
                let selected_style = if !interactive {
                    Style::default()
                        .fg(colors.tag_text)
                        .bg(colors.primary_soft)
                        .add_modifier(Modifier::BOLD)
                } else if index == selected {
                    Style::default()
                        .fg(colors.background)
                        .bg(colors.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(colors.text).bg(colors.primary_soft)
                };
                [selector_choice_span(label, selected_style), Span::raw(" ")]
            })
            .collect::<Vec<_>>();
        let block = rounded_block(
            title,
            border,
            colors.panel,
            if focused && interactive {
                colors.text
            } else {
                colors.tag_text
            },
        );
        if interactive {
            self.ui_state.selector_areas.extend(
                selector_choice_areas(block.inner(area), labels)
                    .into_iter()
                    .enumerate()
                    .map(|(index, rect)| (field, index, rect)),
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans))
                .style(Style::default().bg(colors.panel))
                .block(block),
            area,
        );
    }

    fn render_toggle_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        enabled: bool,
        interactive: bool,
        note: &str,
        focused: bool,
        colors: Palette,
    ) {
        let border = if focused && interactive {
            colors.primary
        } else if interactive {
            colors.border
        } else {
            colors.muted
        };
        let pill_style = if interactive {
            if enabled {
                Style::default()
                    .fg(colors.background)
                    .bg(colors.success)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(colors.text)
                    .bg(colors.primary_soft)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default()
                .fg(colors.tag_text)
                .bg(colors.primary_soft)
                .add_modifier(Modifier::BOLD)
        };
        let label = if interactive {
            if enabled { "开启" } else { "关闭" }
        } else if enabled {
            "固定开启"
        } else {
            "不适用"
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {} ", label), pill_style),
                Span::raw("  "),
                Span::styled(note.to_string(), Style::default().fg(colors.muted)),
            ]))
            .style(Style::default().bg(colors.panel))
            .block(rounded_block(
                title,
                border,
                colors.panel,
                if focused {
                    colors.text
                } else {
                    colors.tag_text
                },
            )),
            area,
        );
    }

    fn render_preview(
        &self,
        frame: &mut Frame,
        area: Rect,
        colors: Palette,
        preview: &PreviewModel,
    ) {
        let title = if preview.errors.is_empty() {
            " 启动预览 "
        } else {
            " 启动预览 · 需要修正 "
        };
        let block = rounded_block(
            title,
            if preview.errors.is_empty() {
                colors.success
            } else {
                colors.warning
            },
            colors.panel,
            colors.text,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = preview
            .summary_lines
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().fg(colors.text))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }

    fn render_command(
        &self,
        frame: &mut Frame,
        area: Rect,
        colors: Palette,
        preview: &PreviewModel,
    ) {
        let title = if preview.command_is_valid {
            " 等效命令行 "
        } else {
            " 等效命令行 · 当前输入待修正 "
        };
        let block = rounded_block(
            title,
            if preview.command_is_valid {
                colors.primary
            } else {
                colors.warning
            },
            colors.panel,
            colors.text,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    preview.command_line.clone(),
                    Style::default()
                        .fg(colors.text)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    if preview.command_is_valid {
                        "这条命令会直接复现当前设置。"
                    } else {
                        "当前仍有校验问题，这里先展示基于现有输入拼出的命令。"
                    },
                    Style::default().fg(colors.muted),
                )),
            ]))
            .style(Style::default().fg(colors.text))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_output(
        &self,
        frame: &mut Frame,
        area: Rect,
        colors: Palette,
        preview: &PreviewModel,
    ) {
        let block = rounded_block(" 状态输出 ", colors.border, colors.panel, colors.tag_text);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut items = self
            .logs
            .iter()
            .rev()
            .take(inner.height.saturating_sub(1) as usize)
            .cloned()
            .collect::<Vec<_>>();
        items.reverse();

        let mut list_items = items
            .into_iter()
            .map(|line| {
                ListItem::new(Line::from(vec![
                    Span::styled("• ", Style::default().fg(colors.primary)),
                    Span::styled(line, Style::default().fg(colors.text)),
                ]))
            })
            .collect::<Vec<_>>();

        for error in preview.errors.iter().take(2) {
            list_items.push(ListItem::new(Line::from(vec![
                Span::styled("! ", Style::default().fg(colors.danger)),
                Span::styled(error.clone(), Style::default().fg(colors.danger)),
            ])));
        }
        for note in preview.notes.iter().take(2) {
            list_items.push(ListItem::new(Line::from(vec![
                Span::styled("· ", Style::default().fg(colors.warning)),
                Span::styled(note.clone(), Style::default().fg(colors.muted)),
            ])));
        }

        frame.render_widget(List::new(list_items), inner);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, colors: Palette) {
        let line = Line::from(vec![
            chip("Tab / Shift+Tab", colors.primary, colors.panel),
            Span::raw(" 切换字段  "),
            chip("[ ] / F1 F2 F3", colors.primary, colors.panel),
            Span::raw(" 切换分区  "),
            chip("hjkl / arrows", colors.primary, colors.panel),
            Span::raw(" 浏览  "),
            chip("Enter / Click", colors.primary, colors.panel),
            Span::raw(" 编辑输入框  "),
            chip("Ctrl+S", colors.success, colors.panel),
            Span::raw(" 启动  "),
            chip("Esc", colors.primary, colors.panel),
            Span::raw(" 退出输入  "),
            chip("q", colors.danger, colors.panel),
            Span::raw(" 退出应用"),
        ]);
        frame.render_widget(
            Paragraph::new(line)
                .alignment(Alignment::Center)
                .style(Style::default().fg(colors.muted)),
            area,
        );
    }

    fn preview_model(&self) -> PreviewModel {
        let mut summary_lines = vec![
            format!("连接方式: {}", connection_label(self.flow.connection)),
            format!("文件同步模式: {}", self.flow.mode.label()),
            format!("剪贴板同步: {}", self.flow.clipboard_mode.label()),
            format!("音频同步: {}", self.flow.audio_mode.label()),
        ];
        let mut notes = Vec::new();
        let mut errors = Vec::new();

        match self.parsed_initial_sync() {
            Ok(Some(initial_sync)) => {
                summary_lines.push(format!("初始状态: {}", initial_sync.label()));
            }
            Ok(None) => {
                if matches!(self.flow.mode, SyncMode::Both | SyncMode::Auto) {
                    errors.push(
                        "双向/自动文件同步需要选择初始状态来源：本机目录或对端目录。".to_string(),
                    );
                } else {
                    notes.push("当前文件同步模式不会使用初始状态来源。".to_string());
                }
            }
            Err(err) => errors.push(err.to_string()),
        }

        match self.workspace_preview() {
            Ok(workspace) => {
                summary_lines.extend(workspace.lines);
                notes.extend(workspace.notes);
                summary_lines.push(if workspace.can_receive {
                    format!("删除同步: {}", bool_label(self.flow.sync_delete))
                } else {
                    "删除同步: 不适用".to_string()
                });
            }
            Err(err) => {
                errors.push(err.to_string());
                summary_lines.push("删除同步: 等待工作区配置通过校验".to_string());
            }
        }

        match self.parsed_interval_secs() {
            Ok(value) => {
                if !self.flow.mode.can_send() {
                    notes.push("当前文件同步模式不会使用文件重扫间隔。".to_string());
                } else {
                    summary_lines.push(format!("兜底重扫: {} 秒", value));
                }
            }
            Err(err) => errors.push(err.to_string()),
        }

        match self.parsed_discovery_secs() {
            Ok(value) => {
                if self.flow.connection == ConnectionPreference::Join {
                    summary_lines.push(format!("发现等待: {} 秒", value));
                } else {
                    notes.push("Host 模式不会用到设备发现等待时间。".to_string());
                }
            }
            Err(err) => errors.push(err.to_string()),
        }

        match self.parsed_port() {
            Ok(Some(port)) => {
                if self.flow.connection == ConnectionPreference::Host {
                    summary_lines.push(format!("监听端口: 固定为 {}", port));
                } else {
                    notes.push(format!(
                        "固定监听端口已设为 {}，但 Join 模式不会监听端口。",
                        port
                    ));
                }
            }
            Ok(None) => {
                if self.flow.connection == ConnectionPreference::Host {
                    summary_lines.push("监听端口: 自动分配".to_string());
                }
            }
            Err(err) => errors.push(err.to_string()),
        }

        match self.parsed_instance_name() {
            Some(instance_name) => summary_lines.push(format!("当前实例名: {}", instance_name)),
            None => summary_lines.push(format!(
                "当前实例名: 沿用设备名 {}",
                self.context.device_label
            )),
        }

        if self.flow.connection == ConnectionPreference::Join {
            summary_lines.push(format!(
                "目标设备: {}",
                target_label(trimmed_text(&self.pairing.peer_query).as_str())
            ));
        } else {
            summary_lines.push("目标设备: Host 模式下不需要预设目标".to_string());
        }

        match self.parsed_pin() {
            Ok(Some(pin)) => summary_lines.push(format!("固定 PIN: {}", pin)),
            Ok(None) => summary_lines.push("固定 PIN: 留空，运行时再输入或显示".to_string()),
            Err(err) => errors.push(err.to_string()),
        }

        summary_lines.push(format!("自动接受: {}", bool_label(self.pairing.accept)));
        summary_lines.push(format!(
            "建立信任: {}",
            bool_label(self.pairing.trust_device)
        ));
        summary_lines.push(format!(
            "仅可信设备: {}",
            bool_label(self.pairing.trusted_only)
        ));

        let command_line = self.draft_command_line();
        let command_is_valid = errors.is_empty();

        PreviewModel {
            summary_lines,
            notes,
            errors,
            command_line,
            command_is_valid,
        }
    }

    fn draft_command_line(&self) -> String {
        let mut args = vec!["synly".to_string()];
        if let Some(instance_name) = trimmed_non_empty(&self.pairing.instance_name) {
            push_flag_value(&mut args, "--name", instance_name);
        }

        push_flag_value(&mut args, "--fs", sync_mode_arg(self.flow.mode).to_string());
        if matches!(self.flow.mode, SyncMode::Both | SyncMode::Auto)
            && let Some(initial_sync) = self.flow.initial_sync
        {
            push_flag_value(&mut args, "--initial", initial_sync.as_arg().to_string());
        }
        append_draft_paths(
            &mut args,
            self.flow.mode,
            trimmed_text(&self.workspace.path),
        );
        args.push(connection_flag(self.flow.connection).to_string());

        if self.flow.mode.can_receive() {
            args.push(if self.flow.sync_delete {
                "--sync-delete".to_string()
            } else {
                "--no-sync-delete".to_string()
            });
        }

        push_flag_value(
            &mut args,
            "--clipboard",
            clipboard_mode_arg(self.flow.clipboard_mode).to_string(),
        );
        push_flag_value(
            &mut args,
            "--audio",
            audio_mode_arg(self.flow.audio_mode).to_string(),
        );

        if let Some(depth) = trimmed_non_empty(&self.workspace.max_folder_depth) {
            push_flag_value(&mut args, "--max-folder-depth", depth);
        }
        if let Some(interval_secs) = trimmed_non_empty(&self.workspace.interval_secs) {
            push_flag_value(&mut args, "--interval-secs", interval_secs);
        }

        if self.flow.connection == ConnectionPreference::Join {
            if let Some(peer_query) = trimmed_non_empty(&self.pairing.peer_query) {
                push_flag_value(&mut args, "--peer", peer_query);
            }
            if let Some(discovery_secs) = trimmed_non_empty(&self.pairing.discovery_secs) {
                push_flag_value(&mut args, "--discovery-secs", discovery_secs);
            }
        } else if let Some(port) = trimmed_non_empty(&self.pairing.port) {
            push_flag_value(&mut args, "--port", port);
        }

        if let Some(pin) = trimmed_non_empty(&self.pairing.pin) {
            push_flag_value(&mut args, "--pin", pin);
        }
        if self.pairing.accept {
            args.push("--accept".to_string());
        }
        if self.pairing.trust_device {
            args.push("--trust-device".to_string());
        }
        if self.pairing.trusted_only {
            args.push("--trusted-only".to_string());
        }

        shell_join(&args)
    }

    fn workspace_preview(&self) -> Result<WorkspacePreview> {
        if self.flow.mode == SyncMode::Off {
            return Ok(WorkspacePreview {
                lines: vec!["文件同步: 关闭".to_string()],
                can_receive: false,
                notes: Vec::new(),
            });
        }

        let max_folder_depth = self.parsed_max_folder_depth()?;

        match self.flow.mode {
            SyncMode::Off => unreachable!("off mode is handled before workspace preview"),
            SyncMode::Send => {
                let paths = parse_send_paths(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                let mut lines = if paths.len() == 1 && paths[0].is_dir() {
                    vec![format!("发送目录: {}", paths[0].display())]
                } else {
                    paths
                        .iter()
                        .map(|path| format!("发送条目: {}", path.display()))
                        .collect::<Vec<_>>()
                };
                if let Some(depth) = max_folder_depth {
                    lines.push(format!("发送最大目录深度: {}", depth));
                }
                Ok(WorkspacePreview {
                    lines,
                    can_receive: false,
                    notes: Vec::new(),
                })
            }
            SyncMode::Receive => {
                let (path, will_create) = preview_directory_path(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                let mut notes = Vec::new();
                if will_create {
                    notes.push(format!(
                        "接收目录不存在，启动时会自动创建: {}",
                        path.display()
                    ));
                }
                if max_folder_depth.is_some() {
                    notes.push("当前模式不会使用“最大目录深度”，因为本机不发送文件。".to_string());
                }
                Ok(WorkspacePreview {
                    lines: vec![format!("接收目录: {}", path.display())],
                    can_receive: true,
                    notes,
                })
            }
            SyncMode::Both | SyncMode::Auto => {
                let (path, will_create) = preview_directory_path(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                let mut lines = vec![
                    format!("发送目录: {}", path.display()),
                    format!("接收目录: {}", path.display()),
                ];
                if let Some(depth) = max_folder_depth {
                    lines.push(format!("发送最大目录深度: {}", depth));
                }
                let mut notes = Vec::new();
                match self.parsed_initial_sync()? {
                    Some(initial_sync) => {
                        notes.push(format!("初始状态来源: {}", initial_sync.label()))
                    }
                    None => {
                        notes.push("启动前还需要选择初始状态来源：本机目录或对端目录。".to_string())
                    }
                }
                if will_create {
                    notes.push(format!(
                        "共享目录不存在，启动时会自动创建: {}",
                        path.display()
                    ));
                }
                Ok(WorkspacePreview {
                    lines,
                    can_receive: true,
                    notes,
                })
            }
        }
    }

    fn build_runtime_options(&self) -> Result<RuntimeOptions> {
        let max_folder_depth = self.parsed_max_folder_depth()?;
        let interval_secs = self.parsed_interval_secs()?;
        let discovery_secs = self.parsed_discovery_secs()?;
        let port = self.parsed_port()?;
        let pin = self.parsed_pin()?;
        let initial_sync = self.parsed_initial_sync()?;
        let workspace = match self.flow.mode {
            SyncMode::Off => WorkspaceSpec::for_off(),
            SyncMode::Send => WorkspaceSpec::for_send(parse_send_paths(
                trimmed_text(&self.workspace.path).as_str(),
                &self.context.cwd,
            )?)?,
            SyncMode::Receive => {
                let path = build_directory_path(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                WorkspaceSpec::for_receive(path)?
            }
            SyncMode::Both => {
                let path = build_directory_path(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                WorkspaceSpec::for_both(path)?.with_initial_sync(Some(
                    initial_sync.context("双向文件同步必须选择初始状态来源")?,
                ))
            }
            SyncMode::Auto => {
                let path = build_directory_path(
                    trimmed_text(&self.workspace.path).as_str(),
                    &self.context.cwd,
                )?;
                WorkspaceSpec::for_auto(path)?.with_initial_sync(Some(
                    initial_sync.context("自动文件同步必须选择初始状态来源")?,
                ))
            }
        }
        .with_max_folder_depth(max_folder_depth);

        Ok(RuntimeOptions {
            mode: self.flow.mode,
            connection: self.flow.connection,
            instance_name: self.parsed_instance_name(),
            sync_delete: if workspace.incoming_root.is_some() {
                self.flow.sync_delete
            } else {
                false
            },
            clipboard_mode: self.flow.clipboard_mode,
            audio_mode: self.flow.audio_mode,
            workspace,
            clipboard: self.context.clipboard.clone(),
            transfer_limits: self.context.transfer_limits,
            interval_secs,
            pairing: PairingRuntimeOptions {
                no_interact: false,
                peer_query: trimmed_non_empty(&self.pairing.peer_query),
                port,
                pin,
                accept: self.pairing.accept,
                trust_device: self.pairing.trust_device,
                trusted_only: self.pairing.trusted_only,
                discovery_secs,
            },
        })
    }

    fn parsed_initial_sync(&self) -> Result<Option<InitialSyncMode>> {
        if matches!(self.flow.mode, SyncMode::Both | SyncMode::Auto) {
            Ok(self.flow.initial_sync)
        } else {
            Ok(None)
        }
    }

    fn parsed_pin(&self) -> Result<Option<String>> {
        let raw = trimmed_text(&self.pairing.pin);
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(normalize_pin(raw.as_str())?))
    }

    fn parsed_instance_name(&self) -> Option<String> {
        trimmed_non_empty(&self.pairing.instance_name)
    }

    fn parsed_port(&self) -> Result<Option<u16>> {
        let raw = trimmed_text(&self.pairing.port);
        if raw.is_empty() {
            return Ok(None);
        }

        let port = raw.parse::<u16>().with_context(|| {
            format!("固定监听端口必须是 1 到 65535 之间的整数，当前输入为 `{raw}`")
        })?;
        if port == 0 {
            bail!("固定监听端口必须是 1 到 65535 之间的整数，当前输入为 `{raw}`");
        }
        Ok(Some(port))
    }

    fn parsed_interval_secs(&self) -> Result<u64> {
        parse_u64_field(
            "兜底重扫间隔",
            trimmed_text(&self.workspace.interval_secs).as_str(),
            DEFAULT_INTERVAL_SECS,
        )
    }

    fn parsed_discovery_secs(&self) -> Result<u64> {
        parse_u64_field(
            "设备发现等待时间",
            trimmed_text(&self.pairing.discovery_secs).as_str(),
            DEFAULT_DISCOVERY_SECS,
        )
    }

    fn parsed_max_folder_depth(&self) -> Result<Option<usize>> {
        let raw = trimmed_text(&self.workspace.max_folder_depth);
        if raw.is_empty() {
            return Ok(None);
        }
        raw.parse::<usize>()
            .map(Some)
            .with_context(|| format!("最大目录深度必须是非负整数，当前输入为 `{raw}`"))
    }

    fn visible_fields(&self) -> &'static [FieldId] {
        match self.tab {
            StartupTab::Flow => &[
                FieldId::Connection,
                FieldId::Mode,
                FieldId::InitialSync,
                FieldId::SyncDelete,
                FieldId::ClipboardMode,
                FieldId::AudioMode,
            ],
            StartupTab::Workspace => &[
                FieldId::WorkspacePath,
                FieldId::MaxFolderDepth,
                FieldId::IntervalSecs,
            ],
            StartupTab::Pairing => &[
                FieldId::InstanceName,
                FieldId::PeerQuery,
                FieldId::Port,
                FieldId::Pin,
                FieldId::Accept,
                FieldId::TrustDevice,
                FieldId::TrustedOnly,
                FieldId::DiscoverySecs,
            ],
        }
    }

    fn focusable_fields(&self) -> Vec<FieldId> {
        self.visible_fields()
            .iter()
            .copied()
            .filter(|field| self.field_is_focusable(*field))
            .collect()
    }

    fn field_is_text(&self, field: FieldId) -> bool {
        matches!(
            field,
            FieldId::WorkspacePath
                | FieldId::MaxFolderDepth
                | FieldId::IntervalSecs
                | FieldId::InstanceName
                | FieldId::PeerQuery
                | FieldId::Port
                | FieldId::Pin
                | FieldId::DiscoverySecs
        )
    }

    fn field_is_focusable(&self, field: FieldId) -> bool {
        match field {
            FieldId::InitialSync => matches!(self.flow.mode, SyncMode::Both | SyncMode::Auto),
            FieldId::SyncDelete => self.flow.mode.can_receive(),
            _ => true,
        }
    }

    fn current_field_accepts_text(&self) -> bool {
        self.field_is_text(self.current_field())
    }

    fn is_editing_input(&self) -> bool {
        self.editing_input && self.current_field_accepts_text()
    }

    fn current_field(&self) -> FieldId {
        let focusable = self.focusable_fields();
        let index = self.focus_by_tab[self.tab.index()].min(focusable.len().saturating_sub(1));
        focusable[index]
    }

    fn move_focus(&mut self, delta: isize) {
        let focusable = self.focusable_fields();
        let len = focusable.len();
        if len == 0 {
            return;
        }
        self.editing_input = false;
        let slot = &mut self.focus_by_tab[self.tab.index()];
        if delta >= 0 {
            *slot = (*slot + delta as usize) % len;
        } else {
            let step = delta.unsigned_abs();
            *slot = (*slot + len - (step % len)) % len;
        }
    }

    fn clamp_focus_current_tab(&mut self) {
        let len = self.focusable_fields().len();
        if len == 0 {
            self.focus_by_tab[self.tab.index()] = 0;
            return;
        }
        self.focus_by_tab[self.tab.index()] =
            self.focus_by_tab[self.tab.index()].min(len.saturating_sub(1));
    }

    fn set_tab(&mut self, tab: StartupTab) {
        self.tab = tab;
        self.editing_input = false;
        self.clamp_focus_current_tab();
        self.push_log(format!("已切换到 {} 分区。", tab.title()));
    }

    fn switch_tab(&mut self, delta: isize) {
        let current = self.tab.index() as isize;
        let next = (current + delta).rem_euclid(StartupTab::ALL.len() as isize) as usize;
        self.set_tab(StartupTab::ALL[next]);
    }

    fn push_log(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.logs.last() == Some(&message) {
            return;
        }
        self.logs.push(message);
        if self.logs.len() > LOG_LIMIT {
            let overflow = self.logs.len() - LOG_LIMIT;
            self.logs.drain(0..overflow);
        }
    }

    fn active_textarea_mut(&mut self) -> Option<&mut TextArea<'static>> {
        match self.current_field() {
            FieldId::WorkspacePath => Some(&mut self.workspace.path),
            FieldId::MaxFolderDepth => Some(&mut self.workspace.max_folder_depth),
            FieldId::IntervalSecs => Some(&mut self.workspace.interval_secs),
            FieldId::InstanceName => Some(&mut self.pairing.instance_name),
            FieldId::PeerQuery => Some(&mut self.pairing.peer_query),
            FieldId::Port => Some(&mut self.pairing.port),
            FieldId::Pin => Some(&mut self.pairing.pin),
            FieldId::DiscoverySecs => Some(&mut self.pairing.discovery_secs),
            _ => None,
        }
    }

    fn select_field(&mut self, field: FieldId) {
        if let Some(index) = self
            .focusable_fields()
            .iter()
            .position(|candidate| *candidate == field)
        {
            self.focus_by_tab[self.tab.index()] = index;
        }
    }

    fn tab_at(&self, column: u16, row: u16) -> Option<StartupTab> {
        self.ui_state
            .tab_areas
            .iter()
            .find(|(_, rect)| rect_contains(*rect, column, row))
            .map(|(tab, _)| *tab)
    }

    fn field_at(&self, column: u16, row: u16) -> Option<FieldId> {
        self.ui_state
            .field_areas
            .iter()
            .find(|(_, rect)| rect_contains(*rect, column, row))
            .map(|(field, _)| *field)
    }

    fn selector_choice_at(&self, field: FieldId, column: u16, row: u16) -> Option<usize> {
        self.ui_state
            .selector_areas
            .iter()
            .find(|(candidate, _, rect)| *candidate == field && rect_contains(*rect, column, row))
            .map(|(_, index, _)| *index)
    }
}

fn single_line_textarea(value: String, placeholder: &str) -> TextArea<'static> {
    let mut textarea = TextArea::new(vec![value]);
    textarea.set_cursor_line_style(Style::default());
    textarea.set_placeholder_text(placeholder);
    textarea.set_max_histories(128);
    textarea
}

fn apply_textarea_theme(
    textarea: &mut TextArea<'static>,
    title: &str,
    placeholder: &str,
    selected: bool,
    editing: bool,
    colors: Palette,
) {
    textarea.set_placeholder_text(placeholder);
    textarea.set_style(Style::default().fg(colors.text).bg(colors.panel));
    textarea.set_cursor_line_style(if editing {
        Style::default().bg(colors.primary_soft)
    } else {
        Style::default()
    });
    textarea.set_cursor_style(if editing {
        Style::default().fg(colors.background).bg(colors.primary)
    } else {
        Style::default().fg(colors.text).bg(colors.panel)
    });
    textarea.set_placeholder_style(Style::default().fg(colors.muted));
    textarea.set_block(rounded_block(
        title,
        if selected {
            colors.primary
        } else {
            colors.border
        },
        colors.panel,
        if selected {
            colors.text
        } else {
            colors.tag_text
        },
    ));
}

fn cycle_mode(mode: SyncMode, reverse: bool) -> SyncMode {
    let modes = [
        SyncMode::Off,
        SyncMode::Send,
        SyncMode::Receive,
        SyncMode::Both,
        SyncMode::Auto,
    ];
    let current = modes
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(4);
    let next = if reverse {
        (current + modes.len() - 1) % modes.len()
    } else {
        (current + 1) % modes.len()
    };
    modes[next]
}

fn cycle_initial_sync(
    initial_sync: Option<InitialSyncMode>,
    reverse: bool,
) -> Option<InitialSyncMode> {
    let modes = [
        None,
        Some(InitialSyncMode::This),
        Some(InitialSyncMode::Other),
    ];
    let current = modes
        .iter()
        .position(|candidate| *candidate == initial_sync)
        .unwrap_or(0);
    let next = if reverse {
        (current + modes.len() - 1) % modes.len()
    } else {
        (current + 1) % modes.len()
    };
    modes[next]
}

fn cycle_clipboard_mode(mode: ClipboardMode, reverse: bool) -> ClipboardMode {
    let modes = [
        ClipboardMode::Off,
        ClipboardMode::Send,
        ClipboardMode::Receive,
        ClipboardMode::Both,
    ];
    let current = modes
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0);
    let next = if reverse {
        (current + modes.len() - 1) % modes.len()
    } else {
        (current + 1) % modes.len()
    };
    modes[next]
}

fn cycle_audio_mode(mode: AudioMode, reverse: bool) -> AudioMode {
    let modes = [AudioMode::Off, AudioMode::Send, AudioMode::Receive];
    let current = modes
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0);
    let next = if reverse {
        (current + modes.len() - 1) % modes.len()
    } else {
        (current + 1) % modes.len()
    };
    modes[next]
}

fn connection_label(connection: ConnectionPreference) -> &'static str {
    match connection {
        ConnectionPreference::Host => "等待连接",
        ConnectionPreference::Join => "主动连接",
    }
}

fn enabled_label(value: bool) -> &'static str {
    if value { "开启" } else { "关闭" }
}

fn bool_label(value: bool) -> &'static str {
    if value { "开启" } else { "关闭" }
}

fn target_label(value: &str) -> String {
    if value.trim().is_empty() {
        "留空，启动后搜索并选择设备".to_string()
    } else {
        value.trim().to_string()
    }
}

fn chip(label: &str, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", label),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

fn tab_title_line(tab: StartupTab) -> Line<'static> {
    Line::from(format!(" {} ", tab.title()))
}

fn selector_choice_span(label: &str, style: Style) -> Span<'static> {
    Span::styled(format!(" {} ", label), style)
}

fn rounded_block(
    title: impl Into<String>,
    border_color: Color,
    background: Color,
    title_color: Color,
) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(background))
        .border_style(Style::default().fg(border_color))
        .title_style(
            Style::default()
                .fg(title_color)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        )
        .title(title.into())
}

fn tab_click_areas(area: Rect) -> Vec<(StartupTab, Rect)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut x = area.x;
    let right = area.x.saturating_add(area.width);
    let padding_left_width = Line::from(" ").width() as u16;
    let padding_right_width = Line::from(" ").width() as u16;
    let divider_width = Span::raw(ratatui::symbols::line::VERTICAL).width() as u16;
    let mut rects = Vec::with_capacity(StartupTab::ALL.len());

    for (index, tab) in StartupTab::ALL.iter().enumerate() {
        if x >= right {
            break;
        }

        let start = x;
        x = x.saturating_add(remaining_inline_width(x, right).min(padding_left_width));
        let title_width = tab_title_line(*tab).width() as u16;
        x = x.saturating_add(remaining_inline_width(x, right).min(title_width));
        x = x.saturating_add(remaining_inline_width(x, right).min(padding_right_width));
        let width = x.saturating_sub(start);
        if width > 0 {
            rects.push((
                *tab,
                Rect {
                    x: start,
                    y: area.y,
                    width,
                    height: 1,
                },
            ));
        }

        if index + 1 < StartupTab::ALL.len() {
            x = x.saturating_add(remaining_inline_width(x, right).min(divider_width));
        }
    }

    rects
}

fn selector_choice_areas(inner: Rect, labels: &[&str]) -> Vec<Rect> {
    if inner.width == 0 || inner.height == 0 {
        return Vec::new();
    }

    let mut x = inner.x;
    let right = inner.x.saturating_add(inner.width);
    let gap_width = Span::raw(" ").width() as u16;
    let mut rects = Vec::with_capacity(labels.len());

    for label in labels {
        if x >= right {
            break;
        }

        let start = x;
        let chip_width = selector_choice_span(label, Style::default()).width() as u16;
        x = x.saturating_add(remaining_inline_width(x, right).min(chip_width));
        x = x.saturating_add(remaining_inline_width(x, right).min(gap_width));
        let width = x.saturating_sub(start);
        if width > 0 {
            rects.push(Rect {
                x: start,
                y: inner.y,
                width,
                height: 1,
            });
        }
    }

    rects
}

fn remaining_inline_width(x: u16, right: u16) -> u16 {
    right.saturating_sub(x)
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn remap_navigation_key(key: KeyEvent) -> KeyEvent {
    let code = match key.code {
        KeyCode::Char('h') => KeyCode::Left,
        KeyCode::Char('j') => KeyCode::Down,
        KeyCode::Char('k') => KeyCode::Up,
        KeyCode::Char('l') => KeyCode::Right,
        other => other,
    };
    KeyEvent { code, ..key }
}

fn parse_u64_field(name: &str, raw: &str, default_value: u64) -> Result<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default_value);
    }
    let parsed = trimmed
        .parse::<u64>()
        .with_context(|| format!("{name}必须是整数，当前输入为 `{trimmed}`"))?;
    Ok(parsed.max(1))
}

fn parse_send_paths(raw: &str, cwd: &Path) -> Result<Vec<PathBuf>> {
    if raw.trim().is_empty() {
        bail!("Workspace 还没有填写发送路径；请输入路径，或用 `.` 表示当前目录");
    }

    let mut paths = Vec::new();
    for piece in raw.split(',') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        let expanded = expand_path_string(trimmed)?;
        let absolute = absolutize(expanded, cwd);
        paths.push(canonicalize_existing(&absolute)?);
    }

    if paths.is_empty() {
        bail!("发送路径不能为空；多个路径请用英文逗号分隔");
    }

    Ok(paths)
}

fn preview_directory_path(raw: &str, cwd: &Path) -> Result<(PathBuf, bool)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Workspace 还没有填写目录；请输入路径，或用 `.` 表示当前目录");
    }

    let path = absolutize(expand_path_string(trimmed)?, cwd);

    if path.exists() {
        let canonical =
            fs::canonicalize(&path).with_context(|| format!("无法访问目录 {}", path.display()))?;
        let metadata = fs::metadata(&canonical)
            .with_context(|| format!("无法读取目录信息 {}", canonical.display()))?;
        if !metadata.is_dir() {
            bail!("{} 不是目录", canonical.display());
        }
        Ok((canonical, false))
    } else {
        Ok((path, true))
    }
}

fn build_directory_path(raw: &str, cwd: &Path) -> Result<PathBuf> {
    let (path, _) = preview_directory_path(raw, cwd)?;
    Ok(path)
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        bail!("发送路径不存在: {}", path.display());
    }
    fs::canonicalize(path).with_context(|| format!("无法访问发送路径 {}", path.display()))
}

fn absolutize(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn trimmed_text(textarea: &TextArea<'_>) -> String {
    textarea
        .lines()
        .first()
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn trimmed_non_empty(textarea: &TextArea<'_>) -> Option<String> {
    let value = trimmed_text(textarea);
    if value.is_empty() { None } else { Some(value) }
}

fn equivalent_command_from_options(options: &RuntimeOptions, cwd: &Path) -> String {
    let mut args = vec!["synly".to_string()];
    if let Some(instance_name) = options.instance_name.as_deref() {
        push_flag_value(&mut args, "--name", instance_name.to_string());
    }

    push_flag_value(&mut args, "--fs", sync_mode_arg(options.mode).to_string());
    if matches!(options.mode, SyncMode::Both | SyncMode::Auto)
        && let Some(initial_sync) = options.workspace.initial_sync
    {
        push_flag_value(&mut args, "--initial", initial_sync.as_arg().to_string());
    }
    append_workspace_paths(&mut args, &options.workspace, cwd);
    args.push(connection_flag(options.connection).to_string());

    if options.workspace.incoming_root.is_some() {
        args.push(if options.sync_delete {
            "--sync-delete".to_string()
        } else {
            "--no-sync-delete".to_string()
        });
    }

    push_flag_value(
        &mut args,
        "--clipboard",
        clipboard_mode_arg(options.clipboard_mode).to_string(),
    );
    push_flag_value(
        &mut args,
        "--audio",
        audio_mode_arg(options.audio_mode).to_string(),
    );
    push_flag_value(
        &mut args,
        "--interval-secs",
        options.interval_secs.to_string(),
    );

    if let Some(max_folder_depth) = workspace_max_folder_depth(&options.workspace) {
        push_flag_value(
            &mut args,
            "--max-folder-depth",
            max_folder_depth.to_string(),
        );
    }

    if options.connection == ConnectionPreference::Join {
        if let Some(peer_query) = options.pairing.peer_query.as_deref() {
            push_flag_value(&mut args, "--peer", peer_query.to_string());
        }
        push_flag_value(
            &mut args,
            "--discovery-secs",
            options.pairing.discovery_secs.to_string(),
        );
    } else if let Some(port) = options.pairing.port {
        push_flag_value(&mut args, "--port", port.to_string());
    }

    if let Some(pin) = options.pairing.pin.as_deref() {
        push_flag_value(&mut args, "--pin", pin.to_string());
    }
    if options.pairing.accept {
        args.push("--accept".to_string());
    }
    if options.pairing.trust_device {
        args.push("--trust-device".to_string());
    }
    if options.pairing.trusted_only {
        args.push("--trusted-only".to_string());
    }

    shell_join(&args)
}

fn append_draft_paths(args: &mut Vec<String>, mode: SyncMode, raw: String) {
    match mode {
        SyncMode::Off => {}
        SyncMode::Send => {
            for path in raw
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
            {
                args.push(path.to_string());
            }
        }
        SyncMode::Receive | SyncMode::Both | SyncMode::Auto => {
            if !raw.is_empty() {
                args.push(raw);
            }
        }
    }
}

fn append_workspace_paths(args: &mut Vec<String>, workspace: &WorkspaceSpec, cwd: &Path) {
    match workspace.mode {
        SyncMode::Off => {}
        SyncMode::Send => match workspace.outgoing.as_ref() {
            Some(crate::sync::OutgoingSpec::RootContents { root, .. }) => {
                args.push(display_path_arg(root, cwd));
            }
            Some(crate::sync::OutgoingSpec::SelectedItems { items, .. }) => {
                for item in items {
                    args.push(display_path_arg(&item.path, cwd));
                }
            }
            None => {}
        },
        SyncMode::Receive | SyncMode::Both | SyncMode::Auto => {
            if let Some(path) = workspace.incoming_root.as_ref() {
                args.push(display_path_arg(path, cwd));
            }
        }
    }
}

fn workspace_max_folder_depth(workspace: &WorkspaceSpec) -> Option<usize> {
    match workspace.outgoing.as_ref() {
        Some(crate::sync::OutgoingSpec::RootContents {
            max_folder_depth, ..
        }) => *max_folder_depth,
        Some(crate::sync::OutgoingSpec::SelectedItems {
            max_folder_depth, ..
        }) => *max_folder_depth,
        None => None,
    }
}

fn connection_flag(connection: ConnectionPreference) -> &'static str {
    match connection {
        ConnectionPreference::Host => "--host",
        ConnectionPreference::Join => "--join",
    }
}

fn sync_mode_arg(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Off => "off",
        SyncMode::Send => "send",
        SyncMode::Receive => "receive",
        SyncMode::Both => "both",
        SyncMode::Auto => "auto",
    }
}

fn clipboard_mode_arg(mode: ClipboardMode) -> &'static str {
    match mode {
        ClipboardMode::Off => "off",
        ClipboardMode::Send => "send",
        ClipboardMode::Receive => "receive",
        ClipboardMode::Both => "both",
    }
}

fn audio_mode_arg(mode: AudioMode) -> &'static str {
    match mode {
        AudioMode::Off => "off",
        AudioMode::Send => "send",
        AudioMode::Receive => "receive",
    }
}

fn push_flag_value(args: &mut Vec<String>, flag: &str, value: String) {
    args.push(flag.to_string());
    args.push(value);
}

fn display_path_arg(path: &Path, cwd: &Path) -> String {
    if path == cwd {
        ".".to_string()
    } else if let Ok(relative) = path.strip_prefix(cwd) {
        if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            format!(".{}{}", std::path::MAIN_SEPARATOR, relative.display())
        }
    } else {
        path.display().to_string()
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '='))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{OutgoingSpec, WorkspaceSpec};

    #[test]
    fn tab_click_areas_follow_rendered_tab_widths() {
        let areas = tab_click_areas(Rect {
            x: 2,
            y: 1,
            width: 40,
            height: 1,
        });

        assert_eq!(
            areas,
            vec![
                (
                    StartupTab::Flow,
                    Rect {
                        x: 2,
                        y: 1,
                        width: 8,
                        height: 1,
                    },
                ),
                (
                    StartupTab::Workspace,
                    Rect {
                        x: 11,
                        y: 1,
                        width: 13,
                        height: 1,
                    },
                ),
                (
                    StartupTab::Pairing,
                    Rect {
                        x: 25,
                        y: 1,
                        width: 11,
                        height: 1,
                    },
                ),
            ]
        );
    }

    #[test]
    fn selector_choice_areas_follow_chip_widths() {
        let areas = selector_choice_areas(
            Rect {
                x: 5,
                y: 3,
                width: 20,
                height: 1,
            },
            &["A", "Long", "Z"],
        );

        assert_eq!(
            areas,
            vec![
                Rect {
                    x: 5,
                    y: 3,
                    width: 4,
                    height: 1,
                },
                Rect {
                    x: 9,
                    y: 3,
                    width: 7,
                    height: 1,
                },
                Rect {
                    x: 16,
                    y: 3,
                    width: 4,
                    height: 1,
                },
            ]
        );
    }

    #[test]
    fn equivalent_command_renders_effective_runtime_options() {
        let workspace = WorkspaceSpec {
            mode: SyncMode::Both,
            outgoing: Some(OutgoingSpec::RootContents {
                root: PathBuf::from("/tmp/demo"),
                max_folder_depth: Some(2),
            }),
            incoming_root: Some(PathBuf::from("/tmp/demo")),
            initial_sync: Some(InitialSyncMode::Other),
        };
        let options = RuntimeOptions {
            mode: SyncMode::Both,
            connection: ConnectionPreference::Join,
            instance_name: Some("worker-a".to_string()),
            workspace,
            sync_delete: false,
            clipboard_mode: ClipboardMode::Receive,
            audio_mode: AudioMode::Send,
            clipboard: ClipboardRuntimeOptions {
                max_file_bytes: 1,
                max_cache_bytes: None,
                cache_dir: PathBuf::from("/tmp/cache"),
            },
            transfer_limits: TransferLimits {
                max_meta_len: 1,
                max_frame_data_len: 1,
                max_clipboard_binary_len: 1,
            },
            interval_secs: 5,
            pairing: PairingRuntimeOptions {
                no_interact: false,
                peer_query: Some("studio display".to_string()),
                port: Some(7000),
                pin: Some("123456".to_string()),
                accept: true,
                trust_device: false,
                trusted_only: true,
                discovery_secs: 9,
            },
        };

        let command = equivalent_command_from_options(&options, Path::new("/tmp"));
        assert_eq!(
            command,
            "synly --name worker-a --fs both --initial other ./demo --join --no-sync-delete --clipboard receive --audio send --interval-secs 5 --max-folder-depth 2 --peer 'studio display' --discovery-secs 9 --pin 123456 --accept --trusted-only"
        );
    }

    #[test]
    fn shell_quote_wraps_whitespace_and_quotes() {
        assert_eq!(shell_quote("plain-value"), "plain-value");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's ready"), "'it'\"'\"'s ready'");
    }
}
