use serde::Deserialize;
use std::io::BufRead;
use std::process::{Command, Stdio};

fn container_name(app: &str) -> String {
    format!("psht-{app}")
}

#[derive(Debug, Deserialize)]
pub struct ContainerInfo {
    pub name: String,
    pub status: String,
}

pub struct IncusCommand {
    args: Vec<String>,
}

impl IncusCommand {
    fn new() -> Self {
        Self { args: Vec::new() }
    }

    fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    fn args(mut self, args: &[&str]) -> Self {
        self.args.extend(args.iter().map(|s| s.to_string()));
        self
    }

    fn build(self) -> Command {
        let mut cmd = Command::new("incus");
        cmd.args(&self.args);
        cmd
    }

    fn run(self) -> Result<(), String> {
        let args_display = self.args.join(" ");
        let status = self
            .build()
            .status()
            .map_err(|e| format!("failed to run incus {args_display}: {e}"))?;
        if !status.success() {
            return Err(format!("incus {args_display} failed"));
        }
        Ok(())
    }

    fn output(self) -> Result<String, String> {
        let args_display = self.args.join(" ");
        let output = self
            .build()
            .output()
            .map_err(|e| format!("failed to run incus {args_display}: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("incus {args_display} failed: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn run_rolling(self, window: usize) -> Result<(), String> {
        let window = window.max(1);
        let args_display = self.args.join(" ");
        let mut child = self
            .build()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run incus {args_display}: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture stderr".to_string())?;

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx2 = tx.clone();

        let stdout_thread = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                if let Ok(line) = line {
                    let _ = tx.send(line);
                }
            }
        });

        let stderr_thread = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines() {
                if let Ok(line) = line {
                    let _ = tx2.send(line);
                }
            }
        });

        let term_width = 80;
        let mut ring: Vec<String> = Vec::new();
        let mut drawn_lines = 0usize;

        for line in rx {
            update_ring(&mut ring, line, window);

            // Move cursor up to erase previous window
            if drawn_lines > 0 {
                eprint!("\x1b[{}A", drawn_lines);
            }

            // Redraw each line in the window
            for entry in &ring {
                let display = truncate_line(entry, term_width);
                eprint!("\x1b[2K  \x1b[2m{display}\x1b[0m\r\n");
            }
            drawn_lines = ring.len();
        }

        stdout_thread
            .join()
            .map_err(|_| "stdout reader panicked".to_string())?;
        stderr_thread
            .join()
            .map_err(|_| "stderr reader panicked".to_string())?;

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for incus {args_display}: {e}"))?;

        if status.success() {
            // Erase the window on success
            if drawn_lines > 0 {
                eprint!("\x1b[{}A", drawn_lines);
                for _ in 0..drawn_lines {
                    eprint!("\x1b[2K\r\n");
                }
                eprint!("\x1b[{}A", drawn_lines);
            }
        }
        // On failure, leave the last lines visible for debugging

        if !status.success() {
            return Err(format!("incus {args_display} failed"));
        }
        Ok(())
    }
}

fn incus() -> IncusCommand {
    IncusCommand::new()
}

fn update_ring(ring: &mut Vec<String>, line: String, capacity: usize) {
    if capacity == 0 {
        return;
    }
    if ring.len() >= capacity {
        ring.remove(0);
    }
    ring.push(line);
}

fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_string();
    }
    let mut end = max;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_string()
}

pub fn create(app: &str) -> Result<(), String> {
    incus()
        .args(&["launch", "images:ubuntu/24.04"])
        .arg(container_name(app))
        .run()
}

pub fn push_code(app: &str, source_dir: &str) -> Result<(), String> {
    exec_cmd(app, "rm -rf /app && mkdir -p /app")?;
    let mut tar = Command::new("tar")
        .args(["-C", source_dir, "-cf", "-", "."])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to tar code: {e}"))?;
    let stdin = tar
        .stdout
        .take()
        .ok_or_else(|| "failed to capture tar stdout".to_string())?;
    let status = Command::new("incus")
        .arg("exec")
        .arg(container_name(app))
        .args(["--", "tar", "xf", "-", "-C", "/app"])
        .stdin(stdin)
        .status()
        .map_err(|e| format!("failed to push code to container: {e}"))?;
    let tar_status = tar.wait().map_err(|e| format!("tar failed: {e}"))?;
    if !status.success() {
        return Err("failed to push code to container".to_string());
    }
    if !tar_status.success() {
        return Err(format!("tar failed with status {tar_status}"));
    }
    Ok(())
}

