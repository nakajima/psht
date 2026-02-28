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
    /// Deploy the current directory (Caddy routing is experimental)
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
    stage_binary_dir_with_start(binary_path, &start_cmd)
}

fn stage_binary_dir_with_start(binary_path: &Path, start: &str) -> Result<PathBuf, String> {
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
    Ok(staging)
}

fn deploy_binary(host: &str, app: &str, binary_path: &Path) -> Result<(), String> {
    let staging = stage_binary_dir(binary_path)?;
    let result = deploy_from_dir(host, app, &staging, false);
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

fn deploy_from_git(host: &str, app: &str, cwd: &Path) -> Result<(), String> {
    ensure_psht_git_remote(host, app, cwd)?;
    let status = Command::new("git")
        .args(["push", "psht", "HEAD"])
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("failed to run git push: {e}"))?;
    if !status.success() {
        return Err(format!("git push failed with status {status}"));
    }
    Ok(())
}

fn deploy(host: &str, app: &str, cwd: &Path) -> Result<(), String> {
    deploy_from_dir(host, app, cwd, true)
}

fn deploy_with_app_or_binary(host: &str, cwd: &Path, value: Option<&str>) -> Result<(), String> {
    if let Some(arg) = value
        && looks_like_path(arg)
    {
        let binary_path = resolve_binary_path(cwd, arg);
        if !binary_path.is_file() {
            return Err(format!("binary not found: {}", binary_path.display()));
        }
        let name = app_name(None, cwd);
        app_name::validate_app_name(&name)?;
        return deploy_binary(host, &name, &binary_path);
    }
    let name = app_name(value, cwd);
    app_name::validate_app_name(&name)?;
    if value.is_none() && is_git_worktree(cwd) {
        eprintln!("-----> Deploying via git");
        return deploy_from_git(host, &name, cwd);
    }
    deploy(host, &name, cwd)
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

fn stage_binary_from_url(url: &str, start: &str, bin: Option<&str>) -> Result<PathBuf, String> {
    let tmp = mktemp_dir("psht-release")?;
    let archive_path = tmp.join("asset");
    let extract_dir = tmp.join("extract");
    fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("failed to create {}: {e}", extract_dir.display()))?;

    let fmt = detect_archive_format(url)?;
    download_url_to_file(url, &archive_path)?;
    extract_archive(&archive_path, &extract_dir, fmt)?;
    let binary = resolve_binary_from_archive(&extract_dir, bin)?;
    let staged = stage_binary_dir_with_start(&binary, start);
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

fn ensure_no_release_conflicts(
    file_cfg: &ProjectConfig,
    cli_cfg: &ProjectConfig,
) -> Result<(), String> {
    let mut conflicts = Vec::new();
    push_conflict(&mut conflicts, "url", &file_cfg.url, &cli_cfg.url);
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

fn deploy_from_release_config(host: &str, cfg: &ProjectConfig) -> Result<(), String> {
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

    let staging = stage_binary_from_url(url, start, cfg.bin.as_deref())?;
    let result = deploy_from_dir(host, &app, &staging, false);
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
) -> Result<(), String> {
    let config_path = project_config_path(cwd);
    let file_cfg = load_project_config(&config_path)?;
    let cli_cfg = cli_release_settings(url, start, app_flag, bin);

    match file_cfg {
        Some(file_cfg) => {
            if file_cfg.url.is_some() {
                ensure_no_release_conflicts(&file_cfg, &cli_cfg)?;
                if app.is_some() {
                    return Err(format!(
                        "positional deploy target cannot be used when {} has `url`; set `app` in {} instead",
                        PROJECT_CONFIG_FILE, PROJECT_CONFIG_FILE
                    ));
                }
                return deploy_from_release_config(host, &file_cfg);
            }

            if has_release_settings(&cli_cfg) {
                return Err(format!(
                    "{} exists without release settings. Edit {} to add `url` and `start`.",
                    PROJECT_CONFIG_FILE, PROJECT_CONFIG_FILE
                ));
            }

            deploy_with_app_or_binary(host, cwd, app)
        }
        None => {
            if app.is_some() && !has_release_settings(&cli_cfg) {
                return deploy_with_app_or_binary(host, cwd, app);
            }

            let cfg = bootstrap_project_config(&config_path, &cli_cfg)?;
            deploy_from_release_config(host, &cfg)
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

fn setup_project_in(host: &str, cwd: &Path, config_path: &Path) -> Result<(), String> {
    if is_git_worktree(cwd) {
        let app = app_name(None, cwd);
        ensure_psht_git_remote(host, &app, cwd)?;
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
        .arg("update-cli")
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
        CliCommand::Deploy {
            app,
            url,
            start,
            app_flag,
            bin,
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
            )
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
            "url = \"https://example.com/app.tar.gz\"\nstart = \"./app\"\napp = \"demo\"\n",
        )
        .unwrap();
        let cfg = load_project_config(&path).unwrap().unwrap();
        assert_eq!(cfg.url.as_deref(), Some("https://example.com/app.tar.gz"));
        assert_eq!(cfg.start.as_deref(), Some("./app"));
        assert_eq!(cfg.app.as_deref(), Some("demo"));
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
            } => {
                assert!(app.is_none());
                assert_eq!(url.as_deref(), Some("https://example.com/app.tar.gz"));
                assert_eq!(start.as_deref(), Some("./app --port $PORT"));
                assert_eq!(app_flag.as_deref(), Some("my-app"));
                assert_eq!(bin.as_deref(), Some("dist/app"));
            }
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

        let staged = stage_binary_dir_with_start(&bin, "./mybin --debug").unwrap();
        let marker = fs::read_to_string(staged.join(".psht-start-command")).unwrap();
        assert_eq!(marker.trim(), "./mybin --debug");
        let _ = fs::remove_dir_all(staged);
    }

    #[test]
    fn stage_binary_dir_with_start_rejects_empty() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("mybin");
        fs::write(&bin, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let err = stage_binary_dir_with_start(&bin, "  ").unwrap_err();
        assert!(err.contains("start command is empty"));
    }

    #[test]
    fn deploy_with_path_errors_when_binary_missing() {
        let cwd = tempdir().unwrap();
        let err =
            deploy_with_app_or_binary("example.com", cwd.path(), Some("bin/missing")).unwrap_err();
        assert!(err.contains("binary not found"));
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
    fn ensure_no_release_conflicts_detects_mismatch() {
        let file = ProjectConfig {
            url: Some("https://a".to_string()),
            start: Some("./app".to_string()),
            app: Some("myapp".to_string()),
            bin: None,
        };
        let cli = ProjectConfig {
            url: Some("https://b".to_string()),
            start: Some("./app".to_string()),
            app: None,
            bin: None,
        };
        let err = ensure_no_release_conflicts(&file, &cli).unwrap_err();
        assert!(err.contains("url"));
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
