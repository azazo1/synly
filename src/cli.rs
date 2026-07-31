use crate::config::RuntimeConfig;
use crate::settings::ConnectionPreference;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备, 通过 PIN 配对, 建立安全连接并持续同步数据"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(
        long,
        global = true,
        help = "以无界面模式运行, 会话参数从配置文件读取"
    )]
    pub headless: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 以 host 角色监听, 等待对端连接
    #[command(alias = "listen")]
    Host,
    /// 以 join 角色连接对端, peer 缺省时使用配置中的 peer_query
    #[command(alias = "connect")]
    Join {
        /// 对端, 可为实例名, 设备名, 设备 ID 前缀, IPv4 地址或 IPv4:端口
        peer: Option<String>,
    },
    #[command(name = "__input-agent", hide = true)]
    InputAgent {
        #[arg(long)]
        command_pipe: String,
        #[arg(long)]
        event_pipe: String,
        #[arg(long)]
        token: String,
        #[arg(long)]
        parent_pid: u32,
    },
}

#[derive(Clone, Debug)]
pub struct SessionCli {
    pub connection: ConnectionPreference,
    pub peer_query: Option<String>,
}

impl Cli {
    /// 将会话子命令映射为本次启动的内存覆盖参数, 内部命令或无子命令时返回 None.
    pub fn session_override(&self) -> Option<SessionCli> {
        match &self.command {
            Some(Command::Host) => Some(SessionCli {
                connection: ConnectionPreference::Host,
                peer_query: None,
            }),
            Some(Command::Join { peer }) => Some(SessionCli {
                connection: ConnectionPreference::Join,
                peer_query: peer.clone(),
            }),
            _ => None,
        }
    }
}

impl SessionCli {
    /// 只修改内存中的运行配置, 不触发任何配置持久化.
    ///
    /// headless 模式无法进行交互配对, 因此同时强制 trusted_only = true.
    pub fn apply_to(&self, runtime: &mut RuntimeConfig, headless: bool) {
        runtime.connection = Some(self.connection);
        if let Some(peer) = self
            .peer_query
            .as_deref()
            .map(str::trim)
            .filter(|peer| !peer.is_empty())
        {
            runtime.peer_query = peer.to_string();
        }
        if headless {
            runtime.trusted_only = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    #[test]
    fn host_and_listen_alias_parse() {
        let cli = Cli::try_parse_from(["synly", "host"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Host)));
        assert!(!cli.headless);

        let cli = Cli::try_parse_from(["synly", "listen"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Host)));
    }

    #[test]
    fn join_and_connect_alias_parse_with_optional_peer() {
        let cli = Cli::try_parse_from(["synly", "join", "demo-device"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Command::Join { peer: Some(peer) }) if peer == "demo-device"
        ));

        let cli = Cli::try_parse_from(["synly", "connect"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Join { peer: None })));
    }

    #[test]
    fn headless_combines_before_and_after_subcommand() {
        let cli = Cli::try_parse_from(["synly", "--headless", "host"]).unwrap();
        assert!(cli.headless);
        assert!(matches!(cli.command, Some(Command::Host)));

        let cli = Cli::try_parse_from(["synly", "host", "--headless"]).unwrap();
        assert!(cli.headless);
        assert!(matches!(cli.command, Some(Command::Host)));
    }

    #[test]
    fn plain_headless_keeps_no_session_command() {
        let cli = Cli::try_parse_from(["synly", "--headless"]).unwrap();
        assert!(cli.headless);
        assert!(cli.command.is_none());
        assert!(cli.session_override().is_none());
    }

    #[test]
    fn session_override_maps_role_and_peer() {
        assert!(
            Cli::try_parse_from(["synly"])
                .unwrap()
                .session_override()
                .is_none()
        );

        let host = Cli::try_parse_from(["synly", "host"])
            .unwrap()
            .session_override()
            .expect("host should map to session override");
        assert_eq!(host.connection, ConnectionPreference::Host);
        assert_eq!(host.peer_query, None);

        let join = Cli::try_parse_from(["synly", "join", "worker-a"])
            .unwrap()
            .session_override()
            .expect("join should map to session override");
        assert_eq!(join.connection, ConnectionPreference::Join);
        assert_eq!(join.peer_query.as_deref(), Some("worker-a"));
    }

    #[test]
    fn session_override_ignores_internal_command() {
        let cli = Cli::try_parse_from([
            "synly",
            "__input-agent",
            "--command-pipe",
            r"\\.\pipe\synly-input-command-test",
            "--event-pipe",
            r"\\.\pipe\synly-input-event-test",
            "--token",
            "test-token",
            "--parent-pid",
            "42",
        ])
        .unwrap();

        assert!(cli.session_override().is_none());
        assert!(matches!(
            cli.command,
            Some(Command::InputAgent { parent_pid: 42, .. })
        ));
    }

    #[test]
    fn apply_to_overrides_role_peer_and_headless_trusted_only() {
        let mut runtime = RuntimeConfig {
            peer_query: "old-peer".to_string(),
            ..RuntimeConfig::default()
        };

        SessionCli {
            connection: ConnectionPreference::Join,
            peer_query: Some(" new-peer ".to_string()),
        }
        .apply_to(&mut runtime, true);

        assert_eq!(runtime.connection, Some(ConnectionPreference::Join));
        assert_eq!(runtime.peer_query, "new-peer");
        assert!(runtime.trusted_only);
    }

    #[test]
    fn apply_to_keeps_trusted_policy_without_headless() {
        let mut runtime = RuntimeConfig::default();

        SessionCli {
            connection: ConnectionPreference::Host,
            peer_query: None,
        }
        .apply_to(&mut runtime, false);

        assert_eq!(runtime.connection, Some(ConnectionPreference::Host));
        assert!(!runtime.trusted_only);
    }

    #[test]
    fn apply_to_empty_peer_falls_back_to_config() {
        let mut runtime = RuntimeConfig {
            peer_query: "old-peer".to_string(),
            ..RuntimeConfig::default()
        };

        SessionCli {
            connection: ConnectionPreference::Join,
            peer_query: Some("   ".to_string()),
        }
        .apply_to(&mut runtime, false);

        assert_eq!(runtime.peer_query, "old-peer");
    }
}
