use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "synly",
    version,
    about = "在局域网中发现设备, 通过 PIN 配对, 建立安全连接并持续同步数据"
)]
pub struct Cli {
    #[command(subcommand)]
    pub internal_command: Option<InternalCommand>,
    #[arg(long, help = "以无界面模式运行, 会话参数从配置文件读取")]
    pub headless: bool,
}

#[derive(Subcommand, Debug)]
pub enum InternalCommand {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_cli_only_accepts_headless_mode() {
        assert!(!Cli::try_parse_from(["synly"]).unwrap().headless);
        assert!(Cli::try_parse_from(["synly", "--headless"]).unwrap().headless);
        assert!(Cli::try_parse_from(["synly", "--host"]).is_err());
        assert!(Cli::try_parse_from(["synly", "--peer", "demo"]).is_err());
    }

    #[test]
    fn internal_input_agent_command_parses() {
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

        assert!(matches!(
            cli.internal_command,
            Some(InternalCommand::InputAgent {
                parent_pid: 42,
                ..
            })
        ));
    }
}
