use crate::config::{DeviceConfig, SynlyConfig};
use crate::path_expand::expand_path_string;
use crate::protocol::TransferLimits;
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use console::{Term, style};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备、通过 PIN 配对、建立安全连接并持续同步文件与可选剪贴板"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(
        long,
        global = true,
        help = "禁止进入启动交互；如果启动参数不完整，则直接报错并列出缺失项"
    )]
    pub no_interact: bool,
    #[arg(long, global = true, conflicts_with = "join")]
    pub host: bool,
    #[arg(long, global = true, conflicts_with = "host")]
    pub join: bool,
    #[arg(long, global = true, conflicts_with = "no_sync_delete")]
    pub sync_delete: bool,
    #[arg(long, global = true, conflicts_with = "sync_delete")]
    pub no_sync_delete: bool,
    #[arg(
        long,
        global = true,
        conflicts_with = "no_sync_clipboard",
        help = "开启剪贴板同步；支持文本、富文本、图片和限制大小内的文件，只有双方都开启才会生效，方向跟随当前同步模式"
    )]
    pub sync_clipboard: bool,
    #[arg(long, global = true, conflicts_with = "sync_clipboard")]
    pub no_sync_clipboard: bool,
    #[arg(
        long,
        global = true,
        conflicts_with = "no_sync_clipboard",
        help = "仅同步剪贴板，不进行文件同步；会自动开启剪贴板同步"
    )]
    pub clipboard_only: bool,
    #[arg(
        long,
        global = true,
        default_value_t = 3,
        help = "兜底全量重扫间隔（秒），目录变化仍会实时监听"
    )]
    pub interval_secs: u64,
    #[arg(
        long,
        global = true,
        help = "发送目录时允许递归进入的最大文件夹深度；0 表示只发送共享根目录下的直接内容，默认不限制"
    )]
    pub max_folder_depth: Option<usize>,
    #[arg(
        long,
        global = true,
        help = "join 模式下要连接的设备；可填写设备名、设备 ID 前缀或广播出的 IPv4 地址"
    )]
    pub peer: Option<String>,
    #[arg(
        long,
        global = true,
        help = "当前连接使用的 6 位 PIN；host 模式下会把它作为固定 PIN，join 模式下会直接使用它而不再询问"
    )]
    pub pin: Option<String>,
    #[arg(
        long,
        global = true,
        help = "对未受信任设备在认证通过后自动接受本次同步，不再二次确认；可信设备默认自动接受"
    )]
    pub accept: bool,
    #[arg(
        long,
        global = true,
        help = "在 PIN 认证成功后尽量建立可信设备绑定；host 端会直接记住对端，join 端会自动同意“是否信任服务端”的提示"
    )]
    pub trust_device: bool,
    #[arg(
        long,
        global = true,
        help = "只允许使用已建立的可信设备公钥；若未被信任则直接失败，不回退到 PIN"
    )]
    pub trusted_only: bool,
    #[arg(
        long,
        global = true,
        default_value_t = 3,
        help = "join 模式搜索设备时等待的秒数"
    )]
    pub discovery_secs: u64,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Send { paths: Vec<PathBuf> },
    Receive { path: Option<PathBuf> },
    Both { path: Option<PathBuf> },
    Auto { path: Option<PathBuf> },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Send,
    Receive,
    Both,
    Auto,
}

impl SyncMode {
    pub fn can_send(self) -> bool {
        matches!(self, SyncMode::Send | SyncMode::Both | SyncMode::Auto)
    }

    pub fn can_receive(self) -> bool {
        matches!(self, SyncMode::Receive | SyncMode::Both | SyncMode::Auto)
    }

    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Send => "发送方",
            SyncMode::Receive => "接收方",
            SyncMode::Both => "双向同步",
            SyncMode::Auto => "自动协商",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            SyncMode::Send => "send",
            SyncMode::Receive => "receive",
            SyncMode::Both => "both",
            SyncMode::Auto => "auto",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "send" => Some(Self::Send),
            "receive" => Some(Self::Receive),
            "both" => Some(Self::Both),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionPreference {
    Host,
    Join,
}

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub mode: SyncMode,
    pub connection: ConnectionPreference,
    pub workspace: WorkspaceSpec,
    pub sync_delete: bool,
    pub sync_clipboard: bool,
    pub clipboard: ClipboardRuntimeOptions,
    pub transfer_limits: TransferLimits,
    pub interval_secs: u64,
    pub pairing: PairingRuntimeOptions,
}

