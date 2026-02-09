use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::container;
use crate::detect;
use crate::tailscale;

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn builds_dir() -> PathBuf {
    home_dir().join("builds")
}

fn repos_dir() -> PathBuf {
    home_dir().join("repos")
}

fn stacks_dir() -> PathBuf {
    home_dir().join("stacks")
}

fn stack_hash(path: &Path) -> Result<String, String> {
    let contents = fs::read(path)
        .map_err(|e| format!("failed to read stack script {}: {e}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn resolve_stack_in(app: &str, code_dir: &Path, detected_stack: &str, stacks: &Path) -> Result<(String, PathBuf), String> {
    let custom = code_dir.join("psht-stack.sh");
    if custom.exists() {
        let saved = stacks.join(format!("{app}.sh"));
        fs::copy(&custom, &saved)
            .map_err(|e| format!("failed to save custom stack: {e}"))?;
        Ok((app.to_string(), saved))
    } else {
        Ok((detected_stack.to_string(), stacks.join(format!("{detected_stack}.sh"))))
    }
}

fn resolve_stack(app: &str, code_dir: &Path, detected_stack: &str) -> Result<(String, PathBuf), String> {
    resolve_stack_in(app, code_dir, detected_stack, &stacks_dir())
}

fn checkout_code(app: &str) -> Result<PathBuf, String> {
    let build_dir = builds_dir().join(app);
    let repo_dir = repos_dir().join(format!("{app}.git"));

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("failed to clean build dir: {e}"))?;
    }
    fs::create_dir_all(&build_dir)
        .map_err(|e| format!("failed to create build dir: {e}"))?;

    let status = Command::new("git")
        .args(["clone", "--depth", "1"])
        .arg(&repo_dir)
        .arg(&build_dir)
        .status()
        .map_err(|e| format!("failed to checkout code: {e}"))?;
    if !status.success() {
        return Err("git clone failed".to_string());
    }

    Ok(build_dir)
}

fn allocate_port(app: &str) -> u16 {
    // Simple deterministic port allocation based on app name hash
    let hash: u32 = app.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    3001 + (hash % 1000) as u16
}

pub fn deploy(app: &str) -> Result<(), String> {
    eprintln!("-----> Deploying {app}");

    eprintln!("-----> Checking out code");
    let build_dir = checkout_code(app)?;

    deploy_from(app, &build_dir)
}

