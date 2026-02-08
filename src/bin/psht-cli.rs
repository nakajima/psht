use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "psht", about = "deploy apps with psht", version)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Deploy the current directory
    Deploy {
        /// App name (defaults to current directory name)
        app: Option<String>,
    },
    /// List running apps
    Ps,
    /// Show app logs
    Logs { app: String },
    /// Stop and remove an app
    Stop { app: String },
    /// Set up project for deployment
    Setup,
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

fn deploy(host: &str, app: &str) -> Result<(), String> {
    let tar = Command::new("tar")
        .args(["cz", "--exclude=.git", "."])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run tar: {e}"))?;

    let status = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .args(["push", app])
        .stdin(tar.stdout.unwrap())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run ssh: {e}"))?;

    if !status.success() {
        return Err(format!("deploy failed with status {}", status));
    }
    Ok(())
}

fn save_config(config: &Config, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create config dir: {e}"))?;
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
            let name = app_name(app.as_deref(), &cwd);
            deploy(&host, &name)
        }
        CliCommand::Ps => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            ssh_cmd(&host, &["ps"])
        }
        CliCommand::Logs { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            ssh_cmd(&host, &["logs", &app])
        }
        CliCommand::Stop { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            ssh_cmd(&host, &["stop", &app])
        }
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