#[derive(Clone, Debug)]
pub struct ClipboardRuntimeOptions {
    pub max_file_bytes: u64,
    pub max_cache_bytes: Option<u64>,
    pub cache_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PairingRuntimeOptions {
    pub peer_query: Option<String>,
    pub pin: Option<String>,
    pub accept: bool,
    pub trust_device: bool,
    pub trusted_only: bool,
    pub discovery_secs: u64,
}

pub fn collect_runtime_options(cli: Cli, config: &SynlyConfig) -> Result<RuntimeOptions> {
    let startup_requirements = missing_startup_requirements(&cli);
    if cli.no_interact && !startup_requirements.is_empty() {
        bail!(
            "{}",
            format_missing_startup_requirements(&startup_requirements)
        );
    }
    if !startup_requirements.is_empty() {
        return crate::startup_tui::collect_runtime_options_tui(cli, config);
    }
    collect_runtime_options_from_cli(cli, config)
}

fn collect_runtime_options_from_cli(cli: Cli, config: &SynlyConfig) -> Result<RuntimeOptions> {
    let device = &config.device;
    let sync_delete_override = if cli.sync_delete {
        Some(true)
    } else if cli.no_sync_delete {
        Some(false)
    } else {
        None
    };
    let sync_clipboard_override = if cli.sync_clipboard {
        Some(true)
    } else if cli.no_sync_clipboard {
        Some(false)
    } else {
        None
    };

    let connection = if cli.host {
        ConnectionPreference::Host
    } else if cli.join {
        ConnectionPreference::Join
    } else {
        choose_connection()?
    };

    let clipboard_only = if cli.clipboard_only {
        true
    } else if cli.command.is_none() {
        choose_clipboard_only()?
    } else {
        false
    };

    let mode = match &cli.command {
        Some(Command::Send { .. }) => SyncMode::Send,
        Some(Command::Receive { .. }) => SyncMode::Receive,
        Some(Command::Both { .. }) => SyncMode::Both,
        Some(Command::Auto { .. }) => SyncMode::Auto,
        None => choose_mode(device, connection, clipboard_only)?,
    };

    let workspace = if clipboard_only {
        WorkspaceSpec::for_clipboard_only(mode)
    } else {
        match cli.command {
            Some(Command::Send { paths }) => WorkspaceSpec::for_send(paths)?,
            Some(Command::Receive { path }) => {
                let destination = resolve_receive_path(path)?;
                WorkspaceSpec::for_receive(destination)?
            }
            Some(Command::Both { path }) => {
                let root = resolve_both_path(path)?;
                WorkspaceSpec::for_both(root)?
            }
            Some(Command::Auto { path }) => {
                let root = resolve_auto_path(path)?;
                WorkspaceSpec::for_auto(root)?
            }
            None => interactive_workspace(mode)?,
        }
    }
    .with_max_folder_depth(cli.max_folder_depth);
    let sync_delete = resolve_sync_delete(sync_delete_override, &workspace)?;
    let sync_clipboard = resolve_sync_clipboard(sync_clipboard_override, clipboard_only)?;
    let pin = cli.pin.as_deref().map(normalize_pin).transpose()?;

    Ok(RuntimeOptions {
        mode,
        connection,
        workspace,
        sync_delete,
        sync_clipboard,
        clipboard: ClipboardRuntimeOptions {
            max_file_bytes: config.clipboard.max_file_bytes,
            max_cache_bytes: config.clipboard.max_cache_bytes,
            cache_dir: config.clipboard_cache_dir()?,
        },
        transfer_limits: config.transfer.to_limits()?,
        interval_secs: cli.interval_secs.max(1),
        pairing: PairingRuntimeOptions {
            peer_query: cli.peer.map(|value| value.trim().to_string()),
            pin,
            accept: cli.accept,
            trust_device: cli.trust_device,
            trusted_only: cli.trusted_only,
            discovery_secs: cli.discovery_secs.max(1),
        },
    })
}

fn missing_startup_requirements(cli: &Cli) -> Vec<String> {
    let mut missing = Vec::new();

    if !cli.host && !cli.join {
        missing.push("缺少连接方式：请传 `--host` 或 `--join`".to_string());
    }

    if cli.command.is_none() {
        missing.push("缺少同步模式：请传子命令 `send`、`receive`、`both` 或 `auto`".to_string());
    }

    if !sync_clipboard_is_explicit(cli) {
        missing.push(
            "缺少剪贴板同步策略：请传 `--sync-clipboard`、`--no-sync-clipboard`，或使用 `--clipboard-only`"
                .to_string(),
        );
    }

    if cli.clipboard_only {
        return missing;
    }

    match &cli.command {
        Some(Command::Send { paths }) if paths.is_empty() => {
            missing.push("缺少发送路径：请在 `send` 后至少提供一个路径".to_string());
        }
        Some(Command::Receive { path }) if path.is_none() => {
            missing.push("缺少接收目录：请在 `receive` 后提供目录路径".to_string());
        }
        Some(Command::Both { path }) if path.is_none() => {
            missing.push("缺少双向同步目录：请在 `both` 后提供目录路径".to_string());
        }
        Some(Command::Auto { path }) if path.is_none() => {
            missing.push("缺少共享目录：请在 `auto` 后提供目录路径".to_string());
        }
        _ => {}
    }

    if matches!(
        cli.command,
        Some(Command::Receive { .. } | Command::Both { .. } | Command::Auto { .. })
    ) && !sync_delete_is_explicit(cli)
    {
        missing.push("缺少删除同步策略：请传 `--sync-delete` 或 `--no-sync-delete`".to_string());
    }

    missing
}

fn format_missing_startup_requirements(missing: &[String]) -> String {
    let mut message =
        String::from("已禁止进入启动交互（`--no-interact`），但当前参数还不足以完成启动：");
    for item in missing {
        message.push_str("\n- ");
        message.push_str(item);
    }
    message
}

fn sync_delete_is_explicit(cli: &Cli) -> bool {
    cli.sync_delete || cli.no_sync_delete
}

fn sync_clipboard_is_explicit(cli: &Cli) -> bool {
    cli.clipboard_only || cli.sync_clipboard || cli.no_sync_clipboard
}

pub fn sync_delete_label(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
}

pub fn sync_clipboard_label(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
}

pub fn prompt_select(
    title: &str,
    options: &[String],
    default_index: Option<usize>,
) -> Result<usize> {
    if options.is_empty() {
        bail!("no options available for selection");
    }
    if let Some(default_index) = default_index
        && default_index >= options.len()
    {
        bail!("default selection index out of range");
    }

    let term = Term::stdout();
    term.write_line("")?;
    term.write_line(&style(title).bold().to_string())?;
    for (idx, option) in options.iter().enumerate() {
        let default_suffix = if default_index == Some(idx) {
            " [默认]"
        } else {
            ""
        };
        term.write_line(&format!("  {}. {}{}", idx + 1, option, default_suffix))?;
    }
    if let Some(index) = default_index {
        term.write_line(&format!("回车选择默认项 {}", index + 1))?;
    }

    loop {
        let raw = prompt_input("编号", None)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            if let Some(index) = default_index {
                return Ok(index);
            }
            term.write_line("请输入编号。")?;
            continue;
        }

        match trimmed.parse::<usize>() {
            Ok(number) if (1..=options.len()).contains(&number) => return Ok(number - 1),
            Ok(_) => {
                term.write_line("编号超出范围，请重新输入。")?;
            }
            Err(_) => {
                term.write_line("请输入有效编号。")?;
            }
        }
    }
}

