use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[path = "../app_name.rs"]
mod app_name;

const PROJECT_CONFIG_FILE: &str = "psht.toml";

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
    /// Deploy the current directory
    Deploy {
        /// App name, path to a binary, or HTTPS release URL
        app: Option<String>,
        /// Release artifact URL (.tar.gz/.tgz/.zip)
        #[arg(long)]
        url: Option<String>,
        /// Custom start command
        #[arg(long)]
        start: Option<String>,
        /// App name for release deploys
        #[arg(long = "app")]
        app_flag: Option<String>,
        /// Path to binary inside archive
        #[arg(long)]
        bin: Option<String>,
        /// Force deploy even when payload hash is unchanged
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// List running apps
    Ps,
    /// Show app logs
    Logs {
        app: Option<String>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop an app
    Stop { app: Option<String> },
    /// Start a stopped app
    Start { app: Option<String> },
    /// Restart an app
    Restart { app: Option<String> },
    /// Stop and remove an app
    Destroy {
        /// Preserve the app's /storage volume
        #[arg(long)]
        keep_storage: bool,
        app: Option<String>,
    },
    /// Manage environment variables for the project app
    Env {
        #[arg(value_name = "KEY=value")]
        assignments: Vec<String>,
    },
    /// Unset one or more environment variables for the project app
    #[command(name = "env:unset")]
    EnvUnset {
        #[arg(value_name = "NAME")]
        names: Vec<String>,
    },
    /// Set up project for deployment
    Setup,
    /// Update the psht CLI
    Update,
    /// Manage host Tailscale connectivity
    Tailscale {
        #[command(subcommand)]
        command: TailscaleCommand,
    },
    #[command(name = "__is-cli", hide = true)]
    IsCli,
}

#[derive(Subcommand)]
enum TailscaleCommand {
    /// Show Tailscale status for an app container
    Status { app: Option<String> },
    /// Repair/bring up Tailscale for an app container
    Up { app: Option<String> },
    /// Bring Tailscale down for an app container
    Down { app: Option<String> },
}

#[derive(Deserialize, Serialize, Default)]
struct Config {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    projects: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preinstall: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    postinstall: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    apt_packages: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_env: Option<Vec<String>>,
}

#[derive(Copy, Clone, Debug)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

fn config_path_at(home: &Path) -> PathBuf {
    home.join(".psht").join("config.toml")
}

fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_CONFIG_FILE)
}

fn load_config_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn load_project_config(path: &Path) -> Result<Option<ProjectConfig>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let cfg: ProjectConfig =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(cfg))
}

fn save_project_config(path: &Path, cfg: &ProjectConfig) -> Result<(), String> {
    let content = toml::to_string_pretty(cfg)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn resolve_host_from(config: &Config, cwd: &str) -> Result<String, String> {
    if let Some(host) = config.projects.get(cwd) {
        return Ok(host.clone());
    }
    config
        .host
        .clone()
        .ok_or_else(|| "no host configured. Set `host` in ~/.psht/config.toml".to_string())
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

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_env_assignment(raw: &str) -> Result<(&str, &str), String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid env assignment '{raw}'; expected NAME=value"))?;
    if !is_valid_env_name(name) {
        return Err(format!("invalid env name '{name}'"));
    }
    Ok((name, value))
}

fn parse_env_name(raw: &str) -> Result<&str, String> {
    if !is_valid_env_name(raw) {
        return Err(format!("invalid env name '{raw}'"));
    }
    Ok(raw)
}

fn configured_project_app(cwd: &Path) -> Result<Option<String>, String> {
    let path = project_config_path(cwd);
    let cfg = load_project_config(&path)?;
    Ok(cfg
        .and_then(|cfg| cfg.app)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty()))
}

fn resolve_command_app(cwd: &Path, explicit: Option<&str>) -> Result<String, String> {
    let configured = configured_project_app(cwd)?;
    if let Some(explicit) = explicit {
        app_name::validate_app_name(explicit)?;
        if let Some(configured) = configured
            && configured != explicit
        {
            return Err(format!(
                "app '{explicit}' does not match `{PROJECT_CONFIG_FILE}` app '{configured}'"
            ));
        }
        return Ok(explicit.to_string());
    }

    if let Some(configured) = configured {
        app_name::validate_app_name(&configured)?;
        return Ok(configured);
    }

    Err(format!(
        "could not resolve app name from `{PROJECT_CONFIG_FILE}`. Run `psht setup`."
    ))
}

fn project_config_template(app: &str) -> String {
    format!(
        "app = \"{app}\"\n# url = \"https://github.com/org/repo/releases/download/v1.2.3/my-app-linux-amd64.tar.gz\"\n# start = \"./my-app --port $PORT\"\n# bin = \"my-app\"\n# required_env = [\"DATABASE_URL\", \"JWT_SECRET\"]\n"
    )
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

fn ssh_cmd_owned(host: &str, args: &[String]) -> Result<(), String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ssh_cmd(host, &refs)
}

fn deploy_from_dir(
    host: &str,
    app: &str,
    source_dir: &Path,
    exclude_git: bool,
    force: bool,
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

    let mut push_args = vec!["push".to_string()];
    if force {
        push_args.push("--force".to_string());
    }
    push_args.push(app.to_string());

    let status = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .args(&push_args)
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

fn mktemp_dir(prefix: &str) -> Result<PathBuf, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = env::temp_dir().join(format!("{prefix}-{}-{ts}", process::id()));
    fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create staging dir {}: {e}", dir.display()))?;
    Ok(dir)
}

fn stage_binary_dir(binary_path: &Path) -> Result<PathBuf, String> {
    let file_name = binary_path
        .file_name()
        .ok_or_else(|| format!("invalid binary path: {}", binary_path.display()))?;
    let start_cmd = format!("./{}", file_name.to_string_lossy());
    stage_binary_dir_with_start(binary_path, &start_cmd, None, None, None, None)
}

