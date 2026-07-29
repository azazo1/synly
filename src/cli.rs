use crate::config::{DiscoveryConfig, RuntimeConfig, SynlyConfig};
use crate::clipboard::ClipboardRuntimeOptions;
use crate::input::{Hotkey, InputMode, InputRuntimeOptions, ScreenEdge};
use crate::path_expand::expand_path_string;
use crate::protocol::{RuntimeCapabilities, TransferLimits};
use crate::runtime_control::{RuntimeControl, RuntimeTuning};
pub use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode,
};
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备、通过 PIN 配对、建立安全连接并持续同步文件与可选剪贴板"
)]
pub struct Cli {
    #[arg(long, help = "以无界面模式运行; 所有必要参数必须显式提供")]
    pub headless: bool,
    #[arg(
        long,
        conflicts_with = "no_notifications",
        help = "为本次运行启用连接成功和断开的系统提醒"
    )]
    pub notifications: bool,
    #[arg(
        long,
        conflicts_with = "notifications",
        help = "为本次运行关闭连接成功和断开的系统提醒"
    )]
    pub no_notifications: bool,
    #[arg(
        long = "fs",
        value_enum,
        help = "文件同步模式；默认 off，可选 off / send / receive / both / auto"
    )]
    pub fs: Option<FileSyncMode>,
    #[arg(
        long,
        help = "当前实例名；仅影响本次运行的发现与配对显示，不会写入配置"
    )]
    pub name: Option<String>,
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
        long = "input",
        value_enum,
        help = "鼠标键盘同步方向; 默认关闭, 可选 off / send / receive"
    )]
    pub input_mode: Option<InputMode>,
    #[arg(
        long,
        value_enum,
        help = "发送鼠标键盘时跨入对端的屏幕边缘; 默认 right"
    )]
    pub input_edge: Option<ScreenEdge>,
    #[arg(
        long,
        default_value = Hotkey::DEFAULT,
        help = "紧急收回控制的全局热键"
    )]
    pub input_hotkey: String,
    #[arg(
        long,
        value_enum,
        help = "双向/自动文件同步时的初始状态来源；this 表示本机目录先作为初始状态，other 表示先采用对端目录"
    )]
    pub initial: Option<InitialSyncMode>,
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
        help = "join 模式下要连接的设备；可填写实例名、设备名、设备 ID 前缀或广播出的 IPv4 地址 (可带端口)"
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

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub file_sync_mode: FileSyncMode,
    pub connection: ConnectionPreference,
    pub instance_name: Option<String>,
    pub workspace: WorkspaceSpec,
    pub sync_delete: bool,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input_mode: InputMode,
    pub input: InputRuntimeOptions,
    pub notifications_enabled: bool,
    pub discovery: DiscoveryConfig,
    pub clipboard: ClipboardRuntimeOptions,
    pub transfer_limits: TransferLimits,
    pub interval_secs: u64,
    pub pairing: PairingRuntimeOptions,
    pub control: RuntimeControl,
}

#[derive(Clone, Debug)]
pub struct PairingRuntimeOptions {
    pub headless: bool,
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
    if !startup_requirements.is_empty() {
        bail!(
            "{}",
            format_missing_startup_requirements(&startup_requirements)
        );
    }
    collect_runtime_options_from_cli(cli, config)
}