pub fn prompt_input(label: &str, default: Option<&str>) -> Result<String> {
    let term = Term::stdout();
    let prompt = match default {
        Some(value) => format!("{} [{}]: ", label, value),
        None => format!("{}: ", label),
    };
    term.write_str(&prompt)?;
    let line = term.read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty()
        && let Some(value) = default
    {
        return Ok(value.to_string());
    }
    Ok(trimmed.to_string())
}

pub fn prompt_secret(label: &str) -> Result<String> {
    let term = Term::stdout();
    loop {
        term.write_line(label)?;
        let value = prompt_input("PIN", None)?;
        if value.is_empty() {
            term.write_line("输入不能为空，请重新输入。")?;
            continue;
        }
        match normalize_pin(&value) {
            Ok(pin) => return Ok(pin),
            Err(err) => {
                term.write_line(&format!("PIN 无效，请重新输入: {err:#}"))?;
            }
        }
    }
}

pub fn normalize_pin(pin: &str) -> Result<String> {
    let trimmed = pin.trim();
    if trimmed.len() != 6 || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("PIN 必须是 6 位数字");
    }
    Ok(trimmed.to_string())
}

pub fn require_peer_query(peer_query: Option<&str>) -> Result<&str> {
    match peer_query {
        Some(query) if !query.trim().is_empty() => Ok(query.trim()),
        _ => {
            bail!("join 模式下请用 --peer 指定要连接的设备（支持设备名、设备 ID 前缀或 IPv4 地址）")
        }
    }
}

