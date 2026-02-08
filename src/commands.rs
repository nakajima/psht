use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::container;
use crate::detect;

fn builds_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string());
    PathBuf::from(home).join("builds")
}

fn repos_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string());
    PathBuf::from(home).join("repos")
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

    // 1. Check out code
    eprintln!("-----> Checking out code");
    let build_dir = checkout_code(app)?;

    // 2. Detect app type
    eprintln!("-----> Detecting app type");
    let config = detect::detect(&build_dir)?;
    eprintln!("       Detected: {:?}", config.app_type);

    // 3. Tear down existing container if present
    if container::exists(app) {
        eprintln!("-----> Removing existing container");
        let _ = container::stop(app);
        let _ = container::delete(app);
    }

    // 4. Create new container
    eprintln!("-----> Creating container");
    container::create(app)?;

    // 5. Push code into container
    eprintln!("-----> Pushing code to container");
    container::push_code(app, &build_dir.to_string_lossy())?;

    // 6. Install deps + build
    let install_cmd = config.install_command();
    if !install_cmd.is_empty() {
        eprintln!("-----> Installing dependencies");
        container::exec_cmd(app, install_cmd)?;
    }

    // 7. Allocate port and set up proxy
    let port = allocate_port(app);
    eprintln!("-----> Setting up port forwarding on :{port}");
    container::add_proxy(app, port, port)?;

    // 8. Start the app
    eprintln!("-----> Starting app");
    let start_cmd = format!(
        "cd /app && PORT={port} nohup {cmd} > /app/app.log 2>&1 &",
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

pub fn logs(app: &str) -> Result<(), String> {
    container::logs(app)
}

fn help_text(hostname: &str) -> String {
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";
    let prefix = format!("ssh psht@{hostname} ");
    let commands: &[(&str, &str, &str)] = &[
        ("setup", " | sh", "Set up a git remote for deployment"),
        ("ps", "", "List running apps"),
        ("logs", " <app>", "Show app logs"),
        ("stop", " <app>", "Stop and remove an app"),
    ];

    let mut lines = vec![
        "psht - deploy apps with git push".to_string(),
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
    lines.join("\n")
}

pub fn help() -> Result<(), String> {
    eprintln!("{}", help_text(&hostname()));
    Ok(())
}

fn setup_script(hostname: &str) -> String {
    let url = format!("psht@{hostname}:$(basename $PWD)");
    [
        "git remote remove psht 2>/dev/null",
        &format!("git remote add psht {url}"),
        r#"echo "Ready! Deploy with: git push psht main" >&2"#,
    ]
    .join("\n")
}

pub fn setup() -> Result<(), String> {
    let hostname = hostname();
    eprintln!("{}", help_text(&hostname));
    println!("{}", setup_script(&hostname));
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn stop(app: &str) -> Result<(), String> {
    eprintln!("-----> Stopping {app}");
    container::stop(app)?;
    container::delete(app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn setup_script_is_idempotent() {
        let script = setup_script("example.com");
        // Must remove existing remote before adding, so re-running doesn't error
        assert!(
            script.contains("git remote remove psht"),
            "script should remove existing remote first"
        );
    }

    #[test]
    fn setup_script_prints_next_step() {
        let script = setup_script("example.com");
        assert!(
            script.contains("git push psht"),
            "script should tell user how to deploy"
        );
    }

    #[test]
    fn setup_script_has_no_help_text() {
        let script = setup_script("example.com");
        assert!(
            !script.contains("Commands:"),
            "script should not contain help text — help goes to stderr"
        );
    }

    #[test]
    fn help_text_contains_all_commands() {
        let text = help_text("example.com");
        // Strip ANSI codes for content assertions
        let plain: String = text
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert!(plain.contains("ssh psht@example.com setup"), "missing setup");
        assert!(plain.contains("ssh psht@example.com ps"), "missing ps");
        assert!(plain.contains("ssh psht@example.com logs <app>"), "missing logs");
        assert!(plain.contains("ssh psht@example.com stop <app>"), "missing stop");
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
}