pub fn push(app: &str) -> Result<(), String> {
    eprintln!("-----> Deploying {app}");

    let code_dir = home_dir().join(app);

    if code_dir.exists() {
        fs::remove_dir_all(&code_dir)
            .map_err(|e| format!("failed to clean code dir: {e}"))?;
    }
    fs::create_dir_all(&code_dir)
        .map_err(|e| format!("failed to create code dir: {e}"))?;

    eprintln!("-----> Receiving code");
    let status = Command::new("tar")
        .args(["xz", "-C"])
        .arg(&code_dir)
        .stdin(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to extract tar: {e}"))?;
    if !status.success() {
        return Err("tar extraction failed".to_string());
    }

    deploy_from(app, &code_dir)
}

fn deploy_from(app: &str, code_dir: &Path) -> Result<(), String> {
    eprintln!("-----> Detecting app type");
    let config = detect::detect(code_dir)?;
    eprintln!("       Detected: {:?}", config.app_type);

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;

    let needs_setup = if container::exists(app) {
        let remote_hash = container::exec_output(app, "cat /etc/psht-setup-hash 2>/dev/null")
            .unwrap_or_default()
            .trim()
            .to_string();
        if remote_hash == hash {
            eprintln!("-----> Reusing container");
            container::exec_cmd(app, "kill $(cat /var/psht/app.pid 2>/dev/null) 2>/dev/null || true")?;
            false
        } else {
            eprintln!("-----> Rebuilding container");
            let _ = container::stop(app);
            let _ = container::delete(app);
            true
        }
    } else {
        true
    };

    if needs_setup {
        if container::image_exists(&stack, &hash) {
            eprintln!("-----> Creating container from cached image");
            container::create_from_image(app, &stack, &hash)?;

            eprintln!("-----> Installing tailscale");
            tailscale::install_in_container(app)?;
        } else {
            eprintln!("-----> Creating container");
            container::create(app)?;

            eprintln!("-----> Installing tailscale");
            tailscale::install_in_container(app)?;

            eprintln!("-----> Setting up runtime");
            container::push_file(app, &script_path.to_string_lossy(), "/tmp/setup.sh")?;
            container::exec_cmd_rolling(app, "chmod +x /tmp/setup.sh && /tmp/setup.sh", 5)?;

            eprintln!("-----> Caching stack image");
            if let Err(e) = container::publish_image(app, &stack, &hash) {
                eprintln!("       Warning: failed to cache stack image: {e}");
            }
        }

        container::exec_cmd(app, &format!("echo -n '{hash}' > /etc/psht-setup-hash"))?;

        eprintln!("-----> Connecting to tailnet");
        tailscale::join_in_container(app)?;

        let port = allocate_port(app);
        eprintln!("-----> Setting up port forwarding on :{port}");
        container::add_proxy(app, port, port)?;
    }

    eprintln!("-----> Pushing code to container");
    container::push_code(app, &code_dir.to_string_lossy())?;

    if !config.install_command.is_empty() {
        eprintln!("-----> Installing dependencies");
        container::exec_cmd_rolling(app, &config.install_command, 5)?;
    }

    let port = allocate_port(app);
    eprintln!("-----> Starting app");
    let start_cmd = format!(
        "mkdir -p /var/psht && cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
        cmd = config.start_command
    );
    container::exec_cmd(app, &start_cmd)?;

    eprintln!("=====> App {app} deployed on port {port}");
    Ok(())
}

pub fn ps() -> Result<(), String> {
    let containers = container::list()?;
    if containers.is_empty() {
        println!("No apps running.");
        return Ok(());
    }
    println!("{:<20} {:<10}", "APP", "STATUS");
    for c in &containers {
        let app = c.name.strip_prefix("psht-").unwrap_or(&c.name);
        println!("{:<20} {:<10}", app, c.status);
    }
    Ok(())
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    container::logs(app, follow)
}

fn help_text(hostname: &str) -> String {
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";
    let prefix = format!("ssh psht@{hostname} ");
    let commands: &[(&str, &str, &str)] = &[
        ("setup", " | sh", "Set up project and install CLI"),
        ("update", " | sh", "Update CLI to latest version"),
        ("ps", "", "List running apps"),
        ("logs", " [-f] <app>", "Show app logs"),
        ("stop", " <app>", "Stop an app"),
        ("start", " <app>", "Start a stopped app"),
        ("destroy", " <app>", "Stop and remove an app"),
    ];

    let mut lines = vec![
        "psht - deploy apps with psht deploy".to_string(),
        String::new(),
        "Commands:".to_string(),
    ];
    for (name, suffix, desc) in commands {
        let visible_len = name.len() + suffix.len();
        let pad = 14_usize.saturating_sub(visible_len);
        lines.push(format!(
            "  {dim}{prefix}{reset}{name}{dim}{suffix}{reset}{:pad$}  {desc}",
            "",
        ));
    }
    lines.push(String::new());
    lines.push("Deploy:".to_string());
    lines.push(format!("  psht deploy{:>21}  Deploy current directory", ""));
    lines.join("\n")
}

pub fn help() -> Result<(), String> {
    eprintln!("{}", help_text(&hostname()));
    Ok(())
}

fn setup_script(hostname: &str) -> String {
    format!(
        r#"#!/bin/sh
set -e

# Find or install psht CLI
if command -v psht >/dev/null 2>&1; then
  PSHT_BIN=$(command -v psht)
else
  printf "Install psht CLI to (default: ~/.local/bin): " >&2
  read -r install_dir < /dev/tty
  install_dir="${{install_dir:-$HOME/.local/bin}}"
  mkdir -p "$install_dir"
  scp "psht@{hostname}:bin/psht-cli" "$install_dir/psht"
  chmod +x "$install_dir/psht"
  PSHT_BIN="$install_dir/psht"
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "NOTE: Add $install_dir to your PATH: export PATH=\"$install_dir:\$PATH\"" >&2 ;;
  esac
  echo "Installed psht CLI to $PSHT_BIN" >&2
fi

# Write default host
mkdir -p "$HOME/.psht"
config="$HOME/.psht/config.toml"
if [ ! -f "$config" ]; then
  echo 'host = "{hostname}"' > "$config"
fi

# Set up project
"$PSHT_BIN" setup"#
    )
}

pub fn setup() -> Result<(), String> {
    println!("{}", setup_script(&hostname()));
    Ok(())
}

fn update_script(hostname: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"#!/bin/sh
set -e
PSHT_BIN=$(command -v psht) || {{ echo "psht not found. Run: ssh psht@{hostname} setup | sh" >&2; exit 1; }}
current=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || current=""
if [ "$current" = "{version}" ]; then
  echo "psht {version} (up to date)" >&2
  exit 0
fi
rm -f "$PSHT_BIN"
scp "psht@{hostname}:bin/psht-cli" "$PSHT_BIN"
chmod +x "$PSHT_BIN"
echo "psht {version} (updated)" >&2"#
    )
}