fn normalize_opt_vec(values: Option<&[String]>) -> Option<Vec<String>> {
    let values = values?;
    let mut normalized = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if normalized.iter().any(|v| v == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn write_deploy_hooks_config(
    staging: &Path,
    preinstall: Option<&str>,
    postinstall: Option<&str>,
    apt_packages: Option<&[String]>,
    required_env: Option<&[String]>,
) -> Result<(), String> {
    let cfg = ProjectConfig {
        preinstall: normalize_opt(preinstall),
        postinstall: normalize_opt(postinstall),
        apt_packages: normalize_opt_vec(apt_packages),
        required_env: normalize_opt_vec(required_env),
        ..ProjectConfig::default()
    };
    if cfg.preinstall.is_none()
        && cfg.postinstall.is_none()
        && cfg.apt_packages.is_none()
        && cfg.required_env.is_none()
    {
        return Ok(());
    }

    let content = toml::to_string_pretty(&cfg)
        .map_err(|e| format!("failed to serialize {PROJECT_CONFIG_FILE}: {e}"))?;
    fs::write(staging.join(PROJECT_CONFIG_FILE), content)
        .map_err(|e| format!("failed to write staged {PROJECT_CONFIG_FILE}: {e}"))
}

fn stage_binary_dir_with_start(
    binary_path: &Path,
    start: &str,
    preinstall: Option<&str>,
    postinstall: Option<&str>,
    apt_packages: Option<&[String]>,
    required_env: Option<&[String]>,
) -> Result<PathBuf, String> {
    let file_name = binary_path
        .file_name()
        .ok_or_else(|| format!("invalid binary path: {}", binary_path.display()))?;
    let start = start.trim();
    if start.is_empty() {
        return Err("start command is empty".to_string());
    }

    let staging = mktemp_dir("psht-bin")?;
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

    fs::write(staging.join(".psht-start-command"), format!("{start}\n"))
        .map_err(|e| format!("failed to write .psht-start-command: {e}"))?;
    write_deploy_hooks_config(
        &staging,
        preinstall,
        postinstall,
        apt_packages,
        required_env,
    )?;
    Ok(staging)
}

fn deploy_binary(
    host: &str,
    app: &str,
    binary_path: &Path,
    preinstall: Option<&str>,
    postinstall: Option<&str>,
    apt_packages: Option<&[String]>,
    required_env: Option<&[String]>,
    force: bool,
) -> Result<(), String> {
    let staging = if preinstall.is_none()
        && postinstall.is_none()
        && apt_packages.is_none()
        && required_env.is_none()
    {
        stage_binary_dir(binary_path)?
    } else {
        let file_name = binary_path
            .file_name()
            .ok_or_else(|| format!("invalid binary path: {}", binary_path.display()))?;
        let start_cmd = format!("./{}", file_name.to_string_lossy());
        stage_binary_dir_with_start(
            binary_path,
            &start_cmd,
            preinstall,
            postinstall,
            apt_packages,
            required_env,
        )?
    };
    let result = deploy_from_dir(host, app, &staging, false, force);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn is_git_worktree(cwd: &Path) -> bool {
    let output = match Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout).trim() == "true"
}

fn ensure_psht_git_remote(host: &str, app: &str, cwd: &Path) -> Result<(), String> {
    let expected = format!("psht@{host}:{app}");
    let output = Command::new("git")
        .args(["remote", "get-url", "psht"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to inspect git remote: {e}"))?;

    if output.status.success() {
        let current = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if current == expected {
            return Ok(());
        }
        let status = Command::new("git")
            .args(["remote", "set-url", "psht", &expected])
            .current_dir(cwd)
            .status()
            .map_err(|e| format!("failed to update git remote: {e}"))?;
        if !status.success() {
            return Err("failed to update git remote `psht`".to_string());
        }
        return Ok(());
    }

    let status = Command::new("git")
        .args(["remote", "add", "psht", &expected])
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("failed to add git remote: {e}"))?;
    if !status.success() {
        return Err("failed to add git remote `psht`".to_string());
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(format!("git {} failed", args.join(" ")));
        }
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn current_git_deploy_target(cwd: &Path) -> Result<(String, String), String> {
    let sha = git_output(cwd, &["rev-parse", "HEAD"])?;
    let ref_name = Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|out| !out.is_empty())
        .unwrap_or_else(|| sha.clone());
    Ok((ref_name, sha))
}

fn parse_push_updated(porcelain: &str) -> Option<bool> {
    let mut saw_ref_line = false;
    let mut updated = false;
    for line in porcelain.lines() {
        let Some(flag) = line.chars().next() else {
            continue;
        };
        if !matches!(flag, '=' | '*' | '+' | '-' | ' ') {
            continue;
        }
        saw_ref_line = true;
        if flag != '=' {
            updated = true;
        }
    }
    if saw_ref_line { Some(updated) } else { None }
}

fn deploy_ssh_args(app: &str, ref_name: &str, sha: &str, force: bool) -> Vec<String> {
    let mut args = vec![
        "deploy".to_string(),
        app.to_string(),
        "--ref".to_string(),
        ref_name.to_string(),
        "--sha".to_string(),
        sha.to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    args
}

fn deploy_current_ref_over_ssh(
    host: &str,
    app: &str,
    ref_name: &str,
    sha: &str,
    force: bool,
) -> Result<(), String> {
    let args = deploy_ssh_args(app, ref_name, sha, force);
    ssh_cmd_owned(host, &args)
}

fn deploy_from_git(host: &str, app: &str, cwd: &Path, force: bool) -> Result<(), String> {
    ensure_psht_git_remote(host, app, cwd)?;

    let (ref_name, sha) = current_git_deploy_target(cwd)?;
    let output = Command::new("git")
        .args(["push", "--porcelain", "psht", "HEAD"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git push: {e}"))?;

    // Preserve git push output so users can still see transport details.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !output.status.success() {
        return Err(format!("git push failed with status {}", output.status));
    }

    let pushed_updated = parse_push_updated(&stdout);
    if pushed_updated == Some(true) {
        return Ok(());
    }

    if force {
        eprintln!("-----> Forcing deploy of current git revision");
    } else if pushed_updated == Some(false) {
        eprintln!("-----> Verifying deploy state for current git revision");
    } else {
        eprintln!("-----> Push status unclear, verifying current git revision");
    }
    deploy_current_ref_over_ssh(host, app, &ref_name, &sha, force)
}

fn deploy(host: &str, app: &str, cwd: &Path, force: bool) -> Result<(), String> {
    deploy_from_dir(host, app, cwd, true, force)
}

fn deploy_with_app_or_binary(
    host: &str,
    cwd: &Path,
    value: Option<&str>,
    default_app: Option<&str>,
    preinstall: Option<&str>,
    postinstall: Option<&str>,
    apt_packages: Option<&[String]>,
    required_env: Option<&[String]>,
    force: bool,
) -> Result<(), String> {
    let default_app = default_app.map(str::trim).filter(|v| !v.is_empty());
    if let Some(value) = value
        && !looks_like_path(value)
        && let Some(default_app) = default_app
        && value != default_app
    {
        return Err(format!(
            "app '{value}' does not match `{PROJECT_CONFIG_FILE}` app '{default_app}'"
        ));
    }
    if let Some(arg) = value
        && looks_like_path(arg)
    {
        let binary_path = resolve_binary_path(cwd, arg);
        if !binary_path.is_file() {
            return Err(format!("binary not found: {}", binary_path.display()));
        }
        let name = default_app.map(ToString::to_string).ok_or_else(|| {
            format!("could not resolve app name from `{PROJECT_CONFIG_FILE}`. Run `psht setup`.")
        })?;
        app_name::validate_app_name(&name)?;
        eprintln!("-----> Deploying via binary");
        return deploy_binary(
            host,
            &name,
            &binary_path,
            preinstall,
            postinstall,
            apt_packages,
            required_env,
            force,
        );
    }
    let name = if let Some(value) = value {
        value.to_string()
    } else {
        default_app.map(ToString::to_string).ok_or_else(|| {
            format!("could not resolve app name from `{PROJECT_CONFIG_FILE}`. Run `psht setup`.")
        })?
    };
    app_name::validate_app_name(&name)?;
    if value.is_none() && is_git_worktree(cwd) {
        eprintln!("-----> Deploying via git");
        return deploy_from_git(host, &name, cwd, force);
    }
    eprintln!("-----> Deploying via tar");
    deploy(host, &name, cwd, force)
}

fn path_from_url(url: &str) -> &str {
    let no_fragment = url.split('#').next().unwrap_or(url);
    no_fragment.split('?').next().unwrap_or(no_fragment)
}

fn detect_archive_format(url: &str) -> Result<ArchiveFormat, String> {
    let path = path_from_url(url).to_ascii_lowercase();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        return Ok(ArchiveFormat::TarGz);
    }
    if path.ends_with(".zip") {
        return Ok(ArchiveFormat::Zip);
    }
    Err("unsupported archive format; expected .tar.gz, .tgz, or .zip URL".to_string())
}

fn download_url_to_file(url: &str, out: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .arg("-o")
        .arg(out)
        .status()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if !status.success() {
        return Err(format!(
            "failed to download {url} (curl exited with {status})"
        ));
    }
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, out_dir: &Path) -> Result<(), String> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(out_dir)
        .status()
        .map_err(|e| format!("failed to run tar: {e}"))?;
    if !status.success() {
        return Err(format!(
            "failed to unpack {} (tar exited with {status})",
            archive_path.display()
        ));
    }
    Ok(())
}

fn extract_zip(archive_path: &Path, out_dir: &Path) -> Result<(), String> {
    let status = Command::new("unzip")
        .arg("-qq")
        .arg(archive_path)
        .arg("-d")
        .arg(out_dir)
        .status()
        .map_err(|e| format!("failed to run unzip: {e}"))?;
    if !status.success() {
        return Err(format!(
            "failed to unpack {} (unzip exited with {status})",
            archive_path.display()
        ));
    }
    Ok(())
}

fn extract_archive(archive_path: &Path, out_dir: &Path, fmt: ArchiveFormat) -> Result<(), String> {
    match fmt {
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, out_dir),
        ArchiveFormat::Zip => extract_zip(archive_path, out_dir),
    }
}

fn collect_regular_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("failed to read dir entry: {e}"))?;
        let path = entry.path();
        let meta = entry
            .metadata()
            .map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            collect_regular_files(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn is_executable(path: &Path) -> Result<bool, String> {
    let mode = fs::metadata(path)
        .map_err(|e| format!("failed to stat {}: {e}", path.display()))?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

fn safe_join_relative(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err("`bin` must be a relative path inside the archive".to_string());
    }
    if rel_path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
    {
        return Err("`bin` cannot contain parent-directory traversal".to_string());
    }
    Ok(base.join(rel_path))
}

fn resolve_binary_from_archive(extracted_dir: &Path, bin: Option<&str>) -> Result<PathBuf, String> {
    if let Some(bin) = bin {
        let bin = bin.trim();
        if bin.is_empty() {
            return Err("`bin` cannot be empty".to_string());
        }
        let candidate = safe_join_relative(extracted_dir, bin)?;
        if !candidate.is_file() {
            return Err(format!("`bin` target not found in archive: {bin}"));
        }
        return Ok(candidate);
    }

    let mut files = Vec::new();
    collect_regular_files(extracted_dir, &mut files)?;
    if files.is_empty() {
        return Err("archive did not contain files".to_string());
    }

    let mut executable = Vec::new();
    for file in &files {
        if is_executable(file)? {
            executable.push(file.clone());
        }
    }

    if executable.len() == 1 {
        return Ok(executable.remove(0));
    }
    if executable.is_empty() && files.len() == 1 {
        return Ok(files.remove(0));
    }
    if executable.is_empty() {
        return Err(
            "no executable found in archive; set `bin` in psht.toml to select one file".to_string(),
        );
    }
    Err("multiple executable files found; set `bin` in psht.toml to choose one".to_string())
}

fn stage_binary_from_url(
    url: &str,
    start: &str,
    bin: Option<&str>,
    preinstall: Option<&str>,
    postinstall: Option<&str>,
    apt_packages: Option<&[String]>,
    required_env: Option<&[String]>,
) -> Result<PathBuf, String> {
    let tmp = mktemp_dir("psht-release")?;
    let archive_path = tmp.join("asset");
    let extract_dir = tmp.join("extract");
    fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("failed to create {}: {e}", extract_dir.display()))?;

    let fmt = detect_archive_format(url)?;
    download_url_to_file(url, &archive_path)?;
    extract_archive(&archive_path, &extract_dir, fmt)?;
    let binary = resolve_binary_from_archive(&extract_dir, bin)?;
    let staged = stage_binary_dir_with_start(
        &binary,
        start,
        preinstall,
        postinstall,
        apt_packages,
        required_env,
    );
    let _ = fs::remove_dir_all(&tmp);
    staged
}

fn strip_archive_suffix(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tgz", ".zip"] {
        if lower.ends_with(suffix) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

fn strip_arch_suffix(name: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        "-x86_64-unknown-linux-gnu",
        "-aarch64-unknown-linux-gnu",
        "-x86_64-unknown-linux-musl",
        "-aarch64-unknown-linux-musl",
        "-x86_64-apple-darwin",
        "-aarch64-apple-darwin",
        "-x86_64-pc-windows-msvc",
        "-x86_64-pc-windows-gnu",
        "-aarch64-pc-windows-msvc",
        "-linux-amd64",
        "-linux-arm64",
        "-darwin-amd64",
        "-darwin-arm64",
        "-windows-amd64",
        "-windows-arm64",
    ];

    let lower = name.to_ascii_lowercase();
    for suffix in SUFFIXES {
        if lower.ends_with(suffix) && name.len() > suffix.len() {
            return name[..name.len() - suffix.len()].trim_end_matches('-');
        }
    }
    name
}

fn is_semverish(value: &str) -> bool {
    let value = value
        .strip_prefix('v')
        .or_else(|| value.strip_prefix('V'))
        .unwrap_or(value);
    if !value.contains('.') {
        return false;
    }
    if !value.bytes().any(|b| b.is_ascii_digit()) {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn strip_version_suffix(name: &str) -> &str {
    for (idx, ch) in name.char_indices().rev() {
        if ch != '-' {
            continue;
        }
        let candidate = &name[idx + 1..];
        if is_semverish(candidate) {
            let base = name[..idx].trim_end_matches('-');
            if !base.is_empty() {
                return base;
            }
        }
    }
    name
}

fn sanitize_app_name(input: &str) -> String {
    let mut output = String::new();
    let mut prev_dash = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            output.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            output.push('-');
            prev_dash = true;
        }
    }

    let output = output.trim_matches('-').to_string();
    if output == "." || output == ".." {
        return String::new();
    }
    output
}

fn is_https_url(value: &str) -> bool {
    value.trim().to_ascii_lowercase().starts_with("https://")
}

fn normalize_deploy_target(
    app: Option<String>,
    url: Option<String>,
) -> Result<(Option<String>, Option<String>), String> {
    match (app, url) {
        (Some(app), Some(url)) if is_https_url(&app) => Err(
            "deploy URL was provided both positionally and with --url; use one form".to_string(),
        ),
        (Some(app), None) if is_https_url(&app) => Ok((None, Some(app))),
        (app, url) => Ok((app, url)),
    }
}

fn derive_app_name_from_url(url: &str) -> Result<String, String> {
    let path = path_from_url(url);
    let segment = path
        .rsplit('/')
        .next()
        .ok_or_else(|| "could not derive app name from URL".to_string())?;
    if segment.is_empty() {
        return Err("could not derive app name from URL; set `app` in psht.toml".to_string());
    }
    let stripped = strip_archive_suffix(segment);
    let stripped = strip_arch_suffix(stripped);
    let stripped = strip_version_suffix(stripped);
    let sanitized = sanitize_app_name(stripped);
    if sanitized.is_empty() {
        return Err("derived app name is empty; set `app` in psht.toml".to_string());
    }
    app_name::validate_app_name(&sanitized)?;
    Ok(sanitized)
}

fn has_release_settings(cfg: &ProjectConfig) -> bool {
    cfg.url.is_some() || cfg.start.is_some() || cfg.app.is_some() || cfg.bin.is_some()
}

fn normalize_opt(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn cli_release_settings(
    url: Option<&str>,
    start: Option<&str>,
    app: Option<&str>,
    bin: Option<&str>,
) -> ProjectConfig {
    ProjectConfig {
        url: normalize_opt(url),
        start: normalize_opt(start),
        app: normalize_opt(app),
        bin: normalize_opt(bin),
        preinstall: None,
        postinstall: None,
        apt_packages: None,
        required_env: None,
    }
}

fn push_conflict(
    conflicts: &mut Vec<&'static str>,
    key: &'static str,
    file_value: &Option<String>,
    cli_value: &Option<String>,
) {
    if let (Some(file_value), Some(cli_value)) = (file_value.as_deref(), cli_value.as_deref())
        && file_value.trim() != cli_value.trim()
    {
        conflicts.push(key);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseUrlIdentity {
    host: String,
    owner: String,
    repo: String,
    asset_name: String,
}

fn normalize_release_asset_name(asset: &str) -> Option<String> {
    let stripped = strip_archive_suffix(asset);
    let stripped = strip_arch_suffix(stripped);
    let stripped = strip_version_suffix(stripped);
    let normalized = sanitize_app_name(stripped);
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

fn parse_release_url_identity(url: &str) -> Option<ReleaseUrlIdentity> {
    let path = path_from_url(url).trim();
    let (scheme, rest) = path.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, path),
        None => (rest, ""),
    };
    if host.trim().is_empty() {
        return None;
    }

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 6 {
        return None;
    }
    if !segments[2].eq_ignore_ascii_case("releases")
        || !segments[3].eq_ignore_ascii_case("download")
        || segments[4].is_empty()
    {
        return None;
    }

    let asset_name = normalize_release_asset_name(segments[5])?;
    Some(ReleaseUrlIdentity {
        host: host.to_ascii_lowercase(),
        owner: segments[0].to_string(),
        repo: segments[1].to_string(),
        asset_name,
    })
}

fn is_same_project_release_url(file_url: &str, cli_url: &str) -> bool {
    let file_identity = match parse_release_url_identity(file_url) {
        Some(identity) => identity,
        None => return false,
    };
    let cli_identity = match parse_release_url_identity(cli_url) {
        Some(identity) => identity,
        None => return false,
    };
    file_identity == cli_identity
}

fn reconcile_release_url_override(
    config_path: &Path,
    file_cfg: &ProjectConfig,
    cli_cfg: &ProjectConfig,
) -> Result<ProjectConfig, String> {
    let mut merged = file_cfg.clone();
    let file_url = file_cfg.url.as_deref().map(str::trim);
    let cli_url = cli_cfg.url.as_deref().map(str::trim);

    if let (Some(file_url), Some(cli_url)) = (file_url, cli_url)
        && file_url != cli_url
    {
        if !is_same_project_release_url(file_url, cli_url) {
            return Err(format!(
                "release URL override is allowed only for same-project version bumps; edit {} to change projects",
                PROJECT_CONFIG_FILE
            ));
        }
        merged.url = Some(cli_url.to_string());
        save_project_config(config_path, &merged)?;
        eprintln!("Updated {PROJECT_CONFIG_FILE} `url` from CLI for this version bump.");
    }

    Ok(merged)
}

fn ensure_no_release_conflicts(
    file_cfg: &ProjectConfig,
    cli_cfg: &ProjectConfig,
) -> Result<(), String> {
    let mut conflicts = Vec::new();
    push_conflict(&mut conflicts, "start", &file_cfg.start, &cli_cfg.start);
    push_conflict(&mut conflicts, "app", &file_cfg.app, &cli_cfg.app);
    push_conflict(&mut conflicts, "bin", &file_cfg.bin, &cli_cfg.bin);

    if conflicts.is_empty() {
        return Ok(());
    }
    Err(format!(
        "conflicting settings between {} and CLI for keys: {}",
        PROJECT_CONFIG_FILE,
        conflicts.join(", ")
    ))
}

fn prompt_required(label: &str, default: Option<&str>) -> Result<String, String> {
    loop {
        match default {
            Some(default) => eprint!("{label} [{default}]: "),
            None => eprint!("{label}: "),
        }
        io::stderr()
            .flush()
            .map_err(|e| format!("failed to flush stderr: {e}"))?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("failed to read input: {e}"))?;
        let value = line.trim();
        if value.is_empty() {
            if let Some(default) = default {
                return Ok(default.to_string());
            }
            eprintln!("Value is required.");
            continue;
        }
        return Ok(value.to_string());
    }
}

fn prompt_confirm(label: &str) -> Result<bool, String> {
    eprint!("{label} [y/N]: ");
    io::stderr()
        .flush()
        .map_err(|e| format!("failed to flush stderr: {e}"))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read input: {e}"))?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn bootstrap_project_config(
    path: &Path,
    defaults: &ProjectConfig,
) -> Result<ProjectConfig, String> {
    if !is_interactive() {
        return Err(format!(
            "{PROJECT_CONFIG_FILE} is missing. Run `psht deploy` once interactively to generate it."
        ));
    }

    eprintln!("No {PROJECT_CONFIG_FILE} found. Creating one for release deploy.");
    let url = prompt_required("Release URL", defaults.url.as_deref())?;
    let start = prompt_required("Start command", defaults.start.as_deref())?;

    let app = if let Some(app) = defaults.app.as_deref() {
        app.trim().to_string()
    } else {
        derive_app_name_from_url(&url)?
    };
    app_name::validate_app_name(&app)?;

    let cfg = ProjectConfig {
        url: Some(url),
        start: Some(start),
        app: Some(app),
        bin: defaults.bin.clone(),
        preinstall: defaults.preinstall.clone(),
        postinstall: defaults.postinstall.clone(),
        apt_packages: defaults.apt_packages.clone(),
        required_env: defaults.required_env.clone(),
    };

    let preview = toml::to_string_pretty(&cfg)
        .map_err(|e| format!("failed to serialize {PROJECT_CONFIG_FILE}: {e}"))?;
    eprintln!("Proposed {PROJECT_CONFIG_FILE}:\n{preview}");

    if !prompt_confirm("Write this file?")? {
        return Err(format!("aborted; {PROJECT_CONFIG_FILE} was not written"));
    }

    save_project_config(path, &cfg)?;
    eprintln!("Wrote {}", path.display());
    Ok(cfg)
}

fn deploy_from_release_config(host: &str, cfg: &ProjectConfig, force: bool) -> Result<(), String> {
    let url = cfg
        .url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{PROJECT_CONFIG_FILE}: missing `url`"))?;
    let start = cfg
        .start
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{PROJECT_CONFIG_FILE}: missing `start`"))?;

    let app = if let Some(app) = cfg.app.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        app.to_string()
    } else {
        derive_app_name_from_url(url)?
    };
    app_name::validate_app_name(&app)?;

    eprintln!("-----> Deploying via release-url");
    let staging = stage_binary_from_url(
        url,
        start,
        cfg.bin.as_deref(),
        cfg.preinstall.as_deref(),
        cfg.postinstall.as_deref(),
        cfg.apt_packages.as_deref(),
        cfg.required_env.as_deref(),
    )?;
    let result = deploy_from_dir(host, &app, &staging, false, force);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn deploy_with_project_config(
    host: &str,
    cwd: &Path,
    app: Option<&str>,
    url: Option<&str>,
    start: Option<&str>,
    app_flag: Option<&str>,
    bin: Option<&str>,
    force: bool,
) -> Result<(), String> {
    let config_path = project_config_path(cwd);
    let file_cfg = load_project_config(&config_path)?;
    let cli_cfg = cli_release_settings(url, start, app_flag, bin);

    match file_cfg {
        Some(file_cfg) => {
            if file_cfg.url.is_some() {
                let merged_cfg = reconcile_release_url_override(&config_path, &file_cfg, &cli_cfg)?;
                ensure_no_release_conflicts(&merged_cfg, &cli_cfg)?;
                if app.is_some() {
                    return Err(format!(
                        "positional deploy target cannot be used when {} has `url`; set `app` in {} instead",
                        PROJECT_CONFIG_FILE, PROJECT_CONFIG_FILE
                    ));
                }
                return deploy_from_release_config(host, &merged_cfg, force);
            }

            if has_release_settings(&cli_cfg) {
                return Err(format!(
                    "{} exists without release settings. Edit {} to add `url` and `start`.",
                    PROJECT_CONFIG_FILE, PROJECT_CONFIG_FILE
                ));
            }

            deploy_with_app_or_binary(
                host,
                cwd,
                app,
                file_cfg.app.as_deref(),
                file_cfg.preinstall.as_deref(),
                file_cfg.postinstall.as_deref(),
                file_cfg.apt_packages.as_deref(),
                file_cfg.required_env.as_deref(),
                force,
            )
        }
        None => {
            if app.is_some() && !has_release_settings(&cli_cfg) {
                return deploy_with_app_or_binary(
                    host, cwd, app, None, None, None, None, None, force,
                );
            }

            if app.is_none() && !has_release_settings(&cli_cfg) {
                return Err(format!(
                    "could not resolve app name from `{PROJECT_CONFIG_FILE}`. Run `psht setup`."
                ));
            }

            let cfg = bootstrap_project_config(&config_path, &cli_cfg)?;
            deploy_from_release_config(host, &cfg, force)
        }
    }
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

fn prompt_setup_app_name(cwd: &Path, project_cfg_path: &Path) -> Result<String, String> {
    if !is_interactive() {
        return Err(
            "setup requires an interactive terminal. Run `psht setup` interactively.".to_string(),
        );
    }

    let default = load_project_config(project_cfg_path)?
        .and_then(|cfg| cfg.app)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| app_name(None, cwd));
    let value = prompt_required("App name", Some(&default))?;
    app_name::validate_app_name(&value)?;
    Ok(value)
}

fn ensure_project_config_app(path: &Path, app: &str) -> Result<(), String> {
    if !path.exists() {
        fs::write(path, project_config_template(app))
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        return Ok(());
    }

    let content =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut root: toml::Value =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    let Some(table) = root.as_table_mut() else {
        return Err(format!("{} must contain a TOML table", path.display()));
    };
    table.insert("app".to_string(), toml::Value::String(app.to_string()));
    let updated = toml::to_string_pretty(&root)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, updated).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn setup_project_in(host: &str, cwd: &Path, config_path: &Path, app: &str) -> Result<(), String> {
    if is_git_worktree(cwd) {
        ensure_psht_git_remote(host, app, cwd)?;
    }

    let project_cfg_path = project_config_path(cwd);
    ensure_project_config_app(&project_cfg_path, app)?;

    let cwd_str = cwd.to_string_lossy().to_string();
    let mut config = load_config_from(config_path);
    if !config.projects.contains_key(&cwd_str) {
        config.projects.insert(cwd_str, host.to_string());
        save_config(&config, config_path)?;
    }
    eprintln!("Ready! Deploy with: psht deploy");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ServerUpdateManifest {
    version: String,
    #[serde(default)]
    forge_url: String,
}

fn configured_forge_url() -> String {
    env::var("PSHT_FORGE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://github.com/nakajima/psht".to_string())
}

fn detect_release_target() -> Result<&'static str, String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        (os, arch) => Err(format!("unsupported platform: {os}/{arch}")),
    }
}

fn binary_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().nth(1).map(|v| v.to_string())
}

