use crate::config::SynlyConfig;
use crate::path_expand::expand_path_string;
use crate::protocol::TransferLimits;
use crate::sync::WorkspaceSpec;
use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use console::{Term, style};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备、通过 PIN 配对、建立安全连接并持续同步文件与可选剪贴板"
)]
pub struct Cli {
    #[arg(
        long,
        help = "禁止进入启动交互；如果启动参数不完整，则直接报错并列出缺失项"
    )]
    pub no_interact: bool,
    #[arg(
        long = "fs",
        value_enum,
        help = "文件同步模式；默认 off，可选 off / send / receive / both / auto"
    )]
    pub fs: Option<SyncMode>,
    #[arg(long, conflicts_with = "join")]
    pub host: bool,
    #[arg(long, conflicts_with = "host")]
    pub join: bool,
    #[arg(long, conflicts_with = "no_sync_delete")]
    pub sync_delete: bool,
    #[arg(long, conflicts_with = "sync_delete")]
    pub no_sync_delete: bool,
    #[arg(
        long,
        value_enum,
        help = "剪贴板同步方向；默认关闭，可选 off / send / receive / both"
    )]
    pub clipboard: Option<ClipboardMode>,
    #[arg(
        long,
        value_enum,
        help = "音频同步模式；默认关闭，可选 off / send / receive"
    )]
    pub audio: Option<AudioMode>,
    #[arg(
        long,
        default_value_t = 3,
        help = "兜底全量重扫间隔（秒），目录变化仍会实时监听"
    )]
    pub interval_secs: u64,
    #[arg(
        long,
        help = "发送目录时允许递归进入的最大文件夹深度；0 表示只发送共享根目录下的直接内容，默认不限制"
    )]
    pub max_folder_depth: Option<usize>,
    #[arg(
        long,
        help = "join 模式下要连接的设备；可填写设备名、设备 ID 前缀或广播出的 IPv4 地址 (可带端口)"
    )]
    pub peer: Option<String>,
    #[arg(
        long,
        value_parser = clap::value_parser!(u16).range(1..),
        help = "host 模式下固定监听端口；留空则每次自动分配"
    )]
    pub port: Option<u16>,
    #[arg(
        long,
        help = "当前连接使用的 6 位 PIN；host 模式下会把它作为固定 PIN，join 模式下会直接使用它而不再询问"
    )]
    pub pin: Option<String>,
    #[arg(
        long,
        help = "对未受信任设备在认证通过后自动接受本次同步，不再二次确认；可信设备默认自动接受"
    )]
    pub accept: bool,
    #[arg(
        long,
        help = "在 PIN 认证成功后尽量建立可信设备绑定；host 端会直接记住对端，join 端会自动同意“是否信任服务端”的提示"
    )]
    pub trust_device: bool,
    #[arg(
        long,
        help = "只允许使用已建立的可信设备公钥；若未被信任则直接失败，不回退到 PIN"
    )]
    pub trusted_only: bool,
    #[arg(long, default_value_t = 3, help = "join 模式搜索设备时等待的秒数")]
    pub discovery_secs: u64,
    #[arg(
        value_name = "PATH",
        help = "文件同步路径；send 可传多个路径，receive / both / auto 只能传一个目录，off 不需要"
    )]
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Off,
    Send,
    Receive,
    Both,
    Auto,
}

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    #[default]
    Off,
    Send,
    Receive,
    Both,
}

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Off,
    Send,
    Receive,
}

impl AudioMode {
    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "关闭",
            AudioMode::Send => "发送",
            AudioMode::Receive => "接收",
        }
    }
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
            SyncMode::Off => "关闭文件同步",
            SyncMode::Send => "发送方",
            SyncMode::Receive => "接收方",
            SyncMode::Both => "双向同步",
            SyncMode::Auto => "自动协商",
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            SyncMode::Off => "off",
            SyncMode::Send => "send",
            SyncMode::Receive => "receive",
            SyncMode::Both => "both",
            SyncMode::Auto => "auto",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "send" => Some(Self::Send),
            "receive" => Some(Self::Receive),
            "both" => Some(Self::Both),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

impl ClipboardMode {
    pub fn can_send(self) -> bool {
        matches!(self, ClipboardMode::Send | ClipboardMode::Both)
    }