pub fn cli_from_runtime_config(
    runtime: &RuntimeConfig,
    pin: Option<String>,
    headless: bool,
) -> Cli {
    Cli {
        headless,
        notifications: false,
        no_notifications: false,
        fs: Some(runtime.file_sync_mode),
        name: normalize_optional_text(Some(runtime.instance_name.clone())),
        host: runtime.connection == Some(ConnectionPreference::Host),
        join: runtime.connection == Some(ConnectionPreference::Join),
        sync_delete: runtime.sync_delete,
        no_sync_delete: !runtime.sync_delete,
        clipboard: Some(runtime.clipboard_mode),
        audio: Some(runtime.audio_mode),
        input_mode: Some(runtime.input_mode),
        input_edge: (runtime.input_mode == InputMode::Send).then_some(runtime.input_edge),
        input_hotkey: runtime.input_hotkey.clone(),
        initial: runtime.initial,
        interval_secs: runtime.interval_secs.max(1),
        max_folder_depth: runtime.max_folder_depth,
        peer: normalize_optional_text(Some(runtime.peer_query.clone())),
        port: runtime.port,
        pin,
        accept: runtime.accept,
        trust_device: runtime.trust_device,
        trusted_only: runtime.trusted_only,
        discovery_secs: 3,
        paths: runtime.paths.clone(),
    }
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

    let notifications_enabled =
        resolve_notifications_enabled(&cli, config.notifications.enabled);
    let file_sync_mode = cli.fs.unwrap_or(FileSyncMode::Off);
    let workspace = workspace_from_cli_paths(file_sync_mode, cli.paths, cli.initial)?;

    let workspace = workspace.with_max_folder_depth(cli.max_folder_depth);
    let sync_delete = if workspace.incoming_root.is_some() {
        sync_delete_override.unwrap_or(false)
    } else {
        false
    };
    let pin = cli.pin.as_deref().map(normalize_pin).transpose()?;
    let clipboard_mode = cli.clipboard.unwrap_or(ClipboardMode::Off);
    let audio_mode = cli.audio.unwrap_or(AudioMode::Off);
    let input_mode = cli.input_mode.unwrap_or(InputMode::Off);
    if input_mode != InputMode::Send && cli.input_edge.is_some() {
        bail!("`--input-edge` 只能和 `--input send` 一起使用");
    }
    let input = InputRuntimeOptions {
        mode: input_mode,
        edge: cli.input_edge.unwrap_or(ScreenEdge::Right),
        hotkey: cli.input_hotkey.parse()?,
    };
    let clipboard = ClipboardRuntimeOptions {
        max_file_bytes: config.clipboard.max_file_bytes,
        max_cache_bytes: config.clipboard.max_cache_bytes,
        cache_dir: config.clipboard_cache_dir()?,
    };
    let capabilities = RuntimeCapabilities {
        clipboard_mode,
        audio_mode,
        input_mode,
    };
    let instance_name = normalize_optional_text(cli.name.clone());
    let tuning = RuntimeTuning {
        interval_secs: cli.interval_secs.max(1),
        sync_delete,
        notifications_enabled,
        device_name: config.device.device_name.clone(),
        instance_name: instance_name.clone(),
        discovery: config.discovery.clone(),
        input: input.clone(),
        clipboard: clipboard.clone(),
    };
    Ok(RuntimeOptions {
        file_sync_mode,
        connection,
        instance_name,
        workspace,
        sync_delete,
        clipboard_mode,
        audio_mode,
        input_mode,
        input,
        notifications_enabled,
        discovery: config.discovery.clone(),
        clipboard,
        transfer_limits: config.transfer.to_limits()?,
        interval_secs: cli.interval_secs.max(1),
        pairing: PairingRuntimeOptions {
            headless: cli.headless,
            peer_query: cli.peer.map(|value| value.trim().to_string()),
            port: cli.port,
            pin,
            accept: cli.accept,
            trust_device: cli.trust_device,
            trusted_only: cli.trusted_only,
            discovery_secs: cli.discovery_secs.max(1),
        },
        control: RuntimeControl::detached(capabilities, tuning),
    })
}

pub fn resolve_notifications_enabled(cli: &Cli, configured: bool) -> bool {
    if cli.notifications {
        true
    } else if cli.no_notifications {
        false
    } else {
        configured
    }
}

fn missing_startup_requirements(cli: &Cli) -> Vec<String> {
    let mut missing = Vec::new();

    if !cli.host && !cli.join {
        missing.push("缺少连接方式：请传 `--host` 或 `--join`".to_string());
    }

    if cli.headless && cli.join && cli.peer.as_deref().unwrap_or("").trim().is_empty() {
        missing.push(
            "缺少目标设备: headless join 需要通过 `--peer` 指定目标设备".to_string(),
        );
    }

    if cli.headless && cli.host && !cli.trusted_only && cli.pin.is_none() {
        missing.push(
            "缺少固定 PIN: headless host 需要通过 `--pin` 提供 6 位 PIN, 或启用 `--trusted-only`"
                .to_string(),
        );
    }

    match cli.fs {
        Some(FileSyncMode::Send) if cli.paths.is_empty() => {
            missing.push("缺少发送路径：请在 `--fs send` 后至少提供一个路径".to_string());
        }
        Some(FileSyncMode::Receive) if cli.paths.is_empty() => {
            missing.push("缺少接收目录：请在 `--fs receive` 时提供目录路径".to_string());
        }
        Some(FileSyncMode::Both) if cli.paths.is_empty() => {
            missing.push("缺少双向同步目录：请在 `--fs both` 时提供目录路径".to_string());
        }
        Some(FileSyncMode::Auto) if cli.paths.is_empty() => {
            missing.push("缺少共享目录：请在 `--fs auto` 时提供目录路径".to_string());
        }
        _ => {}
    }

    match cli.fs {
        Some(FileSyncMode::Both | FileSyncMode::Auto) if cli.initial.is_none() => {
            missing.push(
                "缺少初始状态来源：`--fs both/auto` 时请传 `--initial this` 或 `--initial other`"
                    .to_string(),
            );
        }
        _ => {}
    }

    missing
}