fn parse_update_manifest_stdout(stdout: &str) -> Result<ServerUpdateManifest, String> {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(manifest) = serde_json::from_str::<ServerUpdateManifest>(trimmed) {
            return Ok(manifest);
        }
    }

    // Backward compatibility: pre-rust-native servers emit an installer script.
    let mut version = None;
    let mut forge_url = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if version.is_none()
            && let Some(raw) = trimmed.strip_prefix("VERSION=\"")
            && let Some(value) = raw.strip_suffix('"')
            && !value.trim().is_empty()
        {
            version = Some(value.trim().to_string());
            continue;
        }
        if forge_url.is_none()
            && let Some(raw) = trimmed.strip_prefix("FORGE_URL=\"${PSHT_FORGE_URL:-")
            && let Some(value) = raw.strip_suffix("}\"")
            && !value.trim().is_empty()
        {
            forge_url = Some(value.trim().trim_end_matches('/').to_string());
        }
    }
    if let Some(version) = version {
        return Ok(ServerUpdateManifest {
            version,
            forge_url: forge_url.unwrap_or_default(),
        });
    }

    Err("server update manifest was not valid JSON".to_string())
}

fn fetch_update_manifest(host: &str) -> Result<ServerUpdateManifest, String> {
    let output = Command::new("ssh")
        .arg(format!("psht@{host}"))
        .arg("update-cli")
        .output()
        .map_err(|e| format!("failed to run ssh: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "failed to fetch update manifest over ssh: {stderr}"
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_update_manifest_stdout(&stdout)
}

fn install_local_update(manifest: &ServerUpdateManifest) -> Result<(), String> {
    let version = manifest.version.trim();
    if version.is_empty() {
        return Err("update manifest missing version".to_string());
    }
    let forge_url = if manifest.forge_url.trim().is_empty() {
        configured_forge_url()
    } else {
        manifest.forge_url.trim().trim_end_matches('/').to_string()
    };
    let target = detect_release_target()?;
    let install_path =
        env::current_exe().map_err(|e| format!("failed to resolve current executable: {e}"))?;

    if binary_version(&install_path).as_deref() == Some(version) {
        eprintln!("psht {version} (up to date)");
        return Ok(());
    }

    let tmpdir = mktemp_dir("psht-update-")?;
    let tarball = tmpdir.join("psht.tar.gz");
    let tarball_s = tarball.to_string_lossy().to_string();
    let tmpdir_s = tmpdir.to_string_lossy().to_string();
    let asset_url =
        format!("{forge_url}/releases/download/v{version}/psht-{version}-{target}.tar.gz");
    let source_url = env::var("PSHT_SOURCE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| forge_url.clone());

    let result = (|| -> Result<(), String> {
        let mut candidate = tmpdir.join("psht");
        let release_downloaded = Command::new("curl")
            .args(["-fsSL", &asset_url, "-o", &tarball_s])
            .status()
            .map_err(|e| format!("failed to run curl: {e}"))?
            .success();

        if release_downloaded {
            let status = Command::new("tar")
                .args(["xzf", &tarball_s, "-C", &tmpdir_s])
                .status()
                .map_err(|e| format!("failed to run tar: {e}"))?;
            if !status.success() || !candidate.is_file() {
                return Err(format!(
                    "downloaded archive missing executable psht binary: {asset_url}"
                ));
            }
        } else {
            eprintln!("warning: no prebuilt psht release at {asset_url}; building from source");
            let source_root = tmpdir.join("source-root");
            let source_root_s = source_root.to_string_lossy().to_string();
            let tag = format!("v{version}");
            let status = Command::new("cargo")
                .args([
                    "install",
                    "--git",
                    &source_url,
                    "--tag",
                    &tag,
                    "--root",
                    &source_root_s,
                    "--bin",
                    "psht",
                ])
                .status()
                .map_err(|e| format!("failed to run cargo install: {e}"))?;
            if !status.success() {
                return Err("cargo install failed while building psht from source".to_string());
            }
            candidate = source_root.join("bin/psht");
            if !candidate.is_file() {
                return Err("cargo install completed but psht binary was not found".to_string());
            }
        }

        let candidate_version = binary_version(&candidate).unwrap_or_else(|| "unknown".to_string());
        if candidate_version != version {
            return Err(format!(
                "downloaded psht {candidate_version}, expected {version}"
            ));
        }

        let file_name = install_path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| format!("invalid install path: {}", install_path.display()))?;
        let staged = install_path.with_file_name(format!("{file_name}.new"));
        fs::copy(&candidate, &staged).map_err(|e| {
            format!(
                "failed to stage update {} -> {}: {e}",
                candidate.display(),
                staged.display()
            )
        })?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("failed to chmod {}: {e}", staged.display()))?;
        fs::rename(&staged, &install_path).map_err(|e| {
            format!(
                "failed to install update {} -> {}: {e}",
                staged.display(),
                install_path.display()
            )
        })?;

        let installed = binary_version(&install_path).unwrap_or_else(|| "unknown".to_string());
        if installed != version {
            return Err(format!(
                "installed psht {installed}, expected {version} at {}",
                install_path.display()
            ));
        }
        Ok(())
    })();

    let _ = fs::remove_dir_all(&tmpdir);
    result?;
    eprintln!("psht {version} (updated)");
    Ok(())
}

fn update(host: &str) -> Result<(), String> {
    let manifest = fetch_update_manifest(host)?;
    install_local_update(&manifest)
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
            let app = prompt_setup_app_name(&cwd, &project_config_path(&cwd))?;
            setup_project_in(&host, &cwd, &config_path, &app)
        }
        CliCommand::Deploy {
            app,
            url,
            start,
            app_flag,
            bin,
            force,
        } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let (app, url) = normalize_deploy_target(app, url)?;
            deploy_with_project_config(
                &host,
                &cwd,
                app.as_deref(),
                url.as_deref(),
                start.as_deref(),
                app_flag.as_deref(),
                bin.as_deref(),
                force,
            )
        }
        CliCommand::Ps => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            ssh_cmd(&host, &["ps"])
        }
        CliCommand::Logs { app, follow } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, app.as_deref())?;
            if follow {
                ssh_cmd(&host, &["logs", "-f", &app])
            } else {
                ssh_cmd(&host, &["logs", &app])
            }
        }
        CliCommand::Stop { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, app.as_deref())?;
            ssh_cmd(&host, &["stop", &app])
        }
        CliCommand::Start { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, app.as_deref())?;
            ssh_cmd(&host, &["start", &app])
        }
        CliCommand::Restart { app } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, app.as_deref())?;
            ssh_cmd(&host, &["restart", &app])
        }
        CliCommand::Destroy { app, keep_storage } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, app.as_deref())?;
            let mut args = vec!["destroy"];
            if keep_storage {
                args.push("--keep-storage");
            }
            args.push(&app);
            ssh_cmd(&host, &args)
        }
        CliCommand::Env { assignments } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, None)?;
            let mut args = vec!["env".to_string(), app];
            for assignment in &assignments {
                parse_env_assignment(assignment)?;
                args.push(assignment.to_string());
            }
            ssh_cmd_owned(&host, &args)
        }
        CliCommand::EnvUnset { names } => {
            if names.is_empty() {
                return Err("env:unset requires at least one NAME".to_string());
            }
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            let app = resolve_command_app(&cwd, None)?;
            let mut args = vec!["env-unset".to_string(), app];
            for name in &names {
                let name = parse_env_name(name)?;
                args.push(name.to_string());
            }
            ssh_cmd_owned(&host, &args)
        }
        CliCommand::Update => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            update(&host)
        }
        CliCommand::Tailscale { command } => {
            let host = resolve_host_from(&config, &cwd.to_string_lossy())?;
            match command {
                TailscaleCommand::Status { app } => {
                    let app = resolve_command_app(&cwd, app.as_deref())?;
                    ssh_cmd(&host, &["tailscale", "status", &app])
                }
                TailscaleCommand::Up { app } => {
                    let app = resolve_command_app(&cwd, app.as_deref())?;
                    ssh_cmd(&host, &["tailscale", "up", &app])
                }
                TailscaleCommand::Down { app } => {
                    let app = resolve_command_app(&cwd, app.as_deref())?;
                    ssh_cmd(&host, &["tailscale", "down", &app])
                }
            }
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
    fn load_project_config_missing_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("psht.toml");
        assert!(load_project_config(&path).unwrap().is_none());
    }

    #[test]
    fn load_project_config_parses_flat_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("psht.toml");
        fs::write(
            &path,
            "url = \"https://example.com/app.tar.gz\"\nstart = \"./app\"\napp = \"demo\"\npreinstall = \"echo pre\"\npostinstall = \"echo post\"\napt_packages = [\"curl\", \"git\"]\nrequired_env = [\"DATABASE_URL\"]\n",
        )
        .unwrap();
        let cfg = load_project_config(&path).unwrap().unwrap();
        assert_eq!(cfg.url.as_deref(), Some("https://example.com/app.tar.gz"));
        assert_eq!(cfg.start.as_deref(), Some("./app"));
        assert_eq!(cfg.app.as_deref(), Some("demo"));
        assert_eq!(cfg.preinstall.as_deref(), Some("echo pre"));
        assert_eq!(cfg.postinstall.as_deref(), Some("echo post"));
        assert_eq!(
            cfg.apt_packages,
            Some(vec!["curl".to_string(), "git".to_string()])
        );
        assert_eq!(cfg.required_env, Some(vec!["DATABASE_URL".to_string()]));
    }

    #[test]
    fn save_project_config_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("psht.toml");
        let cfg = ProjectConfig {
            url: Some("https://example.com/a.tar.gz".to_string()),
            start: Some("./app".to_string()),
            app: Some("demo".to_string()),
            bin: Some("bin/app".to_string()),
            preinstall: Some("echo pre".to_string()),
            postinstall: Some("echo post".to_string()),
            apt_packages: Some(vec!["curl".to_string(), "git".to_string()]),
            required_env: Some(vec!["DATABASE_URL".to_string()]),
        };
        save_project_config(&path, &cfg).unwrap();
        let loaded = load_project_config(&path).unwrap().unwrap();
        assert_eq!(loaded, cfg);
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
    fn detect_release_target_supported_in_tests() {
        // CI/dev environments for this repo are Linux/macOS amd64/arm64.
        assert!(detect_release_target().is_ok());
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
    fn project_config_path_uses_cwd() {
        let path = project_config_path(Path::new("/home/user/app"));
        assert_eq!(path, PathBuf::from("/home/user/app/psht.toml"));
    }

    #[test]
    fn is_cli_probe_command_parses() {
        let cli = Cli::try_parse_from(["psht", "__is-cli"]).expect("probe command should parse");
        assert!(matches!(cli.command, CliCommand::IsCli));
    }

    #[test]
    fn env_command_parses_assignments() {
        let cli = Cli::try_parse_from(["psht", "env", "A=1", "B=two"]).unwrap();
        match cli.command {
            CliCommand::Env { assignments } => {
                assert_eq!(assignments, vec!["A=1".to_string(), "B=two".to_string()]);
            }
            _ => panic!("expected env command"),
        }
    }

    #[test]
    fn env_unset_command_parses_names() {
        let cli = Cli::try_parse_from(["psht", "env:unset", "A", "B"]).unwrap();
        match cli.command {
            CliCommand::EnvUnset { names } => {
                assert_eq!(names, vec!["A".to_string(), "B".to_string()]);
            }
            _ => panic!("expected env:unset command"),
        }
    }

    #[test]
    fn start_command_parses_with_optional_app() {
        let cli = Cli::try_parse_from(["psht", "start"]).unwrap();
        match cli.command {
            CliCommand::Start { app } => {
                assert!(app.is_none());
            }
            _ => panic!("expected start command"),
        }
    }

    #[test]
    fn restart_command_parses_with_explicit_app() {
        let cli = Cli::try_parse_from(["psht", "restart", "demo"]).unwrap();
        match cli.command {
            CliCommand::Restart { app } => {
                assert_eq!(app.as_deref(), Some("demo"));
            }
            _ => panic!("expected restart command"),
        }
    }

    #[test]
    fn destroy_command_parses_with_keep_storage() {
        let cli = Cli::try_parse_from(["psht", "destroy", "--keep-storage", "demo"]).unwrap();
        match cli.command {
            CliCommand::Destroy { app, keep_storage } => {
                assert_eq!(app.as_deref(), Some("demo"));
                assert!(keep_storage);
            }
            _ => panic!("expected destroy command"),
        }
    }

    #[test]
    fn tailscale_status_command_parses() {
        let cli = Cli::try_parse_from(["psht", "tailscale", "status"]).unwrap();
        match cli.command {
            CliCommand::Tailscale { command } => match command {
                TailscaleCommand::Status { app } => {
                    assert!(app.is_none());
                }
                _ => panic!("expected tailscale status"),
            },
            _ => panic!("expected tailscale command"),
        }
    }

    #[test]
    fn tailscale_up_command_parses() {
        let cli = Cli::try_parse_from(["psht", "tailscale", "up"]).unwrap();
        match cli.command {
            CliCommand::Tailscale { command } => match command {
                TailscaleCommand::Up { app } => {
                    assert!(app.is_none());
                }
                _ => panic!("expected tailscale up"),
            },
            _ => panic!("expected tailscale command"),
        }
    }

    #[test]
    fn tailscale_down_command_parses() {
        let cli = Cli::try_parse_from(["psht", "tailscale", "down"]).unwrap();
        match cli.command {
            CliCommand::Tailscale { command } => match command {
                TailscaleCommand::Down { app } => {
                    assert!(app.is_none());
                }
                _ => panic!("expected tailscale down"),
            },
            _ => panic!("expected tailscale command"),
        }
    }

    #[test]
    fn tailscale_status_with_explicit_app_parses() {
        let cli = Cli::try_parse_from(["psht", "tailscale", "status", "demo"]).unwrap();
        match cli.command {
            CliCommand::Tailscale { command } => match command {
                TailscaleCommand::Status { app } => {
                    assert_eq!(app.as_deref(), Some("demo"));
                }
                _ => panic!("expected tailscale status"),
            },
            _ => panic!("expected tailscale command"),
        }
    }

    #[test]
    fn resolve_command_app_uses_project_config_when_present() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(PROJECT_CONFIG_FILE), "app = \"demo\"\n").unwrap();
        let app = resolve_command_app(dir.path(), None).unwrap();
        assert_eq!(app, "demo");
    }

    #[test]
    fn resolve_command_app_errors_without_explicit_or_project_app() {
        let dir = tempdir().unwrap();
        let err = resolve_command_app(dir.path(), None).unwrap_err();
        assert!(err.contains("Run `psht setup`"));
    }

    #[test]
    fn deploy_release_flags_parse() {
        let cli = Cli::try_parse_from([
            "psht",
            "deploy",
            "--url",
            "https://example.com/app.tar.gz",
            "--start",
            "./app --port $PORT",
            "--app",
            "my-app",
            "--bin",
            "dist/app",
        ])
        .expect("deploy should parse");

        match cli.command {
            CliCommand::Deploy {
                app,
                url,
                start,
                app_flag,
                bin,
                force,
            } => {
                assert!(app.is_none());
                assert_eq!(url.as_deref(), Some("https://example.com/app.tar.gz"));
                assert_eq!(start.as_deref(), Some("./app --port $PORT"));
                assert_eq!(app_flag.as_deref(), Some("my-app"));
                assert_eq!(bin.as_deref(), Some("dist/app"));
                assert!(!force);
            }
            _ => panic!("expected deploy command"),
        }
    }

    #[test]
    fn deploy_force_flag_parse() {
        let cli = Cli::try_parse_from([
            "psht",
            "deploy",
            "--force",
            "--url",
            "https://example.com/app.tar.gz",
            "--start",
            "./app",
        ])
        .expect("deploy should parse");

        match cli.command {
            CliCommand::Deploy { force, .. } => assert!(force),
            _ => panic!("expected deploy command"),
        }
    }

    #[test]
    fn deploy_force_short_flag_parse() {
        let cli = Cli::try_parse_from([
            "psht",
            "deploy",
            "-f",
            "--url",
            "https://example.com/app.tar.gz",
            "--start",
            "./app",
        ])
        .expect("deploy should parse");

        match cli.command {
            CliCommand::Deploy { force, .. } => assert!(force),
            _ => panic!("expected deploy command"),
        }
    }

    #[test]
    fn deploy_release_positional_https_url_maps_to_url() {
        let cli = Cli::try_parse_from(["psht", "deploy", "https://example.com/app.tar.gz"])
            .expect("deploy should parse");

        match cli.command {
            CliCommand::Deploy { app, url, .. } => {
                let (app, url) =
                    normalize_deploy_target(app, url).expect("target normalization should succeed");
                assert!(app.is_none());
                assert_eq!(url.as_deref(), Some("https://example.com/app.tar.gz"));
            }
            _ => panic!("expected deploy command"),
        }
    }

    #[test]
    fn deploy_release_positional_https_url_conflicts_with_url_flag() {
        let cli = Cli::try_parse_from([
            "psht",
            "deploy",
            "https://example.com/a.tar.gz",
            "--url",
            "https://example.com/b.tar.gz",
        ])
        .expect("deploy should parse");

        match cli.command {
            CliCommand::Deploy { app, url, .. } => {
                let err = normalize_deploy_target(app, url).unwrap_err();
                assert!(err.contains("both positionally and with --url"));
            }
            _ => panic!("expected deploy command"),
        }
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
    fn stage_binary_dir_with_start_writes_custom_marker() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("mybin");
        fs::write(&bin, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let staged = stage_binary_dir_with_start(
            &bin,
            "./mybin --debug",
            Some("echo pre"),
            Some("echo post"),
            Some(&["curl".to_string(), "git".to_string()]),
            Some(&["DATABASE_URL".to_string()]),
        )
        .unwrap();
        let marker = fs::read_to_string(staged.join(".psht-start-command")).unwrap();
        assert_eq!(marker.trim(), "./mybin --debug");
        let hooks = fs::read_to_string(staged.join(PROJECT_CONFIG_FILE)).unwrap();
        assert!(hooks.contains("preinstall = \"echo pre\""));
        assert!(hooks.contains("postinstall = \"echo post\""));
        assert!(hooks.contains("apt_packages = ["));
        assert!(hooks.contains("\"curl\""));
        assert!(hooks.contains("\"git\""));
        assert!(hooks.contains("required_env = ["));
        assert!(hooks.contains("\"DATABASE_URL\""));
        let _ = fs::remove_dir_all(staged);
    }

    #[test]
    fn stage_binary_dir_with_start_rejects_empty() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("mybin");
        fs::write(&bin, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let err = stage_binary_dir_with_start(&bin, "  ", None, None, None, None).unwrap_err();
        assert!(err.contains("start command is empty"));
    }

    #[test]
    fn deploy_with_path_errors_when_binary_missing() {
        let cwd = tempdir().unwrap();
        let err = deploy_with_app_or_binary(
            "example.com",
            cwd.path(),
            Some("bin/missing"),
            Some("myapp"),
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("binary not found"));
    }

    #[test]
    fn deploy_with_explicit_app_rejects_psht_toml_mismatch() {
        let cwd = tempdir().unwrap();
        let err = deploy_with_app_or_binary(
            "example.com",
            cwd.path(),
            Some("other"),
            Some("myapp"),
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn is_git_worktree_detects_repo_and_non_repo() {
        let repo = tempdir().unwrap();
        let non_repo = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(is_git_worktree(repo.path()));
        assert!(!is_git_worktree(non_repo.path()));
    }

    #[test]
    fn ensure_psht_git_remote_adds_and_updates_remote() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        ensure_psht_git_remote("host-a", "demo", dir.path()).unwrap();

        let output = Command::new("git")
            .args(["remote", "get-url", "psht"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "psht@host-a:demo"
        );

        ensure_psht_git_remote("host-b", "demo", dir.path()).unwrap();
        let output = Command::new("git")
            .args(["remote", "get-url", "psht"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "psht@host-b:demo"
        );
    }

    #[test]
    fn parse_push_updated_reports_up_to_date() {
        let out = "= refs/heads/main:refs/heads/main [up to date]\nDone\n";
        assert_eq!(parse_push_updated(out), Some(false));
    }

    #[test]
    fn parse_push_updated_reports_changed_refs() {
        let out = "  refs/heads/main:refs/heads/main abc..def\nDone\n";
        assert_eq!(parse_push_updated(out), Some(true));
        let out = "* refs/heads/main:refs/heads/main [new branch]\nDone\n";
        assert_eq!(parse_push_updated(out), Some(true));
    }

    #[test]
    fn parse_push_updated_ignores_non_ref_lines() {
        let out = "To psht:app\nDone\n";
        assert_eq!(parse_push_updated(out), None);
    }

    #[test]
    fn deploy_ssh_args_without_force() {
        let args = deploy_ssh_args("myapp", "refs/heads/main", "deadbeef", false);
        assert_eq!(
            args,
            vec![
                "deploy".to_string(),
                "myapp".to_string(),
                "--ref".to_string(),
                "refs/heads/main".to_string(),
                "--sha".to_string(),
                "deadbeef".to_string(),
            ]
        );
    }

    #[test]
    fn deploy_ssh_args_with_force() {
        let args = deploy_ssh_args("myapp", "refs/heads/main", "deadbeef", true);
        assert_eq!(
            args,
            vec![
                "deploy".to_string(),
                "myapp".to_string(),
                "--ref".to_string(),
                "refs/heads/main".to_string(),
                "--sha".to_string(),
                "deadbeef".to_string(),
                "--force".to_string(),
            ]
        );
    }

    #[test]
    fn detect_archive_format_accepts_tar_and_zip() {
        assert!(matches!(
            detect_archive_format("https://x/y/app.tar.gz").unwrap(),
            ArchiveFormat::TarGz
        ));
        assert!(matches!(
            detect_archive_format("https://x/y/app.tgz?dl=1").unwrap(),
            ArchiveFormat::TarGz
        ));
        assert!(matches!(
            detect_archive_format("https://x/y/app.zip").unwrap(),
            ArchiveFormat::Zip
        ));
    }

    #[test]
    fn detect_archive_format_rejects_other_extensions() {
        let err = detect_archive_format("https://x/y/app.bin").unwrap_err();
        assert!(err.contains("unsupported archive format"));
    }

    #[test]
    fn derive_app_name_from_url_strips_archive_suffix() {
        let name = derive_app_name_from_url("https://example.com/releases/my-app.tar.gz").unwrap();
        assert_eq!(name, "my-app");
    }

    #[test]
    fn derive_app_name_from_url_sanitizes() {
        let name =
            derive_app_name_from_url("https://example.com/releases/My App (linux).zip").unwrap();
        assert_eq!(name, "My-App-linux");
    }

    #[test]
    fn derive_app_name_from_url_strips_version_and_target_triple() {
        let name = derive_app_name_from_url(
            "https://example.com/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz",
        )
        .unwrap();
        assert_eq!(name, "hyperlinked");
    }

    #[test]
    fn derive_app_name_from_url_strips_version_and_go_style_target() {
        let name = derive_app_name_from_url(
            "https://example.com/releases/download/v1.2.3/my-app-v1.2.3-linux-amd64.tar.gz",
        )
        .unwrap();
        assert_eq!(name, "my-app");
    }

    #[test]
    fn ensure_no_release_conflicts_detects_non_url_mismatch() {
        let file = ProjectConfig {
            url: Some("https://a".to_string()),
            start: Some("./app".to_string()),
            app: Some("myapp".to_string()),
            bin: None,
            preinstall: None,
            postinstall: None,
            apt_packages: None,
            required_env: None,
        };
        let cli = ProjectConfig {
            url: Some("https://b".to_string()),
            start: Some("./other".to_string()),
            app: None,
            bin: None,
            preinstall: None,
            postinstall: None,
            apt_packages: None,
            required_env: None,
        };
        let err = ensure_no_release_conflicts(&file, &cli).unwrap_err();
        assert!(err.contains("start"));
    }

    #[test]
    fn is_same_project_release_url_accepts_version_bump() {
        let old_url = "https://github.com/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
        let new_url = "https://github.com/org/repo/releases/download/v0.0.4/hyperlinked-0.0.4-x86_64-unknown-linux-gnu.tar.gz";
        assert!(is_same_project_release_url(old_url, new_url));
    }

    #[test]
    fn is_same_project_release_url_rejects_repo_change() {
        let old_url = "https://github.com/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
        let new_url = "https://github.com/org/other/releases/download/v0.0.4/hyperlinked-0.0.4-x86_64-unknown-linux-gnu.tar.gz";
        assert!(!is_same_project_release_url(old_url, new_url));
    }

    #[test]
    fn is_same_project_release_url_rejects_asset_name_change() {
        let old_url = "https://github.com/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
        let new_url = "https://github.com/org/repo/releases/download/v0.0.4/another-app-0.0.4-x86_64-unknown-linux-gnu.tar.gz";
        assert!(!is_same_project_release_url(old_url, new_url));
    }

    #[test]
    fn is_same_project_release_url_rejects_non_release_download_shape() {
        let old_url = "https://github.com/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
        let new_url = "https://github.com/org/repo/releases/tag/v0.0.4";
        assert!(!is_same_project_release_url(old_url, new_url));
    }

    #[test]
    fn has_release_settings_ignores_hook_only_config() {
        let cfg = ProjectConfig {
            preinstall: Some("echo pre".to_string()),
            postinstall: Some("echo post".to_string()),
            apt_packages: Some(vec!["curl".to_string()]),
            ..ProjectConfig::default()
        };
        assert!(!has_release_settings(&cfg));
    }

    #[test]
    fn resolve_binary_from_archive_prefers_single_executable() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("app");
        fs::write(&bin, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = resolve_binary_from_archive(dir.path(), None).unwrap();
        assert_eq!(resolved, bin);
    }

    #[test]
    fn resolve_binary_from_archive_falls_back_to_single_file() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("app");
        fs::write(&bin, "binary data").unwrap();
        let resolved = resolve_binary_from_archive(dir.path(), None).unwrap();
        assert_eq!(resolved, bin);
    }

    #[test]
    fn resolve_binary_from_archive_errors_on_many_executables() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, "x").unwrap();
        fs::write(&b, "y").unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&b, fs::Permissions::from_mode(0o755)).unwrap();
        let err = resolve_binary_from_archive(dir.path(), None).unwrap_err();
        assert!(err.contains("multiple executable"));
    }

    #[test]
    fn resolve_binary_from_archive_uses_bin_override() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("dist");
        fs::create_dir_all(&nested).unwrap();
        let bin = nested.join("app");
        fs::write(&bin, "x").unwrap();
        let resolved = resolve_binary_from_archive(dir.path(), Some("dist/app")).unwrap();
        assert_eq!(resolved, bin);
    }

    #[test]
    fn safe_join_relative_rejects_parent() {
        let err = safe_join_relative(Path::new("/tmp/x"), "../bad").unwrap_err();
        assert!(err.contains("parent-directory"));
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

        setup_project_in("myhost", dir.path(), &config_path, "myapp").unwrap();

        let output = Command::new("git")
            .args(["remote", "get-url", "psht"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let url = String::from_utf8(output.stdout).unwrap();
        let expected = "psht@myhost:myapp";
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

        setup_project_in("myhost", dir.path(), &config_path, "myapp").unwrap();
        setup_project_in("myhost", dir.path(), &config_path, "myapp").unwrap();

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

        setup_project_in("myhost", dir.path(), &config_path, "myapp").unwrap();

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

        setup_project_in("myhost", &project_dir, &config_path, "myproject").unwrap();

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
    fn setup_writes_psht_toml_template() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "host = \"myhost\"\n").unwrap();
        let project_dir = dir.path().join("myproject");
        fs::create_dir(&project_dir).unwrap();

        setup_project_in("myhost", &project_dir, &config_path, "myproject").unwrap();

        let psht = fs::read_to_string(project_dir.join(PROJECT_CONFIG_FILE)).unwrap();
        assert!(psht.contains("app = \"myproject\""));
        assert!(psht.contains("# url ="));
        assert!(psht.contains("# required_env ="));
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

        setup_project_in("myhost", &project_dir, &config_path, "myproject").unwrap();

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

    #[test]
    fn parse_update_manifest_stdout_reads_embedded_json_line() {
        let stdout = r#"#!/bin/sh
set -e
cat >/dev/null <<'__PSHT_UPDATE_MANIFEST__'
{"version":"0.2.47","forge_url":"https://github.com/nakajima/psht"}
__PSHT_UPDATE_MANIFEST__
"#;

        let manifest = parse_update_manifest_stdout(stdout).unwrap();
        assert_eq!(manifest.version, "0.2.47");
        assert_eq!(manifest.forge_url, "https://github.com/nakajima/psht");
    }

    #[test]
    fn parse_update_manifest_stdout_reads_legacy_script_assignments() {
        let stdout = r#"#!/bin/sh
VERSION="0.2.13"
FORGE_URL="${PSHT_FORGE_URL:-https://example.com/org/repo}"
"#;

        let manifest = parse_update_manifest_stdout(stdout).unwrap();
        assert_eq!(manifest.version, "0.2.13");
        assert_eq!(manifest.forge_url, "https://example.com/org/repo");
    }

    #[test]
    fn parse_update_manifest_stdout_errors_without_manifest() {
        let err = parse_update_manifest_stdout("#!/bin/sh\necho hi\n").unwrap_err();
        assert!(err.contains("not valid JSON"));
    }
}