pub fn resolve_pairing_pin(pin: Option<&str>, prompt: &str) -> Result<String> {
    match pin {
        Some(pin) => normalize_pin(pin),
        None => prompt_secret(prompt),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustPromptDecision {
    Accept,
    AcceptAndTrust,
    Reject,
}

pub fn prompt_confirm_with_trust(label: &str, default_trust: bool) -> Result<TrustPromptDecision> {
    let term = Term::stdout();
    loop {
        term.write_line(label)?;
        let raw = prompt_input("确认 [T/Y/n]", None)?;
        let trimmed = raw.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            return Ok(if default_trust {
                TrustPromptDecision::AcceptAndTrust
            } else {
                TrustPromptDecision::Accept
            });
        }
        match trimmed.as_str() {
            "t" | "trust" => return Ok(TrustPromptDecision::AcceptAndTrust),
            "y" | "yes" => return Ok(TrustPromptDecision::Accept),
            "n" | "no" => return Ok(TrustPromptDecision::Reject),
            _ => term.write_line("请输入 t、y 或 n。")?,
        }
    }
}

pub fn prompt_confirm(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let term = Term::stdout();
    loop {
        term.write_line(label)?;
        let raw = prompt_input(&format!("确认 {}", suffix), None)?;
        let trimmed = raw.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            return Ok(default);
        }
        match trimmed.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => term.write_line("请输入 y 或 n。")?,
        }
    }
}

fn resolve_sync_delete(
    sync_delete_override: Option<bool>,
    workspace: &WorkspaceSpec,
) -> Result<bool> {
    if workspace.incoming_root.is_none() {
        return Ok(false);
    }

    if let Some(sync_delete) = sync_delete_override {
        return Ok(sync_delete);
    }

    prompt_confirm("同步删除吗", false)
}

fn resolve_sync_clipboard(
    sync_clipboard_override: Option<bool>,
    clipboard_only: bool,
) -> Result<bool> {
    if clipboard_only {
        return Ok(true);
    }

    if let Some(sync_clipboard) = sync_clipboard_override {
        return Ok(sync_clipboard);
    }

    prompt_confirm(
        "同步剪贴板吗（支持文本/富文本/图片/文件，双方都开启才会生效）",
        false,
    )
}

