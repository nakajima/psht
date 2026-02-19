mod commands;
mod container;
mod detect;
mod git;
mod tailscale;

#[cfg(all(test, feature = "integration"))]
mod integration_tests;

use std::env;
use std::process;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "psht", about = "deploy apps with git push")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, PartialEq, Subcommand)]
enum Command {
    /// Set up project and install CLI
    Setup,
    /// Update the CLI
    Update,
    /// List running apps
    Ps,
    /// Show app logs
    Logs {
        app: String,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop an app
    Stop { app: String },
    /// Start a stopped app
    Start { app: String },
    /// Stop and remove an app
    Destroy { app: String },
    /// Set up this server as a psht host
    Bootstrap,
    #[command(name = "init-stacks", hide = true)]
    InitStacks,
    #[command(hide = true)]
    Deploy { app: String },
    #[command(hide = true)]
    Push { app: String },
    #[command(name = "git-receive-pack", hide = true)]
    GitReceivePack { app: String },
    #[command(name = "git-upload-pack", hide = true)]
    GitUploadPack { app: String },
}

fn strip_git_suffix(name: &str) -> String {
    name.strip_suffix(".git").unwrap_or(name).to_string()
}


fn cli_from_env() -> Result<Cli, String> {
    let args: Vec<String> = env::args().collect();

    // SSH login shell: psht -c "command args"
    if args.len() == 3 && args[1] == "-c" {
        let mut synthetic = vec!["psht".to_string()];
        synthetic.extend(
            shell_words::split(&args[2]).map_err(|e| format!("failed to parse command: {e}"))?,
        );
        return Cli::try_parse_from(synthetic).map_err(|e| e.to_string());
    }

    // SSH_ORIGINAL_COMMAND
    if args.len() == 1 {
        if let Ok(ssh_cmd) = env::var("SSH_ORIGINAL_COMMAND") {
            let mut synthetic = vec!["psht".to_string()];
            synthetic.extend(
                shell_words::split(&ssh_cmd)
                    .map_err(|e| format!("failed to parse command: {e}"))?,
            );
            return Cli::try_parse_from(synthetic).map_err(|e| e.to_string());
        }
    }

    // Direct invocation
    Cli::try_parse_from(&args).map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let cli = cli_from_env()?;

    let command = match cli.command {
        Some(cmd) => cmd,
        None => return commands::help(),
    };

    match command {
        Command::GitReceivePack { app } => git::handle_receive_pack(&strip_git_suffix(&app)),
        Command::GitUploadPack { app } => git::handle_upload_pack(&strip_git_suffix(&app)),
        Command::Deploy { app } => commands::deploy(&app),
        Command::Push { app } => commands::push(&app),
        Command::Ps => commands::ps(),
        Command::Logs { app, follow } => commands::logs(&app, follow),
        Command::Stop { app } => commands::stop(&app),
        Command::Start { app } => commands::start(&app),
        Command::Destroy { app } => commands::destroy(&app),
        Command::Setup => commands::setup(),
        Command::Update => commands::update(),
        Command::Bootstrap => commands::bootstrap(),
        Command::InitStacks => commands::init_stacks(),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cli(args: &[&str]) -> Result<Cli, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Cli::try_parse_from(args).map_err(|e| e.to_string())
    }

    #[test]
    fn parse_git_receive_pack() {
        let cli = parse_cli(&["psht", "git-receive-pack", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::GitReceivePack {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_git_receive_pack_with_git_suffix() {
        let cli = parse_cli(&["psht", "git-receive-pack", "myapp.git"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::GitReceivePack {
                app: "myapp.git".to_string()
            })
        );
    }

    #[test]
    fn strip_git_suffix_works() {
        assert_eq!(strip_git_suffix("myapp.git"), "myapp");
        assert_eq!(strip_git_suffix("myapp"), "myapp");
    }

    #[test]
    fn parse_git_upload_pack() {
        let cli = parse_cli(&["psht", "git-upload-pack", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::GitUploadPack {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_deploy() {
        let cli = parse_cli(&["psht", "deploy", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Deploy {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_ps() {
        let cli = parse_cli(&["psht", "ps"]).unwrap();
        assert_eq!(cli.command, Some(Command::Ps));
    }

    #[test]
    fn parse_logs() {
        let cli = parse_cli(&["psht", "logs", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                app: "myapp".to_string(),
                follow: false,
            })
        );
    }

    #[test]
    fn parse_logs_follow() {
        let cli = parse_cli(&["psht", "logs", "-f", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                app: "myapp".to_string(),
                follow: true,
            })
        );
    }

    #[test]
    fn parse_stop() {
        let cli = parse_cli(&["psht", "stop", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Stop {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_start() {
        let cli = parse_cli(&["psht", "start", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Start {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_destroy() {
        let cli = parse_cli(&["psht", "destroy", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Destroy {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_push() {
        let cli = parse_cli(&["psht", "push", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Push {
                app: "myapp".to_string()
            })
        );
    }

    #[test]
    fn parse_setup() {
        let cli = parse_cli(&["psht", "setup"]).unwrap();
        assert_eq!(cli.command, Some(Command::Setup));
    }

    #[test]
    fn parse_init_stacks() {
        let cli = parse_cli(&["psht", "init-stacks"]).unwrap();
        assert_eq!(cli.command, Some(Command::InitStacks));
    }

    #[test]
    fn parse_bootstrap() {
        let cli = parse_cli(&["psht", "bootstrap"]).unwrap();
        assert_eq!(cli.command, Some(Command::Bootstrap));
    }

    #[test]
    fn parse_unknown_command() {
        let result = parse_cli(&["psht", "wat"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_shell_dash_c_ps() {
        // Simulates: psht -c "ps" — shell_words splits "ps" into ["ps"]
        let mut synthetic = vec!["psht".to_string()];
        synthetic.extend(shell_words::split("ps").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(cli.command, Some(Command::Ps));
    }

    #[test]
    fn parse_shell_dash_c_with_arg() {
        let mut synthetic = vec!["psht".to_string()];
        synthetic.extend(shell_words::split("logs myapp").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                app: "myapp".to_string(),
                follow: false,
            })
        );
    }

    #[test]
    fn parse_shell_dash_c_git_receive() {
        // SSH sends: git-receive-pack 'myapp.git'
        // shell_words strips the quotes, clap parses the subcommand
        let mut synthetic = vec!["psht".to_string()];
        synthetic.extend(shell_words::split("git-receive-pack 'myapp.git'").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::GitReceivePack {
                app: "myapp.git".to_string()
            })
        );
        // strip_git_suffix is applied at dispatch time
        if let Some(Command::GitReceivePack { app }) = cli.command {
            assert_eq!(strip_git_suffix(&app), "myapp");
        }
    }

    #[test]
    fn parse_shell_dash_c_setup() {
        let mut synthetic = vec!["psht".to_string()];
        synthetic.extend(shell_words::split("setup").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(cli.command, Some(Command::Setup));
    }

    #[test]
    fn parse_no_subcommand_prints_help() {
        let cli = parse_cli(&["psht"]).unwrap();
        assert!(cli.command.is_none());
    }
}