pub fn exec_cmd(app: &str, cmd: &str) -> Result<(), String> {
    incus()
        .arg("exec")
        .arg(container_name(app))
        .args(&["--", "sh", "-c", cmd])
        .run()
}

pub fn exec_cmd_rolling(app: &str, cmd: &str, window: usize) -> Result<(), String> {
    incus()
        .arg("exec")
        .arg(container_name(app))
        .args(&["--", "sh", "-c", cmd])
        .run_rolling(window)
}

pub fn exec_output(app: &str, cmd: &str) -> Result<String, String> {
    incus()
        .arg("exec")
        .arg(container_name(app))
        .args(&["--", "sh", "-c", cmd])
        .output()
}

pub fn push_file(app: &str, local_path: &str, remote_path: &str) -> Result<(), String> {
    incus()
        .args(&["file", "push"])
        .arg(local_path)
        .arg(format!("{}{}", container_name(app), remote_path))
        .run()
}

pub fn add_proxy(app: &str, host_port: u16, container_port: u16) -> Result<(), String> {
    incus()
        .args(&["config", "device", "add"])
        .arg(container_name(app))
        .arg("port")
        .arg("proxy")
        .arg(format!("listen=tcp:0.0.0.0:{host_port}"))
        .arg(format!("connect=tcp:127.0.0.1:{container_port}"))
        .run()
}

pub fn stop(app: &str) -> Result<(), String> {
    incus().arg("stop").arg(container_name(app)).run()
}

pub fn start(app: &str) -> Result<(), String> {
    incus().arg("start").arg(container_name(app)).run()
}

pub fn delete(app: &str) -> Result<(), String> {
    incus().arg("delete").arg(container_name(app)).run()
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    let cmd = if follow {
        "tail -f /var/psht/app.log"
    } else {
        "cat /var/psht/app.log"
    };
    incus()
        .arg("exec")
        .arg(container_name(app))
        .args(&["--", "sh", "-c", cmd])
        .run()
}

pub fn list() -> Result<Vec<ContainerInfo>, String> {
    let output = incus().args(&["list", "--format=json"]).output()?;
    let all: Vec<ContainerInfo> =
        serde_json::from_str(&output).map_err(|e| format!("failed to parse incus list: {e}"))?;
    Ok(all
        .into_iter()
        .filter(|c| c.name.starts_with("psht-"))
        .collect())
}

fn stack_image_alias(stack: &str, hash: &str) -> String {
    format!("psht-stack-{stack}-{hash}")
}