fn choose_mode(
    device: &DeviceConfig,
    connection: ConnectionPreference,
    clipboard_only: bool,
) -> Result<SyncMode> {
    let options = match (connection, clipboard_only) {
        (ConnectionPreference::Host, false) => vec![
            "自动协商: 监听时根据客户端请求决定方向，使用同一个目录收发 (Recommended)".to_string(),
            "发送方: 把本地文件同步给对方".to_string(),
            "接收方: 接收对方同步过来的文件".to_string(),
            "双向同步: 两边都能发送和接收".to_string(),
        ],
        (ConnectionPreference::Join, false) => vec![
            "发送方: 把本地文件同步给对方".to_string(),
            "接收方: 接收对方同步过来的文件".to_string(),
            "双向同步: 两边都能发送和接收".to_string(),
            "自动协商: 使用同一个目录收发，并尽量根据对端能力协商".to_string(),
        ],
        (ConnectionPreference::Host, true) => vec![
            "自动协商: 监听时根据客户端请求决定剪贴板方向，不同步文件 (Recommended)".to_string(),
            "发送方: 把本机剪贴板同步给对方".to_string(),
            "接收方: 只接收对方剪贴板".to_string(),
            "双向同步: 两边都能发送和接收剪贴板".to_string(),
        ],
        (ConnectionPreference::Join, true) => vec![
            "发送方: 把本机剪贴板同步给对方".to_string(),
            "接收方: 只接收对方剪贴板".to_string(),
            "双向同步: 两边都能发送和接收剪贴板".to_string(),
            "自动协商: 不同步文件，只协商剪贴板方向".to_string(),
        ],
    };
    println!(
        "{} {} ({})",
        style("设备").bold(),
        device.device_name,
        device.short_id()
    );
    let default_index = match connection {
        ConnectionPreference::Host => Some(0),
        ConnectionPreference::Join => Some(3),
    };
    let index = prompt_select("同步模式", &options, default_index)?;
    Ok(match connection {
        ConnectionPreference::Host => match index {
            0 => SyncMode::Auto,
            1 => SyncMode::Send,
            2 => SyncMode::Receive,
            _ => SyncMode::Both,
        },
        ConnectionPreference::Join => match index {
            0 => SyncMode::Send,
            1 => SyncMode::Receive,
            2 => SyncMode::Both,
            _ => SyncMode::Auto,
        },
    })
}

fn choose_clipboard_only() -> Result<bool> {
    prompt_confirm("仅同步剪贴板，不同步文件吗", false)
}

fn choose_connection() -> Result<ConnectionPreference> {
    let options = vec![
        "等待别人连接，收到请求后显示本次 PIN".to_string(),
        "连接局域网中的设备，收到提示后输入对方当前显示的 PIN".to_string(),
    ];
    let index = prompt_select("连接方式", &options, Some(0))?;
    Ok(match index {
        0 => ConnectionPreference::Host,
        _ => ConnectionPreference::Join,
    })
}

fn interactive_workspace(mode: SyncMode) -> Result<WorkspaceSpec> {
    match mode {
        SyncMode::Send => {
            let paths = resolve_send_paths(None)?;
            WorkspaceSpec::for_send(paths)
        }
        SyncMode::Receive => {
            let path = resolve_receive_path(None)?;
            WorkspaceSpec::for_receive(path)
        }
        SyncMode::Both => {
            let path = resolve_both_path(None)?;
            WorkspaceSpec::for_both(path)
        }
        SyncMode::Auto => {
            let path = resolve_auto_path(None)?;
            WorkspaceSpec::for_auto(path)
        }
    }
}

fn resolve_send_paths(initial: Option<Vec<PathBuf>>) -> Result<Vec<PathBuf>> {
    if let Some(paths) = initial
        && !paths.is_empty()
    {
        return expand_path_list(paths);
    }

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let term = Term::stdout();
    loop {
        term.write_line("未指定同步源。")?;
        term.write_line(&format!("回车使用当前目录: {}", cwd.display()))?;
        term.write_line("多个路径用英文逗号分隔。")?;
        let raw = prompt_input("路径", None)?;
        if raw.trim().is_empty() {
            return Ok(vec![cwd.clone()]);
        }
        match parse_csv_paths(&raw) {
            Ok(paths) => return Ok(paths),
            Err(err) => term.write_line(&format!("输入无效，请重新输入: {err:#}"))?,
        }
    }
}

fn resolve_receive_path(initial: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    resolve_directory_path(initial, "未指定接收目录。", &cwd)
}

