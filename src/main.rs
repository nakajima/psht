mod app_name;
mod app_state;
mod commands;
mod container;
mod control_plane;
mod deploy_log;
mod deploy_state;
mod detect;
mod git;
mod reconcile_command;
mod reconcile_runtime;
mod runtime_graph;
mod sqlite_store;
mod stats;
mod tailscale;

#[cfg(all(test, feature = "integration"))]
mod integration_tests;

use std::env;
use std::process;

use clap::{Parser, Subcommand};

const WEB_DEFAULT_BIND: &str = "127.0.0.1";
const WEB_DEFAULT_PORT: u16 = 8787;

#[derive(Parser)]
#[command(
    name = "psht-server",
    about = "psht server commands",
    version = concat!(env!("CARGO_PKG_VERSION"), " (server)"),
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Subcommand)]
enum Command {
    /// Print setup script for local CLI install
    Setup,
    /// Update local CLI
    #[command(name = "update-cli")]
    UpdateCli,
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
    /// Restart an app
    Restart { app: String },
    /// Stop and remove an app
    Destroy {
        /// Preserve the app's /storage volume
        #[arg(long)]
        keep_storage: bool,
        app: String,
    },
    /// Set up this server as a psht host
    Bootstrap,
    /// Upgrade psht server, incus, and stacks
    Upgrade,
    /// Check server health
    Doctor,
    /// Check deployed app health
    Health,
    /// Manage the web UI
    Web {
        /// Optional action: serve, start, or stop
        #[arg(value_name = "ACTION", value_parser = ["serve", "start", "stop"])]
        action: Option<String>,
        /// Bind address for the web UI serve/start commands
        #[arg(long)]
        bind: Option<String>,
        /// Listen port for the web UI serve/start commands
        #[arg(long)]
        port: Option<u16>,
    },
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    Supervise,
    /// Manage host tailscale
    Tailscale {
        #[command(subcommand)]
        command: TailscaleCommand,
    },
    #[command(name = "init-stacks", hide = true)]
    InitStacks,
    #[command(name = "print-cli", hide = true)]
    PrintCli,
    #[command(hide = true)]
    Deploy {
        app: String,
        #[arg(long = "ref")]
        ref_name: Option<String>,
        #[arg(long)]
        sha: Option<String>,
        #[arg(short = 'f', long)]
        force: bool,
    },
    #[command(hide = true)]
    Push {
        app: String,
        #[arg(short = 'f', long)]
        force: bool,
    },
    #[command(name = "git-receive-pack", hide = true)]
    GitReceivePack { app: String },
    #[command(name = "git-upload-pack", hide = true)]
    GitUploadPack { app: String },
    #[command(hide = true)]
    Env {
        app: String,
        assignments: Vec<String>,
    },
    #[command(name = "env-unset", hide = true)]
    EnvUnset { app: String, names: Vec<String> },
    #[command(hide = true)]
    Cleanup {
        #[command(subcommand)]
        command: CleanupCommand,
    },
    #[command(name = "debug-resources", hide = true)]
    DebugResources {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        candidate: Option<String>,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
enum TailscaleCommand {
    /// Show tailscale status
    Status { app: String },
    /// Bring tailscale up with SSH enabled
    Up { app: String },
    /// Bring tailscale down
    Down { app: String },
}

#[derive(Debug, PartialEq, Subcommand)]
enum CleanupCommand {
    Previous { app: String },
}

fn strip_git_suffix(name: &str) -> String {
    name.strip_suffix(".git").unwrap_or(name).to_string()
}

fn cli_from_env() -> Result<Cli, String> {
    let args: Vec<String> = env::args().collect();

    // SSH login shell: psht-server -c "command args"
    if args.len() == 3 && args[1] == "-c" {
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(
            shell_words::split(&args[2]).map_err(|e| format!("failed to parse command: {e}"))?,
        );
        return Ok(Cli::parse_from(synthetic));
    }

    // SSH_ORIGINAL_COMMAND
    if args.len() == 1 {
        if let Ok(ssh_cmd) = env::var("SSH_ORIGINAL_COMMAND") {
            let mut synthetic = vec!["psht-server".to_string()];
            synthetic.extend(
                shell_words::split(&ssh_cmd)
                    .map_err(|e| format!("failed to parse command: {e}"))?,
            );
            return Ok(Cli::parse_from(synthetic));
        }
    }

    // Direct invocation
    Ok(Cli::parse_from(&args))
}

fn run() -> Result<(), String> {
    let cli = cli_from_env()?;

    match cli.command {
        Command::GitReceivePack { app } => {
            let app = strip_git_suffix(&app);
            app_name::validate_app_name(&app)?;
            git::handle_receive_pack(&app)
        }
        Command::GitUploadPack { app } => {
            let app = strip_git_suffix(&app);
            app_name::validate_app_name(&app)?;
            git::handle_upload_pack(&app)
        }
        Command::Deploy {
            app,
            ref_name,
            sha,
            force,
        } => {
            app_name::validate_app_name(&app)?;
            commands::deploy(&app, ref_name.as_deref(), sha.as_deref(), force)
        }
        Command::Push { app, force } => {
            app_name::validate_app_name(&app)?;
            commands::push(&app, force)
        }
        Command::Env { app, assignments } => {
            app_name::validate_app_name(&app)?;
            commands::env_command(&app, &assignments)
        }
        Command::EnvUnset { app, names } => {
            app_name::validate_app_name(&app)?;
            commands::env_unset(&app, &names)
        }
        Command::Cleanup { command } => match command {
            CleanupCommand::Previous { app } => {
                app_name::validate_app_name(&app)?;
                commands::cleanup_previous(&app)
            }
        },
        Command::Ps => commands::ps(),
        Command::Logs { app, follow } => {
            app_name::validate_app_name(&app)?;
            commands::logs(&app, follow)
        }
        Command::Stop { app } => {
            app_name::validate_app_name(&app)?;
            commands::stop(&app)
        }
        Command::Start { app } => {
            app_name::validate_app_name(&app)?;
            commands::start(&app)
        }
        Command::Restart { app } => {
            app_name::validate_app_name(&app)?;
            commands::restart(&app)
        }
        Command::Destroy { app, keep_storage } => {
            app_name::validate_app_name(&app)?;
            commands::destroy_with_options(&app, commands::DestroyOptions { keep_storage })
        }
        Command::Setup => commands::setup(),
        Command::UpdateCli => commands::update(),
        Command::Bootstrap => commands::bootstrap(),
        Command::Upgrade => commands::upgrade_server(),
        Command::Doctor => commands::doctor(),
        Command::Health => commands::health(),
        Command::Web { action, bind, port } => match action.as_deref() {
            None | Some("serve") => {
                let bind = bind.as_deref().unwrap_or(WEB_DEFAULT_BIND);
                let port = port.unwrap_or(WEB_DEFAULT_PORT);
                commands::web(bind, port)
            }
            Some("start") => {
                let bind = bind.as_deref().unwrap_or(WEB_DEFAULT_BIND);
                let port = port.unwrap_or(WEB_DEFAULT_PORT);
                commands::web_start(bind, port)
            }
            Some("stop") => {
                if bind.is_some() || port.is_some() {
                    return Err(
                        "`psht-server web stop` does not accept `--bind` or `--port`".to_string(),
                    );
                }
                commands::web_stop()
            }
            Some(other) => Err(format!(
                "unknown web action '{other}'; expected one of: serve, start, stop"
            )),
        },
        Command::Daemon => commands::daemon(),
        Command::DebugResources { app, candidate } => {
            if let Some(app) = app.as_deref() {
                app_name::validate_app_name(app)?;
            }
            commands::debug_resources(app.as_deref(), candidate.as_deref())
        }
        Command::Supervise => commands::supervise(),
        Command::Tailscale { command } => match command {
            TailscaleCommand::Status { app } => {
                app_name::validate_app_name(&app)?;
                commands::tailscale_status(&app)
            }
            TailscaleCommand::Up { app } => {
                app_name::validate_app_name(&app)?;
                commands::tailscale_up(&app)
            }
            TailscaleCommand::Down { app } => {
                app_name::validate_app_name(&app)?;
                commands::tailscale_down(&app)
            }
        },
        Command::InitStacks => commands::init_stacks(),
        Command::PrintCli => commands::print_cli(),
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
        let cli = parse_cli(&["psht-server", "git-receive-pack", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::GitReceivePack {
                app: "myapp".to_string()
            }
        );
    }

    #[test]
    fn parse_git_receive_pack_with_git_suffix() {
        let cli = parse_cli(&["psht-server", "git-receive-pack", "myapp.git"]).unwrap();
        assert_eq!(
            cli.command,
            Command::GitReceivePack {
                app: "myapp.git".to_string()
            }
        );
    }

    #[test]
    fn strip_git_suffix_works() {
        assert_eq!(strip_git_suffix("myapp.git"), "myapp");
        assert_eq!(strip_git_suffix("myapp"), "myapp");
    }

    #[test]
    fn parse_git_upload_pack() {
        let cli = parse_cli(&["psht-server", "git-upload-pack", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::GitUploadPack {
                app: "myapp".to_string()
            }
        );
    }

    #[test]
    fn parse_deploy() {
        let cli = parse_cli(&["psht-server", "deploy", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Deploy {
                app: "myapp".to_string(),
                ref_name: None,
                sha: None,
                force: false,
            }
        );
    }

    #[test]
    fn parse_deploy_with_ref_and_sha() {
        let cli = parse_cli(&[
            "psht-server",
            "deploy",
            "myapp",
            "--ref",
            "refs/heads/main",
            "--sha",
            "deadbeef",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Deploy {
                app: "myapp".to_string(),
                ref_name: Some("refs/heads/main".to_string()),
                sha: Some("deadbeef".to_string()),
                force: false,
            }
        );
    }

    #[test]
    fn parse_deploy_with_force() {
        let cli = parse_cli(&[
            "psht-server",
            "deploy",
            "myapp",
            "--ref",
            "refs/heads/main",
            "--sha",
            "deadbeef",
            "--force",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Deploy {
                app: "myapp".to_string(),
                ref_name: Some("refs/heads/main".to_string()),
                sha: Some("deadbeef".to_string()),
                force: true,
            }
        );
    }

    #[test]
    fn parse_deploy_with_short_force() {
        let cli = parse_cli(&[
            "psht-server",
            "deploy",
            "myapp",
            "--ref",
            "refs/heads/main",
            "--sha",
            "deadbeef",
            "-f",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Deploy {
                app: "myapp".to_string(),
                ref_name: Some("refs/heads/main".to_string()),
                sha: Some("deadbeef".to_string()),
                force: true,
            }
        );
    }

    #[test]
    fn parse_ps() {
        let cli = parse_cli(&["psht-server", "ps"]).unwrap();
        assert_eq!(cli.command, Command::Ps);
    }

    #[test]
    fn parse_logs() {
        let cli = parse_cli(&["psht-server", "logs", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Logs {
                app: "myapp".to_string(),
                follow: false,
            }
        );
    }

    #[test]
    fn parse_logs_follow() {
        let cli = parse_cli(&["psht-server", "logs", "-f", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Logs {
                app: "myapp".to_string(),
                follow: true,
            }
        );
    }

    #[test]
    fn parse_stop() {
        let cli = parse_cli(&["psht-server", "stop", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Stop {
                app: "myapp".to_string()
            }
        );
    }

    #[test]
    fn parse_start() {
        let cli = parse_cli(&["psht-server", "start", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Start {
                app: "myapp".to_string()
            }
        );
    }

    #[test]
    fn parse_restart() {
        let cli = parse_cli(&["psht-server", "restart", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Restart {
                app: "myapp".to_string()
            }
        );
    }

    #[test]
    fn parse_destroy() {
        let cli = parse_cli(&["psht-server", "destroy", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Destroy {
                app: "myapp".to_string(),
                keep_storage: false,
            }
        );
    }

    #[test]
    fn parse_destroy_keep_storage() {
        let cli = parse_cli(&["psht-server", "destroy", "--keep-storage", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Destroy {
                app: "myapp".to_string(),
                keep_storage: true,
            }
        );
    }

    #[test]
    fn parse_push() {
        let cli = parse_cli(&["psht-server", "push", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Push {
                app: "myapp".to_string(),
                force: false,
            }
        );
    }

    #[test]
    fn parse_push_force() {
        let cli = parse_cli(&["psht-server", "push", "--force", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Push {
                app: "myapp".to_string(),
                force: true,
            }
        );
    }

    #[test]
    fn parse_push_short_force() {
        let cli = parse_cli(&["psht-server", "push", "-f", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Push {
                app: "myapp".to_string(),
                force: true,
            }
        );
    }

    #[test]
    fn parse_env() {
        let cli = parse_cli(&["psht-server", "env", "myapp", "A=1", "B=two"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Env {
                app: "myapp".to_string(),
                assignments: vec!["A=1".to_string(), "B=two".to_string()],
            }
        );
    }

    #[test]
    fn parse_env_unset() {
        let cli = parse_cli(&["psht-server", "env-unset", "myapp", "A", "B"]).unwrap();
        assert_eq!(
            cli.command,
            Command::EnvUnset {
                app: "myapp".to_string(),
                names: vec!["A".to_string(), "B".to_string()],
            }
        );
    }

    #[test]
    fn parse_cleanup_previous() {
        let cli = parse_cli(&["psht-server", "cleanup", "previous", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Cleanup {
                command: CleanupCommand::Previous {
                    app: "myapp".to_string()
                }
            }
        );
    }

    #[test]
    fn parse_setup() {
        let cli = parse_cli(&["psht-server", "setup"]).unwrap();
        assert_eq!(cli.command, Command::Setup);
    }

    #[test]
    fn parse_update_cli() {
        let cli = parse_cli(&["psht-server", "update-cli"]).unwrap();
        assert_eq!(cli.command, Command::UpdateCli);
    }

    #[test]
    fn parse_init_stacks() {
        let cli = parse_cli(&["psht-server", "init-stacks"]).unwrap();
        assert_eq!(cli.command, Command::InitStacks);
    }

    #[test]
    fn parse_print_cli() {
        let cli = parse_cli(&["psht-server", "print-cli"]).unwrap();
        assert_eq!(cli.command, Command::PrintCli);
    }

    #[test]
    fn parse_bootstrap() {
        let cli = parse_cli(&["psht-server", "bootstrap"]).unwrap();
        assert_eq!(cli.command, Command::Bootstrap);
    }

    #[test]
    fn parse_upgrade() {
        let cli = parse_cli(&["psht-server", "upgrade"]).unwrap();
        assert_eq!(cli.command, Command::Upgrade);
    }

    #[test]
    fn parse_doctor() {
        let cli = parse_cli(&["psht-server", "doctor"]).unwrap();
        assert_eq!(cli.command, Command::Doctor);
    }

    #[test]
    fn parse_health() {
        let cli = parse_cli(&["psht-server", "health"]).unwrap();
        assert_eq!(cli.command, Command::Health);
    }

    #[test]
    fn parse_web() {
        let cli = parse_cli(&["psht-server", "web"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Web {
                action: None,
                bind: None,
                port: None,
            }
        );
    }

    #[test]
    fn parse_web_serve_with_flags() {
        let cli = parse_cli(&[
            "psht-server",
            "web",
            "serve",
            "--bind",
            "0.0.0.0",
            "--port",
            "9999",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Web {
                action: Some("serve".to_string()),
                bind: Some("0.0.0.0".to_string()),
                port: Some(9999),
            }
        );
    }

    #[test]
    fn parse_web_start_with_flags() {
        let cli = parse_cli(&[
            "psht-server",
            "web",
            "start",
            "--bind",
            "100.64.0.1",
            "--port",
            "8788",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Web {
                action: Some("start".to_string()),
                bind: Some("100.64.0.1".to_string()),
                port: Some(8788),
            }
        );
    }

    #[test]
    fn parse_web_stop() {
        let cli = parse_cli(&["psht-server", "web", "stop"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Web {
                action: Some("stop".to_string()),
                bind: None,
                port: None,
            }
        );
    }

    #[test]
    fn parse_daemon() {
        let cli = parse_cli(&["psht-server", "daemon"]).unwrap();
        assert_eq!(cli.command, Command::Daemon);
    }

    #[test]
    fn parse_supervise() {
        let cli = parse_cli(&["psht-server", "supervise"]).unwrap();
        assert_eq!(cli.command, Command::Supervise);
    }

    #[test]
    fn parse_debug_resources() {
        let cli = parse_cli(&[
            "psht-server",
            "debug-resources",
            "--app",
            "myapp",
            "--candidate",
            "psht-myapp-build-123",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::DebugResources {
                app: Some("myapp".to_string()),
                candidate: Some("psht-myapp-build-123".to_string()),
            }
        );
    }

    #[test]
    fn parse_tailscale_status() {
        let cli = parse_cli(&["psht-server", "tailscale", "status", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Tailscale {
                command: TailscaleCommand::Status {
                    app: "myapp".to_string()
                }
            }
        );
    }

    #[test]
    fn parse_tailscale_up() {
        let cli = parse_cli(&["psht-server", "tailscale", "up", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Tailscale {
                command: TailscaleCommand::Up {
                    app: "myapp".to_string()
                }
            }
        );
    }

    #[test]
    fn parse_tailscale_down() {
        let cli = parse_cli(&["psht-server", "tailscale", "down", "myapp"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Tailscale {
                command: TailscaleCommand::Down {
                    app: "myapp".to_string()
                }
            }
        );
    }

    #[test]
    fn parse_unknown_command() {
        let result = parse_cli(&["psht-server", "wat"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_shell_dash_c_ps() {
        // Simulates: psht-server -c "ps" (shell_words splits "ps" into ["ps"])
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(shell_words::split("ps").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(cli.command, Command::Ps);
    }

    #[test]
    fn parse_shell_dash_c_with_arg() {
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(shell_words::split("logs myapp").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(
            cli.command,
            Command::Logs {
                app: "myapp".to_string(),
                follow: false,
            }
        );
    }

    #[test]
    fn parse_shell_dash_c_git_receive() {
        // SSH sends: git-receive-pack 'myapp.git'
        // shell_words strips the quotes, clap parses the subcommand
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(shell_words::split("git-receive-pack 'myapp.git'").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(
            cli.command,
            Command::GitReceivePack {
                app: "myapp.git".to_string()
            }
        );
        // strip_git_suffix is applied at dispatch time
        if let Command::GitReceivePack { app } = cli.command {
            assert_eq!(strip_git_suffix(&app), "myapp");
        } else {
            panic!("expected git-receive-pack command");
        }
    }

    #[test]
    fn parse_shell_dash_c_setup() {
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(shell_words::split("setup").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(cli.command, Command::Setup);
    }

    #[test]
    fn parse_shell_dash_c_update_cli() {
        let mut synthetic = vec!["psht-server".to_string()];
        synthetic.extend(shell_words::split("update-cli").unwrap());
        let cli = Cli::try_parse_from(synthetic).unwrap();
        assert_eq!(cli.command, Command::UpdateCli);
    }

    #[test]
    fn parse_no_subcommand_is_error() {
        let cli = parse_cli(&["psht-server"]);
        assert!(cli.is_err());
    }
}