pub fn update() -> Result<(), String> {
    println!("{}", update_script(&hostname()));
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn stop(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Stopping {app}");
    container::stop(app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

pub fn start(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Starting {app}");
    container::start(app)?;
    eprintln!("=====> {app} started");
    Ok(())
}

pub fn destroy(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Destroying {app}");
    container::stop(app)?;
    container::delete(app)?;
    eprintln!("=====> {app} destroyed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn allocate_port_is_deterministic() {
        let port1 = allocate_port("myapp");
        let port2 = allocate_port("myapp");
        assert_eq!(port1, port2);
    }

    #[test]
    fn allocate_port_in_valid_range() {
        for name in &["app1", "app2", "webapp", "api", "test-long-name"] {
            let port = allocate_port(name);
            assert!(port >= 3001 && port < 4001, "port {port} out of range for {name}");
        }
    }

    #[test]
    fn allocate_port_different_apps_likely_differ() {
        let port1 = allocate_port("myapp");
        let port2 = allocate_port("other");
        // Not guaranteed, but very likely to differ
        assert_ne!(port1, port2);
    }

    #[test]
    fn setup_script_calls_psht_setup() {
        let script = setup_script("example.com");
        assert!(
            script.contains("\"$PSHT_BIN\" setup"),
            "script should delegate project setup to CLI via full path"
        );
    }

    #[test]
    fn setup_script_reads_from_tty() {
        let script = setup_script("example.com");
        assert!(
            script.contains("< /dev/tty"),
            "read should use /dev/tty so prompts work when piped"
        );
    }

    #[test]
    fn setup_script_tracks_binary_path() {
        let script = setup_script("example.com");
        assert!(
            script.contains("PSHT_BIN=$(command -v psht)"),
            "should capture existing binary path"
        );
        assert!(
            script.contains("PSHT_BIN=\"$install_dir/psht\""),
            "should capture newly installed binary path"
        );
    }

    #[test]
    fn setup_script_has_no_help_text() {
        let script = setup_script("example.com");
        assert!(
            !script.contains("Commands:"),
            "script should not contain help text"
        );
    }

    #[test]
    fn setup_script_installs_cli() {
        let script = setup_script("example.com");
        assert!(
            script.contains("Install psht CLI"),
            "script should install the CLI"
        );
        assert!(
            script.contains("scp \"psht@example.com:bin/psht-cli\""),
            "script should download CLI via scp"
        );
        assert!(
            script.contains("chmod +x"),
            "script should make CLI executable"
        );
    }

    #[test]
    fn setup_script_writes_default_host() {
        let script = setup_script("example.com");
        assert!(
            script.contains("host = \"example.com\""),
            "script should write default host to config"
        );
    }

    #[test]
    fn help_text_contains_all_commands() {
        let text = help_text("example.com");
        let plain: String = text
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert!(plain.contains("ssh psht@example.com setup"), "missing setup");
        assert!(plain.contains("ssh psht@example.com update"), "missing update");
        assert!(plain.contains("ssh psht@example.com ps"), "missing ps");
        assert!(plain.contains("ssh psht@example.com logs [-f] <app>"), "missing logs");
        assert!(plain.contains("ssh psht@example.com stop <app>"), "missing stop");
        assert!(plain.contains("ssh psht@example.com start <app>"), "missing start");
        assert!(plain.contains("ssh psht@example.com destroy <app>"), "missing destroy");
    }

    #[test]
    fn help_text_mentions_deploy() {
        let text = help_text("example.com");
        let plain: String = text
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert!(plain.contains("psht deploy"), "missing deploy method");
        assert!(!plain.contains("git push"), "should not mention git push");
    }

    #[test]
    fn update_script_scps_binary() {
        let script = update_script("example.com");
        assert!(
            script.contains("scp \"psht@example.com:bin/psht-cli\" \"$PSHT_BIN\""),
            "should scp the binary to the existing install path"
        );
    }

    #[test]
    fn update_script_removes_before_scp() {
        let script = update_script("example.com");
        let rm_pos = script.find("rm -f \"$PSHT_BIN\"")
            .expect("should rm the old binary to avoid ETXTBSY");
        let scp_pos = script.find("scp ")
            .expect("should scp the new binary");
        assert!(rm_pos < scp_pos, "rm must come before scp");
    }

    #[test]
    fn update_script_errors_if_not_installed() {
        let script = update_script("example.com");
        assert!(
            script.contains("psht not found"),
            "should error if psht is not installed"
        );
    }

    #[test]
    fn update_script_skips_if_up_to_date() {
        let script = update_script("example.com");
        assert!(
            script.contains("up to date"),
            "should say up to date when versions match"
        );
        assert!(
            script.contains(env!("CARGO_PKG_VERSION")),
            "should embed the current version"
        );
    }

    #[test]
    fn home_dir_under_home() {
        let dir = home_dir();
        assert!(!dir.to_string_lossy().is_empty());
    }

    #[test]
    fn builds_dir_under_home() {
        let dir = builds_dir();
        assert!(dir.to_string_lossy().ends_with("builds"));
    }

    #[test]
    fn repos_dir_under_home() {
        let dir = repos_dir();
        assert!(dir.to_string_lossy().ends_with("repos"));
    }

    #[test]
    fn stacks_dir_under_home() {
        let dir = stacks_dir();
        assert!(dir.to_string_lossy().ends_with("stacks"));
    }

    #[test]
    fn stack_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash1 = stack_hash(&script).unwrap();
        let hash2 = stack_hash(&script).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn stack_hash_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash1 = stack_hash(&script).unwrap();
        fs::write(&script, "#!/bin/sh\necho world").unwrap();
        let hash2 = stack_hash(&script).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn stack_hash_is_hex_string() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash = stack_hash(&script).unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stack_hash_errors_on_missing_file() {
        let result = stack_hash(Path::new("/nonexistent/file.sh"));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_stack_uses_custom_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();
        fs::write(code_dir.join("psht-stack.sh"), "#!/bin/sh\ncustom setup").unwrap();

        let (name, path) = resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();
        assert_eq!(name, "myapp");
        assert_eq!(path, stacks.join("myapp.sh"));
    }

    #[test]
    fn resolve_stack_falls_back_to_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();

        let (name, path) = resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();
        assert_eq!(name, "bun");
        assert_eq!(path, stacks.join("bun.sh"));
    }

    #[test]
    fn resolve_stack_saves_custom_to_stacks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();
        let content = "#!/bin/sh\napt install ffmpeg";
        fs::write(code_dir.join("psht-stack.sh"), content).unwrap();

        resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();

        let saved = fs::read_to_string(stacks.join("myapp.sh")).unwrap();
        assert_eq!(saved, content);
    }

    #[test]
    fn start_cmd_mkdir_runs_before_background() {
        // The start command must use { } grouping so mkdir && cd run synchronously
        // before the nohup is backgrounded. Without grouping, & backgrounds the
        // entire && chain, causing a race where echo runs before mkdir completes.
        let cmd = format!(
            "mkdir -p /var/psht && cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
            port = 3737,
            cmd = "bun run index.ts"
        );
        // mkdir and cd must be outside the { } group
        assert!(cmd.starts_with("mkdir -p /var/psht && cd /app && {"));
        // The background & and echo must be inside { }
        assert!(cmd.ends_with("& echo $! > /var/psht/app.pid; }"));
    }
}