fn resolve_both_path(initial: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    resolve_directory_path(initial, "未指定双向同步目录。", &cwd)
}

fn resolve_auto_path(initial: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    resolve_directory_path(initial, "未指定共享目录。", &cwd)
}

fn resolve_directory_path(
    initial: Option<PathBuf>,
    prompt: &str,
    default_path: &Path,
) -> Result<PathBuf> {
    let term = Term::stdout();
    let mut next_candidate = initial;

    loop {
        let path = if let Some(candidate) = next_candidate.take() {
            match expand_pathbuf(candidate) {
                Ok(path) => path,
                Err(err) => {
                    term.write_line(&format!("路径无效，请重新输入: {err:#}"))?;
                    continue;
                }
            }
        } else {
            term.write_line(prompt)?;
            term.write_line(&format!("回车使用当前目录: {}", default_path.display()))?;
            let raw = prompt_input("目录", None)?;
            if raw.trim().is_empty() {
                default_path.to_path_buf()
            } else {
                match expand_path_string(&raw) {
                    Ok(path) => path,
                    Err(err) => {
                        term.write_line(&format!("路径无效，请重新输入: {err:#}"))?;
                        continue;
                    }
                }
            }
        };

        if path.exists() {
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => return Ok(path),
                Ok(_) => {
                    term.write_line("该路径存在，但不是目录，请重新输入。")?;
                }
                Err(err) => {
                    term.write_line(&format!("无法访问该路径，请重新输入: {err:#}"))?;
                }
            }
            continue;
        }

        term.write_line(&format!("目录不存在: {}", path.display()))?;
        if prompt_confirm("创建该目录吗", true)? {
            return Ok(path);
        }

        term.write_line("请重新输入目录。")?;
    }
}

fn parse_csv_paths(raw: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for piece in raw.split(',') {
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            paths.push(expand_path_string(trimmed)?);
        }
    }
    if paths.is_empty() {
        bail!("至少需要提供一个路径");
    }
    Ok(paths)
}

fn expand_path_list(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    paths.into_iter().map(expand_pathbuf).collect()
}