    pub fn can_receive(self) -> bool {
        matches!(self, ClipboardMode::Receive | ClipboardMode::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            ClipboardMode::Off => "关闭",
            ClipboardMode::Send => "发送方",
            ClipboardMode::Receive => "接收方",
            ClipboardMode::Both => "双向同步",
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
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
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
    pub no_interact: bool,
    pub peer_query: Option<String>,
    pub port: Option<u16>,
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
    let sync_delete_override = if cli.sync_delete {
        Some(true)
    } else if cli.no_sync_delete {
        Some(false)
    } else {
        None
    };

    let connection = match (cli.host, cli.join) {
        (true, false) => ConnectionPreference::Host,
        (false, true) => ConnectionPreference::Join,
        _ => bail!("missing connection preference"),
    };

    let mode = cli.fs.unwrap_or(SyncMode::Off);
    let workspace = workspace_from_cli_paths(mode, cli.paths)?;

    let workspace = workspace.with_max_folder_depth(cli.max_folder_depth);
    let sync_delete = if workspace.incoming_root.is_some() {
        sync_delete_override.unwrap_or(false)
    } else {
        false
    };
    let pin = cli.pin.as_deref().map(normalize_pin).transpose()?;
    let clipboard_mode = cli.clipboard.unwrap_or(ClipboardMode::Off);
    let audio_mode = cli.audio.unwrap_or(AudioMode::Off);

    Ok(RuntimeOptions {
        mode,
        connection,
        workspace,
        sync_delete,
        clipboard_mode,
        audio_mode,
        clipboard: ClipboardRuntimeOptions {
            max_file_bytes: config.clipboard.max_file_bytes,
            max_cache_bytes: config.clipboard.max_cache_bytes,
            cache_dir: config.clipboard_cache_dir()?,
        },
        transfer_limits: config.transfer.to_limits()?,
        interval_secs: cli.interval_secs.max(1),
        pairing: PairingRuntimeOptions {
            no_interact: cli.no_interact,
            peer_query: cli.peer.map(|value| value.trim().to_string()),
            port: cli.port,
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

    if cli.no_interact && cli.join && cli.peer.as_deref().unwrap_or("").trim().is_empty() {
        missing.push(
            "缺少目标设备：`--no-interact` + `--join` 时请传 `--peer` 指定目标设备".to_string(),
        );
    }

    match cli.fs {
        Some(SyncMode::Send) if cli.paths.is_empty() => {
            missing.push("缺少发送路径：请在 `--fs send` 后至少提供一个路径".to_string());
        }
        Some(SyncMode::Receive) if cli.paths.is_empty() => {
            missing.push("缺少接收目录：请在 `--fs receive` 时提供目录路径".to_string());
        }
        Some(SyncMode::Both) if cli.paths.is_empty() => {
            missing.push("缺少双向同步目录：请在 `--fs both` 时提供目录路径".to_string());
        }
        Some(SyncMode::Auto) if cli.paths.is_empty() => {
            missing.push("缺少共享目录：请在 `--fs auto` 时提供目录路径".to_string());
        }
        _ => {}
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

pub fn sync_delete_label(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
}

pub fn clipboard_mode_label(mode: ClipboardMode) -> &'static str {
    mode.label()
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

pub fn resolve_pairing_pin(pin: Option<&str>, no_interact: bool, prompt: &str) -> Result<String> {
    match pin {
        Some(pin) => normalize_pin(pin),
        None if no_interact => {
            bail!(
                "当前使用 `--no-interact`，请通过 `--pin` 提供 6 位 PIN，或先建立可信设备后再连接"
            )
        }
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

fn expand_path_list(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    paths.into_iter().map(expand_pathbuf).collect()
}

fn workspace_from_cli_paths(mode: SyncMode, paths: Vec<PathBuf>) -> Result<WorkspaceSpec> {
    match mode {
        SyncMode::Off => {
            if !paths.is_empty() {
                bail!("`--fs off` 不接受路径参数");
            }
            Ok(WorkspaceSpec::for_off())
        }
        SyncMode::Send => {
            if paths.is_empty() {
                bail!("`--fs send` 至少需要 1 个路径");
            }
            Ok(WorkspaceSpec::for_send(expand_path_list(paths)?)?)
        }
        SyncMode::Receive => Ok(WorkspaceSpec::for_receive(expand_single_path(
            paths, "receive",
        )?)?),
        SyncMode::Both => Ok(WorkspaceSpec::for_both(expand_single_path(paths, "both")?)?),
        SyncMode::Auto => Ok(WorkspaceSpec::for_auto(expand_single_path(paths, "auto")?)?),
    }
}

fn expand_single_path(paths: Vec<PathBuf>, mode_name: &str) -> Result<PathBuf> {
    match paths.len() {
        0 => bail!("`--fs {mode_name}` 需要 1 个目录路径"),
        1 => expand_pathbuf(paths.into_iter().next().expect("path length checked")),
        _ => bail!("`--fs {mode_name}` 只能提供 1 个目录路径"),
    }
}

fn expand_pathbuf(path: PathBuf) -> Result<PathBuf> {
    expand_path_string(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClipboardConfig, DeviceConfig, TransferConfig};
    use clap::Parser;
    use uuid::Uuid;

    #[test]
    fn global_flags_parse_before_and_after_paths() {
        let before = Cli::try_parse_from([
            "synly",
            "--join",
            "--no-sync-delete",
            "--fs",
            "receive",
            "--clipboard",
            "both",
            "--port",
            "7070",
            "--interval-secs",
            "9",
            "--max-folder-depth",
            "2",
            ".",
        ])
        .unwrap();
        let after = Cli::try_parse_from([
            "synly",
            ".",
            "--join",
            "--no-sync-delete",
            "--fs",
            "receive",
            "--clipboard",
            "both",
            "--port",
            "7070",
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
    fn conflicting_connection_flags_still_conflict() {
        let result = Cli::try_parse_from(["synly", "--fs", "receive", ".", "--host", "--join"]);
        assert!(result.is_err());
    }

    #[test]
    fn collect_runtime_options_uses_fs_flag_and_paths() {
        let cli = Cli::try_parse_from([
            "synly",
            "--join",
            "--fs",
            "receive",
            "--no-sync-delete",
            "--clipboard",
            "off",
            "--interval-secs",
            "9",
            "--max-folder-depth",
            "4",
            ".",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert!(matches!(options.connection, ConnectionPreference::Join));
        assert_eq!(options.mode, SyncMode::Receive);
        assert!(!options.sync_delete);
        assert_eq!(options.clipboard_mode, ClipboardMode::Off);
        assert_eq!(options.interval_secs, 9);
        assert_eq!(
            options
                .workspace
                .summary(ClipboardMode::Off)
                .max_folder_depth,
            None
        );
        assert!(options.workspace.incoming_root.is_some());
    }

    #[test]
    fn collect_runtime_options_applies_max_folder_depth_to_outgoing_workspace() {
        let cli = Cli::try_parse_from([
            "synly",
            "--join",
            "--fs",
            "both",
            "--no-sync-delete",
            "--clipboard",
            "send",
            "--max-folder-depth",
            "4",
            ".",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert_eq!(
            options
                .workspace
                .summary(ClipboardMode::Send)
                .max_folder_depth,
            Some(4)
        );
    }

    #[test]
    fn collect_runtime_options_captures_pairing_flags() {
        let cli = Cli::try_parse_from([
            "synly",
            "--join",
            "--fs",
            "both",
            "--peer",
            "demo-device",
            "--port",
            "7373",
            "--pin",
            "123456",
            "--no-sync-delete",
            "--clipboard",
            "receive",
            "--accept",
            "--trust-device",
            "--trusted-only",
            "--discovery-secs",
            "7",
            ".",
        ])
        .unwrap();

        let options = collect_runtime_options(cli, &test_config()).unwrap();

        assert!(matches!(options.connection, ConnectionPreference::Join));
        assert_eq!(options.pairing.peer_query.as_deref(), Some("demo-device"));
        assert_eq!(options.pairing.port, Some(7373));
        assert_eq!(options.pairing.pin.as_deref(), Some("123456"));
        assert!(options.pairing.accept);
        assert!(options.pairing.trust_device);
        assert!(options.pairing.trusted_only);
        assert_eq!(options.pairing.discovery_secs, 7);
    }

    #[test]
    fn collect_runtime_options_defaults_audio_mode_off_and_accepts_explicit_audio_role() {
        let default_cli = Cli::try_parse_from([
            "synly",
            "--fs",
            "receive",
            ".",
            "--join",
            "--no-sync-delete",
        ])
        .unwrap();
        let default_options = collect_runtime_options(default_cli, &test_config()).unwrap();
        assert_eq!(default_options.audio_mode, AudioMode::Off);
        assert_eq!(default_options.clipboard_mode, ClipboardMode::Off);

        let explicit_cli = Cli::try_parse_from([
            "synly",
            "--join",
            "--fs",
            "receive",
            "--no-sync-delete",
            "--audio",
            "receive",
            ".",
        ])
        .unwrap();
        let explicit_options = collect_runtime_options(explicit_cli, &test_config()).unwrap();
        assert_eq!(explicit_options.audio_mode, AudioMode::Receive);
    }

    #[test]
    fn normalize_pin_requires_six_digits() {
        assert_eq!(normalize_pin("001234").unwrap(), "001234");
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("12ab56").is_err());
    }

    #[test]
    fn requires_startup_tui_when_connection_or_path_is_missing() {
        let missing_connection =
            Cli::try_parse_from(["synly", "--fs", "both", ".", "--no-sync-delete"]).unwrap();
        assert!(!missing_startup_requirements(&missing_connection).is_empty());

        let missing_path =
            Cli::try_parse_from(["synly", "--fs", "receive", "--host", "--no-sync-delete"])
                .unwrap();
        assert!(!missing_startup_requirements(&missing_path).is_empty());
    }

    #[test]
    fn does_not_require_startup_tui_for_complete_noninteractive_cli() {
        let cli = Cli::try_parse_from([
            "synly",
            "--fs",
            "send",
            ".",
            "--join",
            "--peer",
            "demo-device",
        ])
        .unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
    }

    #[test]
    fn file_off_mode_without_path_does_not_require_startup_tui() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "off", "--host", "--clipboard", "both"]).unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert!(matches!(options.connection, ConnectionPreference::Host));
        assert_eq!(options.mode, SyncMode::Off);
        assert_eq!(options.clipboard_mode, ClipboardMode::Both);
        assert!(!options.workspace.file_sync_enabled());
    }

    #[test]
    fn omitted_fs_defaults_to_off() {
        let cli = Cli::try_parse_from(["synly", "--host", "--clipboard", "both"]).unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert_eq!(options.mode, SyncMode::Off);
        assert_eq!(options.clipboard_mode, ClipboardMode::Both);
        assert!(!options.workspace.file_sync_enabled());
    }

    #[test]
    fn no_interact_reports_missing_startup_requirements() {
        let cli = Cli::try_parse_from(["synly", "--fs", "receive", "--no-interact"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();

        assert!(err.contains("已禁止进入启动交互"));
        assert!(err.contains("`--host` 或 `--join`"));
        assert!(err.contains("`--fs receive`"));
    }

    #[test]
    fn no_interact_join_requires_peer() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "send", ".", "--join", "--no-interact"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();

        assert!(err.contains("`--peer`"));
    }

    #[test]
    fn fixed_port_must_be_positive() {
        let err = Cli::try_parse_from(["synly", "--port", "0", "--fs", "send", ".", "--host"])
            .unwrap_err()
            .to_string();

        assert!(err.contains("1.."));
    }

    #[test]
    fn receive_mode_rejects_multiple_paths() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "receive", "--join", ".", "./other"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();
        assert!(err.contains("只能提供 1 个目录路径"));
    }

    #[test]
    fn resolve_pairing_pin_requires_explicit_pin_in_no_interact() {
        let err = resolve_pairing_pin(None, true, "unused")
            .unwrap_err()
            .to_string();

        assert!(err.contains("`--pin`"));
    }

    fn assert_global_receive_cli(cli: Cli) {
        assert!(cli.join);
        assert!(!cli.host);
        assert!(cli.no_sync_delete);
        assert!(!cli.sync_delete);
        assert_eq!(cli.fs, Some(SyncMode::Receive));
        assert_eq!(cli.clipboard, Some(ClipboardMode::Both));
        assert_eq!(cli.port, Some(7070));
        assert_eq!(cli.interval_secs, 9);
        assert_eq!(cli.max_folder_depth, Some(2));
        assert_eq!(cli.paths, vec![std::path::PathBuf::from(".")]);
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
