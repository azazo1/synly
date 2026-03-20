use crate::config::DeviceConfig;
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use console::{Term, style};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备、通过 PIN 配对、建立安全连接并持续同步文件"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(long, conflicts_with = "join")]
    pub host: bool,
    #[arg(long, conflicts_with = "host")]
    pub join: bool,
    #[arg(long, default_value_t = 3)]
    pub interval_secs: u64,
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

#[derive(Clone, Copy, Debug)]
pub enum ConnectionPreference {
    Host,
    Join,
}

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub mode: SyncMode,
    pub connection: ConnectionPreference,
    pub workspace: WorkspaceSpec,
    pub interval_secs: u64,
}

pub fn collect_runtime_options(cli: Cli, device: &DeviceConfig) -> Result<RuntimeOptions> {
    let connection = if cli.host {
        ConnectionPreference::Host
    } else if cli.join {
        ConnectionPreference::Join
    } else {
        choose_connection()?
    };

    let mode = match &cli.command {
        Some(Command::Send { .. }) => SyncMode::Send,
        Some(Command::Receive { .. }) => SyncMode::Receive,
        Some(Command::Both { .. }) => SyncMode::Both,
        Some(Command::Auto { .. }) => SyncMode::Auto,
        None => choose_mode(device, connection)?,
    };

    let workspace = match cli.command {
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
    };

    Ok(RuntimeOptions {
        mode,
        connection,
        workspace,
        interval_secs: cli.interval_secs.max(1),
    })
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
        if !value.is_empty() {
            return Ok(value);
        }
        term.write_line("输入不能为空，请重新输入。")?;
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

fn choose_mode(device: &DeviceConfig, connection: ConnectionPreference) -> Result<SyncMode> {
    let options = match connection {
        ConnectionPreference::Host => vec![
            "自动协商: 监听时根据客户端请求决定方向，使用同一个目录收发 (Recommended)".to_string(),
            "发送方: 把本地文件同步给对方".to_string(),
            "接收方: 接收对方同步过来的文件".to_string(),
            "双向同步: 两边都能发送和接收".to_string(),
        ],
        ConnectionPreference::Join => vec![
            "发送方: 把本地文件同步给对方".to_string(),
            "接收方: 接收对方同步过来的文件".to_string(),
            "双向同步: 两边都能发送和接收".to_string(),
            "自动协商: 使用同一个目录收发，并尽量根据对端能力协商".to_string(),
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

fn expand_path_string(raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("路径不能为空");
    }

    let with_env = expand_env_vars(trimmed)?;
    let expanded = expand_tilde(&with_env)?;
    Ok(PathBuf::from(expanded))
}

fn expand_tilde(raw: &str) -> Result<String> {
    if !raw.starts_with('~') {
        return Ok(raw.to_string());
    }

    let rest = &raw[1..];
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with('\\') {
        return Ok(raw.to_string());
    }

    let home = home_dir().context("无法展开 `~`，因为当前环境没有可用的 home 目录")?;
    Ok(format!("{}{}", home, rest))
}

fn home_dir() -> Option<String> {
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE")
            && !profile.trim().is_empty()
        {
            return Some(profile);
        }

        let drive = env::var("HOMEDRIVE").ok()?;
        let path = env::var("HOMEPATH").ok()?;
        if !drive.trim().is_empty() && !path.trim().is_empty() {
            return Some(format!("{drive}{path}"));
        }
    }

    env::var("HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn expand_env_vars(raw: &str) -> Result<String> {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    let mut output = String::with_capacity(raw.len());

    while index < chars.len() {
        match chars[index] {
            '$' => {
                if index + 1 < chars.len() && chars[index + 1] == '{' {
                    let mut end = index + 2;
                    while end < chars.len() && chars[end] != '}' {
                        end += 1;
                    }
                    if end >= chars.len() {
                        bail!("环境变量表达式缺少 `}}`: {}", raw);
                    }
                    let name = chars[index + 2..end].iter().collect::<String>();
                    output.push_str(&resolve_env_var(&name)?);
                    index = end + 1;
                    continue;
                }

                let mut end = index + 1;
                while end < chars.len() && is_env_name_char(chars[end], end == index + 1) {
                    end += 1;
                }

                if end == index + 1 {
                    output.push('$');
                    index += 1;
                    continue;
                }

                let name = chars[index + 1..end].iter().collect::<String>();
                output.push_str(&resolve_env_var(&name)?);
                index = end;
            }
            '%' => {
                let mut end = index + 1;
                while end < chars.len() && chars[end] != '%' {
                    end += 1;
                }

                if end >= chars.len() {
                    output.push('%');
                    index += 1;
                    continue;
                }

                let name = chars[index + 1..end].iter().collect::<String>();
                if name.is_empty() {
                    output.push('%');
                    index += 1;
                    continue;
                }

                output.push_str(&resolve_env_var(&name)?);
                index = end + 1;
            }
            ch => {
                output.push(ch);
                index += 1;
            }
        }
    }

    Ok(output)
}

fn resolve_env_var(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("环境变量 `{name}` 未定义"))
}

fn is_env_name_char(ch: char, first: bool) -> bool {
    if first {
        ch == '_' || ch.is_ascii_alphabetic()
    } else {
        ch == '_' || ch.is_ascii_alphanumeric()
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_env_vars, expand_path_string, expand_tilde};
    use std::env;

    #[test]
    fn expands_shell_style_env_var() {
        let path = expand_env_vars("$PATH").unwrap();
        assert_eq!(path, env::var("PATH").unwrap());
    }

    #[test]
    fn expands_braced_env_var() {
        let path = expand_env_vars("${PATH}/bin").unwrap();
        assert_eq!(path, format!("{}/bin", env::var("PATH").unwrap()));
    }

    #[test]
    fn expands_percent_env_var_when_closed() {
        let path = expand_env_vars("%PATH%/bin").unwrap();
        assert_eq!(path, format!("{}/bin", env::var("PATH").unwrap()));
    }

    #[test]
    fn expands_tilde_prefix() {
        let home = env::var("HOME")
            .or_else(|_| env::var("USERPROFILE"))
            .expect("home-like env var should exist during tests");
        let path = expand_tilde("~/demo").unwrap();
        assert_eq!(path, format!("{home}/demo"));
    }

    #[test]
    fn expands_combined_path() {
        let home = env::var("HOME")
            .or_else(|_| env::var("USERPROFILE"))
            .expect("home-like env var should exist during tests");
        let path = expand_path_string("~/$PATH").unwrap();
        assert_eq!(
            path,
            std::path::PathBuf::from(format!("{home}/{}", env::var("PATH").unwrap()))
        );
    }
}