pub fn image_exists(stack: &str, hash: &str) -> bool {
    let alias = stack_image_alias(stack, hash);
    incus()
        .args(&["image", "info"])
        .arg(alias)
        .build()
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn create_from_image(app: &str, stack: &str, hash: &str) -> Result<(), String> {
    let alias = stack_image_alias(stack, hash);
    incus()
        .arg("launch")
        .arg(alias)
        .arg(container_name(app))
        .run()
}

pub fn publish_image(app: &str, stack: &str, hash: &str) -> Result<(), String> {
    let alias = stack_image_alias(stack, hash);
    let name = container_name(app);
    incus().arg("stop").arg(&name).run()?;
    incus()
        .arg("publish")
        .arg(&name)
        .arg("--alias")
        .arg(alias)
        .run()?;
    incus().arg("start").arg(&name).run()
}

pub fn exists(app: &str) -> bool {
    incus()
        .arg("info")
        .arg(container_name(app))
        .build()
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_format() {
        assert_eq!(container_name("myapp"), "psht-myapp");
        assert_eq!(container_name("test-app"), "psht-test-app");
    }

    #[test]
    fn incus_logs_cat_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", "cat /var/psht/app.log"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "exec",
                "psht-myapp",
                "--",
                "sh",
                "-c",
                "cat /var/psht/app.log"
            ]
        );
    }

    #[test]
    fn incus_logs_follow_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", "tail -f /var/psht/app.log"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "exec",
                "psht-myapp",
                "--",
                "sh",
                "-c",
                "tail -f /var/psht/app.log"
            ]
        );
    }

    #[test]
    fn incus_command_builds_correct_args() {
        let cmd = incus()
            .args(&["launch", "images:ubuntu/24.04"])
            .arg("psht-myapp")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["launch", "images:ubuntu/24.04", "psht-myapp"]);
        assert_eq!(cmd.get_program(), "incus");
    }

    #[test]
    fn incus_exec_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd_str = "npm install";
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", cmd_str])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["exec", "psht-myapp", "--", "sh", "-c", "npm install"]
        );
    }

    #[test]
    fn incus_proxy_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "add"])
            .arg(&name)
            .arg("port")
            .arg("proxy")
            .arg(format!("listen=tcp:0.0.0.0:{}", 3001))
            .arg(format!("connect=tcp:127.0.0.1:{}", 3001))
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "add",
                "psht-myapp",
                "port",
                "proxy",
                "listen=tcp:0.0.0.0:3001",
                "connect=tcp:127.0.0.1:3001"
            ]
        );
    }

    #[test]
    fn incus_push_code_exec_builds_correctly() {
        // push_code uses tar | incus exec to untar into /app
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "tar", "xf", "-", "-C", "/app"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["exec", "psht-myapp", "--", "tar", "xf", "-", "-C", "/app"]
        );
    }

    #[test]
    fn incus_exec_output_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", "cat /etc/psht-setup-hash"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "exec",
                "psht-myapp",
                "--",
                "sh",
                "-c",
                "cat /etc/psht-setup-hash"
            ]
        );
    }

    #[test]
    fn incus_push_file_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["file", "push"])
            .arg("/home/psht/stacks/node.sh")
            .arg(format!("{name}/tmp/setup.sh"))
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "file",
                "push",
                "/home/psht/stacks/node.sh",
                "psht-myapp/tmp/setup.sh"
            ]
        );
    }

    #[test]
    fn ring_buffer_fills_up() {
        let mut ring = Vec::new();
        update_ring(&mut ring, "a".into(), 3);
        update_ring(&mut ring, "b".into(), 3);
        update_ring(&mut ring, "c".into(), 3);
        assert_eq!(ring, vec!["a", "b", "c"]);
    }

    #[test]
    fn ring_buffer_rotates_when_full() {
        let mut ring = Vec::new();
        for line in ["a", "b", "c", "d", "e"] {
            update_ring(&mut ring, line.into(), 3);
        }
        assert_eq!(ring, vec!["c", "d", "e"]);
    }

    #[test]
    fn ring_buffer_zero_capacity_noop() {
        let mut ring = Vec::new();
        update_ring(&mut ring, "a".into(), 0);
        assert!(ring.is_empty());
    }

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello", 80), "hello");
    }

    #[test]
    fn truncate_line_long_truncated() {
        let long = "a".repeat(100);
        let result = truncate_line(&long, 10);
        assert_eq!(result.len(), 10);
        assert_eq!(result, "a".repeat(10));
    }

    #[test]
    fn stack_image_alias_format() {
        assert_eq!(
            stack_image_alias("node", "abc123"),
            "psht-stack-node-abc123"
        );
        assert_eq!(
            stack_image_alias("rust", "deadbeef01234567"),
            "psht-stack-rust-deadbeef01234567"
        );
    }

    #[test]
    fn image_exists_command_builds_correctly() {
        let alias = stack_image_alias("node", "abc123");
        let cmd = incus().args(&["image", "info"]).arg(&alias).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["image", "info", "psht-stack-node-abc123"]);
    }

    #[test]
    fn create_from_image_command_builds_correctly() {
        let alias = stack_image_alias("node", "abc123");
        let name = container_name("myapp");
        let cmd = incus().arg("launch").arg(&alias).arg(&name).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["launch", "psht-stack-node-abc123", "psht-myapp"]);
    }

    #[test]
    fn publish_image_stop_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus().arg("stop").arg(&name).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["stop", "psht-myapp"]);
    }

    #[test]
    fn publish_image_publish_command_builds_correctly() {
        let name = container_name("myapp");
        let alias = stack_image_alias("node", "abc123");
        let cmd = incus()
            .arg("publish")
            .arg(&name)
            .arg("--alias")
            .arg(&alias)
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["publish", "psht-myapp", "--alias", "psht-stack-node-abc123"]
        );
    }

    #[test]
    fn publish_image_start_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus().arg("start").arg(&name).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["start", "psht-myapp"]);
    }

    #[test]
    fn exec_rolling_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", "npm install"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["exec", "psht-myapp", "--", "sh", "-c", "npm install"]
        );
    }

    #[test]
    fn incus_start_command_builds_correctly() {
        let cmd = incus().arg("start").arg(container_name("myapp")).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["start", "psht-myapp"]);
    }

    #[test]
    fn parse_container_list() {
        let json = r#"[
            {"name": "psht-myapp", "status": "Running"},
            {"name": "other-container", "status": "Stopped"},
            {"name": "psht-webapp", "status": "Running"}
        ]"#;
        let all: Vec<ContainerInfo> = serde_json::from_str(json).unwrap();
        let filtered: Vec<_> = all
            .into_iter()
            .filter(|c| c.name.starts_with("psht-"))
            .collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "psht-myapp");
        assert_eq!(filtered[1].name, "psht-webapp");
    }
}
