use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[path = "../app_name.rs"]
mod app_name;

#[derive(Parser)]
#[command(
    name = "psht",
    about = "deploy apps with psht",
    version = concat!(env!("CARGO_PKG_VERSION"), " (cli)")
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Deploy the current directory (Caddy routing is experimental)
    Deploy {
        /// App name or path to a binary (defaults to current directory name)
        app: Option<String>,
    },
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
    /// Stop and remove an app (Caddy routing cleanup is experimental)
    Destroy { app: String },
    /// Set up project for deployment
    Setup,
    /// Update the psht CLI
    Update,
    #[command(name = "__is-cli", hide = true)]
    IsCli,
}

#[derive(Deserialize, Serialize, Default)]
struct Config {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    projects: HashMap<String, String>,
}

fn config_path_at(home: &Path) -> PathBuf {
    home.join(".psht").join("config.toml")
}

fn load_config_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn resolve_host_from(config: &Config, cwd: &str) -> Result<String, String> {
    if let Some(host) = config.projects.get(cwd) {
        return Ok(host.clone());
    }
    config
        .host
        .clone()
        .ok_or_else(|| "no host configured. Run: ssh psht@<host> setup | sh".to_string())
}

fn app_name(explicit: Option<&str>, cwd: &Path) -> String {
    match explicit {
        Some(name) => name.to_string(),
        None => cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".to_string()),
    }
}

fn ssh_cmd(host: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if !status.success() {
        return Err(format!("ssh exited with status {}", status));
    }
    Ok(())
}

fn deploy_from_dir(
    host: &str,
    app: &str,
    source_dir: &Path,
    exclude_git: bool,
) -> Result<(), String> {
    let mut tar_cmd = Command::new("tar");
    if exclude_git {
        tar_cmd.args(["cz", "--exclude=.git", "."]);
    } else {
        tar_cmd.args(["cz", "."]);
    }
    let mut tar = tar_cmd
        .current_dir(source_dir)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run tar: {e}"))?;
    let tar_stdout = tar
        .stdout
        .take()
        .ok_or_else(|| "failed to capture tar stdout".to_string())?;

    let status = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .args(["push", app])
        .stdin(tar_stdout)
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    let tar_status = tar
        .wait()
        .map_err(|e| format!("failed to wait for tar: {e}"))?;

    if !status.success() {
        return Err(format!("deploy failed with status {}", status));
    }
    if !tar_status.success() {
        return Err(format!("tar failed with status {}", tar_status));
    }
    Ok(())
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.starts_with('.')
}