fn format_missing_startup_requirements(missing: &[String]) -> String {
    let mut message = String::from("headless 参数不足, 无法启动:");
    for item in missing {
        message.push_str("\n- ");
        message.push_str(item);
    }
    message
}

pub fn sync_delete_label(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
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
            bail!(
                "join 模式下请用 `--peer` 指定要连接的设备(支持实例名, 设备名, 设备 ID 前缀, IPv4 地址, 或完整的 IPv4:端口直连)"
            )
        }
    }
}

fn expand_path_list(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    paths.into_iter().map(expand_pathbuf).collect()
}

fn workspace_from_cli_paths(
    file_sync_mode: FileSyncMode,
    paths: Vec<PathBuf>,
    initial: Option<InitialSyncMode>,
) -> Result<WorkspaceSpec> {
    match file_sync_mode {
        FileSyncMode::Off => {
            if initial.is_some() {
                bail!("`--initial` 只能和 `--fs both` 或 `--fs auto` 一起使用");
            }
            if !paths.is_empty() {
                bail!("`--fs off` 不接受路径参数");
            }
            Ok(WorkspaceSpec::for_off())
        }
        FileSyncMode::Send => {
            if initial.is_some() {
                bail!("`--initial` 只能和 `--fs both` 或 `--fs auto` 一起使用");
            }
            if paths.is_empty() {
                bail!("`--fs send` 至少需要 1 个路径");
            }
            Ok(WorkspaceSpec::for_send(expand_path_list(paths)?)?)
        }
        FileSyncMode::Receive => {
            if initial.is_some() {
                bail!("`--initial` 只能和 `--fs both` 或 `--fs auto` 一起使用");
            }
            Ok(WorkspaceSpec::for_receive(expand_single_path(
                paths, "receive",
            )?)?)
        }
        FileSyncMode::Both => Ok(WorkspaceSpec::for_both(expand_single_path(paths, "both")?)?
            .with_initial_sync(Some(
                initial.context("`--fs both` 时必须传 `--initial this` 或 `--initial other`")?,
            ))),
        FileSyncMode::Auto => Ok(WorkspaceSpec::for_auto(expand_single_path(paths, "auto")?)?
            .with_initial_sync(Some(
                initial.context("`--fs auto` 时必须传 `--initial this` 或 `--initial other`")?,
            ))),
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

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ClipboardConfig, DeviceConfig, DiscoveryConfig, NotificationConfig, TransferConfig,
    };
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
    fn notification_flags_conflict_and_override_config() {
        assert!(
            Cli::try_parse_from([
                "synly",
                "--fs",
                "off",
                "--host",
                "--notifications",
                "--no-notifications",
            ])
            .is_err()
        );

        let mut config = test_config();
        config.notifications.enabled = false;
        let default_options = collect_runtime_options(
            Cli::try_parse_from(["synly", "--fs", "off", "--host"]).unwrap(),
            &config,
        )
        .unwrap();
        let overridden_options = collect_runtime_options(
            Cli::try_parse_from([
                "synly",
                "--fs",
                "off",
                "--host",
                "--notifications",
            ])
            .unwrap(),
            &config,
        )
        .unwrap();

        assert!(!default_options.notifications_enabled);
        assert!(overridden_options.notifications_enabled);
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
        assert_eq!(options.file_sync_mode, FileSyncMode::Receive);
        assert!(!options.sync_delete);
        assert_eq!(options.clipboard_mode, ClipboardMode::Off);
        assert_eq!(options.interval_secs, 9);
        assert_eq!(
            options
                .workspace
                .session_summary(ClipboardMode::Off, AudioMode::Off, InputMode::Off)
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
            "--initial",
            "this",
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
                .session_summary(ClipboardMode::Send, AudioMode::Off, InputMode::Off)
                .max_folder_depth,
            Some(4)
        );
        assert_eq!(
            options
                .workspace
                .session_summary(ClipboardMode::Send, AudioMode::Off, InputMode::Off)
                .initial_sync,
            Some(InitialSyncMode::This)
        );
    }

    #[test]
    fn collect_runtime_options_captures_pairing_flags() {
        let cli = Cli::try_parse_from([
            "synly",
            "--name",
            "worker-a",
            "--join",
            "--fs",
            "both",
            "--initial",
            "other",
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
        assert_eq!(options.instance_name.as_deref(), Some("worker-a"));
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
    fn collect_runtime_options_parses_input_controls() {
        let cli = Cli::try_parse_from([
            "synly",
            "--host",
            "--fs",
            "off",
            "--input",
            "send",
            "--input-edge",
            "left",
            "--input-hotkey",
            "ctrl+shift+f12",
        ])
        .unwrap();
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert_eq!(options.input_mode, InputMode::Send);
        assert_eq!(options.input.edge, ScreenEdge::Left);
        assert_eq!(options.input.hotkey.to_string(), "ctrl+shift+f12");
    }

    #[test]
    fn input_edge_requires_send_mode() {
        let cli = Cli::try_parse_from([
            "synly",
            "--host",
            "--fs",
            "off",
            "--input",
            "receive",
            "--input-edge",
            "left",
        ])
        .unwrap();
        assert!(collect_runtime_options(cli, &test_config()).is_err());
    }

    #[test]
    fn normalize_pin_requires_six_digits() {
        assert_eq!(normalize_pin("001234").unwrap(), "001234");
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("12ab56").is_err());
    }

    #[test]
    fn reports_missing_headless_connection_or_path_requirements() {
        let missing_connection = Cli::try_parse_from([
            "synly",
            "--fs",
            "both",
            "--initial",
            "this",
            ".",
            "--no-sync-delete",
        ])
        .unwrap();
        assert!(!missing_startup_requirements(&missing_connection).is_empty());

        let missing_path =
            Cli::try_parse_from(["synly", "--fs", "receive", "--host", "--no-sync-delete"])
                .unwrap();
        assert!(!missing_startup_requirements(&missing_path).is_empty());
    }

    #[test]
    fn both_and_auto_require_initial_choice() {
        let both = Cli::try_parse_from(["synly", "--fs", "both", ".", "--host"]).unwrap();
        let auto = Cli::try_parse_from(["synly", "--fs", "auto", ".", "--join"]).unwrap();

        let both_missing = missing_startup_requirements(&both).join("\n");
        let auto_missing = missing_startup_requirements(&auto).join("\n");

        assert!(both_missing.contains("--initial this"));
        assert!(auto_missing.contains("--initial other"));
    }

    #[test]
    fn initial_is_rejected_for_non_bidirectional_modes() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "send", "--initial", "this", ".", "--host"])
                .unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();
        assert!(err.contains("`--initial` 只能和 `--fs both` 或 `--fs auto` 一起使用"));
    }

    #[test]
    fn accepts_complete_headless_cli_without_interaction() {
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
    fn accepts_headless_file_off_mode_without_path() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "off", "--host", "--clipboard", "both"]).unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert!(matches!(options.connection, ConnectionPreference::Host));
        assert_eq!(options.file_sync_mode, FileSyncMode::Off);
        assert_eq!(options.clipboard_mode, ClipboardMode::Both);
        assert!(!options.workspace.file_sync_enabled());
    }

    #[test]
    fn omitted_fs_defaults_to_off() {
        let cli = Cli::try_parse_from(["synly", "--host", "--clipboard", "both"]).unwrap();

        assert!(missing_startup_requirements(&cli).is_empty());
        let options = collect_runtime_options(cli, &test_config()).unwrap();
        assert_eq!(options.file_sync_mode, FileSyncMode::Off);
        assert_eq!(options.clipboard_mode, ClipboardMode::Both);
        assert!(!options.workspace.file_sync_enabled());
    }

    #[test]
    fn headless_reports_missing_startup_requirements() {
        let cli = Cli::try_parse_from(["synly", "--fs", "receive", "--headless"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();

        assert!(err.contains("headless 参数不足"));
        assert!(err.contains("`--host` 或 `--join`"));
        assert!(err.contains("`--fs receive`"));
    }

    #[test]
    fn headless_join_requires_peer() {
        let cli =
            Cli::try_parse_from(["synly", "--fs", "send", ".", "--join", "--headless"]).unwrap();

        let err = collect_runtime_options(cli, &test_config())
            .unwrap_err()
            .to_string();

        assert!(err.contains("`--peer`"));
    }

    #[test]
    fn headless_host_requires_pin_unless_trusted_only() {
        let missing_pin = Cli::try_parse_from(["synly", "--host", "--headless"]).unwrap();
        let err = collect_runtime_options(missing_pin, &test_config())
            .unwrap_err()
            .to_string();
        assert!(err.contains("`--pin`"));

        let trusted_only = Cli::try_parse_from([
            "synly",
            "--host",
            "--headless",
            "--trusted-only",
        ])
        .unwrap();
        assert!(collect_runtime_options(trusted_only, &test_config()).is_ok());
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

    fn assert_global_receive_cli(cli: Cli) {
        assert!(cli.join);
        assert!(!cli.host);
        assert!(cli.no_sync_delete);
        assert!(!cli.sync_delete);
        assert_eq!(cli.fs, Some(FileSyncMode::Receive));
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
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: crate::config::UiConfig::default(),
            runtime: crate::config::RuntimeConfig::default(),
            trusted_devices: Vec::new(),
        }
    }
}
