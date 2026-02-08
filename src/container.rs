use serde::Deserialize;
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
        let status = self.build()
            .status()
            .map_err(|e| format!("failed to run incus {args_display}: {e}"))?;
        if !status.success() {
            return Err(format!("incus {args_display} failed"));
        }
        Ok(())
    }

    fn output(self) -> Result<String, String> {
        let args_display = self.args.join(" ");
        let output = self.build()
            .output()
            .map_err(|e| format!("failed to run incus {args_display}: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("incus {args_display} failed: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn incus() -> IncusCommand {
    IncusCommand::new()
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
    let stdin = tar.stdout.take()
        .ok_or_else(|| "failed to capture tar stdout".to_string())?;
    let status = Command::new("incus")
        .arg("exec")
        .arg(container_name(app))
        .args(["--", "tar", "xf", "-", "-C", "/app"])
        .stdin(stdin)
        .status()
        .map_err(|e| format!("failed to push code to container: {e}"))?;
    if !status.success() {
        return Err("failed to push code to container".to_string());
    }
    tar.wait().map_err(|e| format!("tar failed: {e}"))?;
    Ok(())
}

pub fn exec_cmd(app: &str, cmd: &str) -> Result<(), String> {
    incus()
        .arg("exec")
        .arg(container_name(app))
        .args(&["--", "sh", "-c", cmd])
        .run()
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

pub fn delete(app: &str) -> Result<(), String> {
    incus().arg("delete").arg(container_name(app)).run()
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    let cmd = if follow { "tail -f /app/app.log" } else { "cat /app/app.log" };
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
            .args(&["--", "sh", "-c", "cat /app/app.log"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["exec", "psht-myapp", "--", "sh", "-c", "cat /app/app.log"]
        );
    }

    #[test]
    fn incus_logs_follow_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .arg("exec")
            .arg(&name)
            .args(&["--", "sh", "-c", "tail -f /app/app.log"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["exec", "psht-myapp", "--", "sh", "-c", "tail -f /app/app.log"]
        );
    }

    #[test]
    fn incus_command_builds_correct_args() {
        let cmd = incus()
            .args(&["launch", "images:ubuntu/24.04"])
            .arg("psht-myapp")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["launch", "images:ubuntu/24.04", "psht-myapp"]
        );
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
            vec!["exec", "psht-myapp", "--", "sh", "-c", "cat /etc/psht-setup-hash"]
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