fn resolve_binary_path(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn stage_binary_dir(binary_path: &Path) -> Result<PathBuf, String> {
    let file_name = binary_path
        .file_name()
        .ok_or_else(|| format!("invalid binary path: {}", binary_path.display()))?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging = env::temp_dir().join(format!("psht-bin-{}-{ts}", process::id()));
    fs::create_dir_all(&staging)
        .map_err(|e| format!("failed to create staging dir {}: {e}", staging.display()))?;

    let staged_bin = staging.join(file_name);
    fs::copy(binary_path, &staged_bin).map_err(|e| {
        format!(
            "failed to copy {} to {}: {e}",
            binary_path.display(),
            staged_bin.display()
        )
    })?;
    fs::set_permissions(&staged_bin, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("failed to chmod {}: {e}", staged_bin.display()))?;

    let start_cmd = format!("./{}", file_name.to_string_lossy());
    fs::write(
        staging.join(".psht-start-command"),
        format!("{start_cmd}\n"),
    )
    .map_err(|e| format!("failed to write .psht-start-command: {e}"))?;

    Ok(staging)
}

fn deploy_binary(host: &str, app: &str, binary_path: &Path) -> Result<(), String> {
    let staging = stage_binary_dir(binary_path)?;
    let result = deploy_from_dir(host, app, &staging, false);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn deploy(host: &str, app: &str, cwd: &Path) -> Result<(), String> {
    deploy_from_dir(host, app, cwd, true)
}

fn deploy_with_app_or_binary(host: &str, cwd: &Path, value: Option<&str>) -> Result<(), String> {
    if let Some(arg) = value {
        if looks_like_path(arg) {
            let binary_path = resolve_binary_path(cwd, arg);
            if !binary_path.is_file() {
                return Err(format!("binary not found: {}", binary_path.display()));
            }
            let name = app_name(None, cwd);
            app_name::validate_app_name(&name)?;
            return deploy_binary(host, &name, &binary_path);
        }
    }
    let name = app_name(value, cwd);
    app_name::validate_app_name(&name)?;
    deploy(host, &name, cwd)
}

fn save_config(config: &Config, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create config dir: {e}"))?;
    }
    let content =
        toml::to_string_pretty(config).map_err(|e| format!("failed to serialize config: {e}"))?;
    std::fs::write(path, content).map_err(|e| format!("failed to write config: {e}"))?;
    Ok(())
}

fn setup_project_in(host: &str, cwd: &Path, config_path: &Path) -> Result<(), String> {
    if cwd.join(".git").is_dir() {
        let _ = Command::new("git")
            .args(["remote", "remove", "psht"])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let app = app_name(None, cwd);
        let url = format!("psht@{host}:{app}");
        let status = Command::new("git")
            .args(["remote", "add", "psht", &url])
            .current_dir(cwd)
            .status()
            .map_err(|e| format!("failed to add git remote: {e}"))?;
        if !status.success() {
            return Err("failed to add git remote".to_string());
        }
    }
    let cwd_str = cwd.to_string_lossy().to_string();
    let mut config = load_config_from(config_path);
    if !config.projects.contains_key(&cwd_str) {
        config.projects.insert(cwd_str, host.to_string());
        save_config(&config, config_path)?;
    }
    eprintln!("Ready! Deploy with: psht deploy");
    Ok(())
}

fn update(host: &str) -> Result<(), String> {
    let mut ssh = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .arg("update")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    let stdin = ssh
        .stdout
        .take()
        .ok_or_else(|| "failed to capture ssh stdout".to_string())?;
    let script_status = Command::new("sh")
        .stdin(stdin)
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run update script: {e}"))?;
    let ssh_status = ssh
        .wait()
        .map_err(|e| format!("failed to wait for ssh: {e}"))?;

    check_update_pipeline_status(script_status.success(), ssh_status.success())
}

fn check_update_pipeline_status(script_success: bool, ssh_success: bool) -> Result<(), String> {
    if !script_success {
        return Err("update failed while running installer script".to_string());
    }
    if !ssh_success {
        return Err("update failed while fetching installer script over ssh".to_string());
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let cwd = env::current_dir().map_err(|e| format!("failed to get cwd: {e}"))?;
    let home = env::var("HOME").map_err(|e| format!("HOME not set: {e}"))?;
    let config_path = config_path_at(Path::new(&home));
    let config = load_config_from(&config_path);

    match cli.command {
        CliCommand::Setup => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            setup_project_in(&host, &cwd, &config_path)
        }
        CliCommand::Deploy { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            deploy_with_app_or_binary(&host, &cwd, app.as_deref())
        }
        CliCommand::Ps => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            ssh_cmd(&host, &["ps"])
        }
        CliCommand::Logs { app, follow } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            app_name::validate_app_name(&app)?;
            if follow {
                ssh_cmd(&host, &["logs", "-f", &app])
            } else {
                ssh_cmd(&host, &["logs", &app])
            }
        }
        CliCommand::Stop { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            app_name::validate_app_name(&app)?;
            ssh_cmd(&host, &["stop", &app])
        }
        CliCommand::Destroy { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            app_name::validate_app_name(&app)?;
            ssh_cmd(&host, &["destroy", &app])
        }
        CliCommand::Update => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            update(&host)
        }
        CliCommand::IsCli => Ok(()),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn load_config_missing_file_returns_default() {
        let config = load_config_from(Path::new("/nonexistent/config.toml"));
        assert!(config.host.is_none());
        assert!(config.projects.is_empty());
    }

    #[test]
    fn load_config_parses_host() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "host = \"myserver\"\n").unwrap();
        let config = load_config_from(&path);
        assert_eq!(config.host.as_deref(), Some("myserver"));
    }

    #[test]
    fn load_config_parses_projects() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "host = \"default\"\n\n[projects]\n\"/home/user/myapp\" = \"other-host\"\n",
        )
        .unwrap();
        let config = load_config_from(&path);
        assert_eq!(
            config.projects.get("/home/user/myapp").map(|s| s.as_str()),
            Some("other-host")
        );
    }

    #[test]
    fn resolve_host_from_projects() {
        let config = Config {
            host: Some("default".to_string()),
            projects: HashMap::from([("/home/user/myapp".to_string(), "project-host".to_string())]),
        };
        let host = resolve_host_from(&config, "/home/user/myapp").unwrap();
        assert_eq!(host, "project-host");
    }

    #[test]
    fn resolve_host_falls_back_to_default() {
        let config = Config {
            host: Some("default-host".to_string()),
            projects: HashMap::new(),
        };
        let host = resolve_host_from(&config, "/some/other/dir").unwrap();
        assert_eq!(host, "default-host");
    }

    #[test]
    fn resolve_host_errors_when_no_host() {
        let config = Config {
            host: None,
            projects: HashMap::new(),
        };
        let result = resolve_host_from(&config, "/some/dir");
        assert!(result.is_err());
    }

    #[test]
    fn update_pipeline_status_ok_when_both_succeed() {
        assert!(check_update_pipeline_status(true, true).is_ok());
    }

    #[test]
    fn update_pipeline_status_fails_when_script_fails() {
        let err = check_update_pipeline_status(false, true).unwrap_err();
        assert!(err.contains("installer script"));
    }

    #[test]
    fn update_pipeline_status_fails_when_ssh_fails() {
        let err = check_update_pipeline_status(true, false).unwrap_err();
        assert!(err.contains("over ssh"));
    }

    #[test]
    fn app_name_explicit() {
        let name = app_name(Some("myapp"), Path::new("/whatever"));
        assert_eq!(name, "myapp");
    }

    #[test]
    fn app_name_from_cwd() {
        let name = app_name(None, Path::new("/home/user/cool-project"));
        assert_eq!(name, "cool-project");
    }

    #[test]
    fn config_path_at_uses_home() {
        let path = config_path_at(Path::new("/home/user"));
        assert_eq!(path, PathBuf::from("/home/user/.psht/config.toml"));
    }

    #[test]
    fn is_cli_probe_command_parses() {
        let cli = Cli::try_parse_from(["psht", "__is-cli"]).expect("probe command should parse");
        assert!(matches!(cli.command, CliCommand::IsCli));
    }

    #[test]
    fn looks_like_path_distinguishes_paths_from_app_names() {
        assert!(looks_like_path("path/to/bin"));
        assert!(looks_like_path("./bin"));
        assert!(looks_like_path("../bin"));
        assert!(!looks_like_path("myapp"));
    }

    #[test]
    fn stage_binary_dir_writes_start_command_marker() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("mybin");
        fs::write(&bin, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let staged = stage_binary_dir(&bin).unwrap();
        assert!(staged.join("mybin").is_file());
        let marker = fs::read_to_string(staged.join(".psht-start-command")).unwrap();
        assert_eq!(marker.trim(), "./mybin");
        let _ = fs::remove_dir_all(staged);
    }

    #[test]
    fn deploy_with_path_errors_when_binary_missing() {
        let cwd = tempdir().unwrap();
        let err =
            deploy_with_app_or_binary("example.com", cwd.path(), Some("bin/missing")).unwrap_err();
        assert!(err.contains("binary not found"));
    }

    #[test]
    fn setup_adds_git_remote() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"myhost\"\n").unwrap();

        setup_project_in("myhost", dir.path(), &config_path).unwrap();

        let output = Command::new("git")
            .args(["remote", "get-url", "psht"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let url = String::from_utf8(output.stdout).unwrap();
        let expected = format!(
            "psht@myhost:{}",
            dir.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(url.trim(), expected);
    }

    #[test]
    fn setup_is_idempotent_for_git() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"myhost\"\n").unwrap();

        setup_project_in("myhost", dir.path(), &config_path).unwrap();
        setup_project_in("myhost", dir.path(), &config_path).unwrap();

        let output = Command::new("git")
            .args(["remote", "get-url", "psht"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn setup_git_also_writes_project_config() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"myhost\"\n").unwrap();

        setup_project_in("myhost", dir.path(), &config_path).unwrap();

        let config = load_config_from(&config_path);
        assert!(
            config
                .projects
                .contains_key(&dir.path().to_string_lossy().to_string()),
            "git projects should also be written to config"
        );
    }

    #[test]
    fn setup_writes_project_to_config() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"myhost\"\n").unwrap();
        let project_dir = dir.path().join("myproject");
        fs::create_dir(&project_dir).unwrap();

        setup_project_in("myhost", &project_dir, &config_path).unwrap();

        let config = load_config_from(&config_path);
        assert_eq!(
            config
                .projects
                .get(&project_dir.to_string_lossy().to_string())
                .map(|s| s.as_str()),
            Some("myhost")
        );
    }

    #[test]
    fn setup_skips_existing_project() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let project_dir = dir.path().join("myproject");
        fs::create_dir(&project_dir).unwrap();
        let initial = format!(
            "host = \"myhost\"\n\n[projects]\n\"{}\" = \"myhost\"\n",
            project_dir.to_string_lossy()
        );
        fs::write(&config_path, &initial).unwrap();

        setup_project_in("myhost", &project_dir, &config_path).unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, initial, "config should not be rewritten");
    }

    #[test]
    fn save_config_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("nested").join("dir").join("config.toml");
        let config = Config {
            host: Some("myhost".to_string()),
            projects: HashMap::new(),
        };
        save_config(&config, &config_path).unwrap();
        let loaded = load_config_from(&config_path);
        assert_eq!(loaded.host.as_deref(), Some("myhost"));
    }

    #[test]
    fn save_config_preserves_existing_host() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"original\"\n").unwrap();
        let mut config = load_config_from(&config_path);
        config
            .projects
            .insert("/some/path".to_string(), "original".to_string());
        save_config(&config, &config_path).unwrap();
        let loaded = load_config_from(&config_path);
        assert_eq!(loaded.host.as_deref(), Some("original"));
        assert_eq!(
            loaded.projects.get("/some/path").map(|s| s.as_str()),
            Some("original")
        );
    }
}