fn expand_pathbuf(path: PathBuf) -> Result<PathBuf> {
    expand_path_string(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClipboardConfig, TransferConfig};
    use clap::Parser;
    use uuid::Uuid;

    #[test]
    fn global_flags_parse_before_and_after_subcommand() {
        let before = Cli::try_parse_from([
            "synly",
            "--join",
            "--no-sync-delete",
            "--sync-clipboard",
            "--interval-secs",
            "9",
            "--max-folder-depth",
            "2",
            "receive",
            ".",
        ])
        .unwrap();
        let after = Cli::try_parse_from([
            "synly",
            "receive",
            ".",
            "--join",
            "--no-sync-delete",
            "--sync-clipboard",
            "--interval-secs",
            "9",
            "--max-folder-depth",
            "2",
        ])
        .unwrap();

        assert_global_receive_cli(before);
        assert_global_receive_cli(after);
    }

    #[test]
    fn conflicting_global_flags_still_conflict_after_subcommand() {
        let result = Cli::try_parse_from(["synly", "receive", ".", "--host", "--join"]);
        assert!(result.is_err());
    }

    #[test]
    fn collect_runtime_options_uses_subcommand_scoped_global_flags() {
        let cli = Cli::try_parse_from([
            "synly",
            "receive",
            ".",
            "--join",
            "--no-sync-delete",
            "--no-sync-clipboard",
            "--interval-secs",
            "9",
            "--max-folder-depth",
            "4",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert!(matches!(options.connection, ConnectionPreference::Join));
        assert_eq!(options.mode, SyncMode::Receive);
        assert!(!options.sync_delete);
        assert!(!options.sync_clipboard);
        assert_eq!(options.interval_secs, 9);
        assert_eq!(options.workspace.summary(false).max_folder_depth, None);
        assert!(options.workspace.incoming_root.is_some());
    }

    #[test]
    fn collect_runtime_options_applies_max_folder_depth_to_outgoing_workspace() {
        let cli = Cli::try_parse_from([
            "synly",
            "both",
            ".",
            "--join",
            "--no-sync-delete",
            "--no-sync-clipboard",
            "--max-folder-depth",
            "4",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert_eq!(options.workspace.summary(false).max_folder_depth, Some(4));
    }

    #[test]
    fn collect_runtime_options_captures_pairing_flags() {
        let cli = Cli::try_parse_from([
            "synly",
            "both",
            ".",
            "--join",
            "--peer",
            "demo-device",
            "--pin",
            "123456",
            "--no-sync-delete",
            "--no-sync-clipboard",
            "--accept",
            "--trust-device",
            "--trusted-only",
            "--discovery-secs",
            "7",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert!(matches!(options.connection, ConnectionPreference::Join));
        assert_eq!(options.pairing.peer_query.as_deref(), Some("demo-device"));
        assert_eq!(options.pairing.pin.as_deref(), Some("123456"));
        assert!(options.pairing.accept);
        assert!(options.pairing.trust_device);
        assert!(options.pairing.trusted_only);
        assert_eq!(options.pairing.discovery_secs, 7);
    }

    #[test]
    fn normalize_pin_requires_six_digits() {
        assert_eq!(normalize_pin("001234").unwrap(), "001234");
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("12ab56").is_err());
    }

    #[test]
    fn requires_startup_tui_when_connection_or_path_is_missing() {
        let missing_connection = Cli::try_parse_from([
            "synly",
            "both",
            ".",
            "--no-sync-delete",
            "--no-sync-clipboard",
        ])
        .unwrap();
        assert!(!missing_startup_requirements(&missing_connection).is_empty());

        let missing_path = Cli::try_parse_from([
            "synly",
            "receive",
            "--host",
            "--no-sync-delete",
            "--no-sync-clipboard",
        ])
        .unwrap();
        assert!(!missing_startup_requirements(&missing_path).is_empty());
    }

    #[test]
    fn does_not_require_startup_tui_for_complete_noninteractive_cli() {
        let cli = Cli::try_parse_from([
            "synly",
            "send",
            ".",
            "--join",
            "--no-sync-clipboard",
            "--peer",
            "demo-device",
        ])
        .unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
    }

    #[test]
    fn clipboard_only_send_without_path_does_not_require_startup_tui() {
        let cli = Cli::try_parse_from(["synly", "send", "--host", "--clipboard-only"]).unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert!(matches!(options.connection, ConnectionPreference::Host));
        assert_eq!(options.mode, SyncMode::Send);
        assert!(options.sync_clipboard);
        assert!(!options.workspace.file_sync_enabled());
    }

    #[test]
    fn no_interact_reports_missing_startup_requirements() {
        let cli = Cli::try_parse_from(["synly", "receive", "--no-interact"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();

        assert!(err.contains("已禁止进入启动交互"));
        assert!(err.contains("`--host` 或 `--join`"));
        assert!(err.contains("`--sync-clipboard`、`--no-sync-clipboard`"));
        assert!(err.contains("`receive` 后提供目录路径"));
        assert!(err.contains("`--sync-delete` 或 `--no-sync-delete`"));
    }

    fn assert_global_receive_cli(cli: Cli) {
        assert!(cli.join);
        assert!(!cli.host);
        assert!(cli.no_sync_delete);
        assert!(!cli.sync_delete);
        assert!(cli.sync_clipboard);
        assert!(!cli.no_sync_clipboard);
        assert_eq!(cli.interval_secs, 9);
        assert_eq!(cli.max_folder_depth, Some(2));
        match cli.command {
            Some(Command::Receive { path }) => {
                assert_eq!(path, Some(std::path::PathBuf::from(".")));
            }
            other => panic!("expected receive command, got {other:?}"),
        }
    }

    fn test_config() -> SynlyConfig {
        SynlyConfig {
            device: DeviceConfig {
                device_id: Uuid::nil(),
                device_name: "test-device".to_string(),
                identity_private_key: None,
                identity_public_key: None,
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            trusted_devices: Vec::new(),
        }
    }
}
