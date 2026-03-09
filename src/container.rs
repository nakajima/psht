use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::deploy_log;
use crate::runtime_graph;

const CONTAINER_CREATE_TIMEOUT_SECS: u64 = 300;
const CONTAINER_CREATE_PROGRESS_SECS: u64 = 30;
const STORAGE_DEVICE_NAME: &str = "storage";
const STORAGE_MOUNT_PATH: &str = "/storage";
const TAILSCALE_STATE_DEVICE_NAME: &str = "tailscale-state";
const TAILSCALE_STATE_MOUNT_PATH: &str = "/var/lib/tailscale";
const TAILSCALE_STATE_SEED_DEVICE_NAME: &str = "tailscale-state-seed";
const TAILSCALE_STATE_SEED_MOUNT_PATH: &str = "/var/lib/psht-tailscale-state";

macro_rules! eprintln {
    () => {
        std::eprintln!()
    };
    ($($arg:tt)*) => {{
        let rendered = format!($($arg)*);
        std::eprintln!("{}", rendered);
        deploy_log::append("container", &rendered);
    }};
}

fn container_name(app: &str) -> String {
    format!("psht-{app}")
}

fn resolved_target(app: &str) -> Option<runtime_graph::InstanceRef> {
    runtime_graph::resolve_instance(app).ok()
}

fn resolved_instance_name(app: &str) -> String {
    resolved_target(app)
        .map(|target| target.instance_name)
        .unwrap_or_else(|| container_name(app))
}

fn scoped_incus_for_app(app: &str) -> IncusCommand {
    if let Some(target) = resolved_target(app) {
        return incus().arg("--project").arg(target.project.name);
    }
    incus()
}

#[derive(Debug, PartialEq)]
struct StorageDevice {
    dev_type: String,
    path: String,
    pool: String,
    source: String,
}

#[derive(Debug, Deserialize)]
pub struct ContainerInfo {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct OperationInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    class: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    status_code: i64,
    #[serde(default)]
    may_cancel: bool,
    #[serde(default)]
    url: String,
    #[serde(default)]
    resources: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct BlockingOperation {
    pub id: String,
    pub status: String,
    pub status_code: i64,
    pub class: String,
    pub description: String,
    pub may_cancel: bool,
    pub resources: Vec<String>,
    pub instance_names: Vec<String>,
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
            if let Some(last) = ring.last() {
                deploy_log::append("container", last);
            }

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
            if ring.is_empty() {
                return Err(format!("incus {args_display} failed"));
            }
            return Err(format!(
                "incus {args_display} failed\nLast command output:\n{}",
                ring.join("\n")
            ));
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

fn is_missing_device_error(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    lowered.contains("device")
        && (lowered.contains("not found") || lowered.contains("doesn't exist"))
}

fn device_value(app: &str, device: &str, key: &str) -> Result<Option<String>, String> {
    let name = resolved_instance_name(app);
    let output = scoped_incus_for_app(app)
        .args(&["config", "device", "get"])
        .arg(&name)
        .arg(device)
        .arg(key)
        .build()
        .output()
        .map_err(|e| format!("failed to run incus config device get {name} {device} {key}: {e}"))?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(value));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_missing_device_error(&stderr) {
        return Ok(None);
    }
    Err(format!(
        "incus config device get {name} {device} {key} failed: {stderr}"
    ))
}

fn disk_device(app: &str, device: &str) -> Result<Option<StorageDevice>, String> {
    let Some(path) = device_value(app, device, "path")? else {
        return Ok(None);
    };
    let dev_type = device_value(app, device, "type")?.unwrap_or_default();
    let pool = device_value(app, device, "pool")?.unwrap_or_default();
    let source = device_value(app, device, "source")?.unwrap_or_default();
    Ok(Some(StorageDevice {
        dev_type,
        path,
        pool,
        source,
    }))
}

fn ensure_disk_mount(
    app: &str,
    device_name: &str,
    mount_path: &str,
    pool: &str,
    volume: &str,
    label: &str,
) -> Result<(), String> {
    let expected = StorageDevice {
        dev_type: "disk".to_string(),
        path: mount_path.to_string(),
        pool: pool.to_string(),
        source: volume.to_string(),
    };

    if let Some(current) = disk_device(app, device_name)? {
        if current == expected {
            return Ok(());
        }
        return Err(format!(
            "container '{app}' has conflicting {label} device config: expected type=disk path={mount_path} pool={pool} source={volume}, got type={} path={} pool={} source={}",
            current.dev_type, current.path, current.pool, current.source
        ));
    }

    let name = container_name(app);
    scoped_incus_for_app(app)
        .args(&["config", "device", "add"])
        .arg(&name)
        .arg(device_name)
        .arg("disk")
        .arg(format!("path={mount_path}"))
        .arg(format!("pool={pool}"))
        .arg(format!("source={volume}"))
        .run()?;

    if let Some(current) = disk_device(app, device_name)? {
        if current == expected {
            return Ok(());
        }
    }
    Err(format!(
        "failed to verify {label} device for container '{app}' after attaching volume '{volume}'"
    ))
}

fn remove_disk_mount(app: &str, device_name: &str, label: &str) -> Result<(), String> {
    let name = resolved_instance_name(app);
    let output = scoped_incus_for_app(app)
        .args(&["config", "device", "remove"])
        .arg(&name)
        .arg(device_name)
        .build()
        .output()
        .map_err(|e| {
            format!("failed to run incus config device remove {name} {device_name}: {e}")
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_missing_device_error(&stderr) {
        return Ok(());
    }
    Err(format!(
        "incus config device remove {name} {device_name} ({label}) failed: {stderr}"
    ))
}

pub fn ensure_storage_mount(app: &str, pool: &str, volume: &str) -> Result<(), String> {
    ensure_disk_mount(
        app,
        STORAGE_DEVICE_NAME,
        STORAGE_MOUNT_PATH,
        pool,
        volume,
        "storage",
    )
}

pub fn remove_storage_mount(app: &str) -> Result<(), String> {
    remove_disk_mount(app, STORAGE_DEVICE_NAME, "storage")
}

pub fn has_tailscale_state_mount(app: &str, pool: &str, volume: &str) -> Result<bool, String> {
    let expected = StorageDevice {
        dev_type: "disk".to_string(),
        path: TAILSCALE_STATE_MOUNT_PATH.to_string(),
        pool: pool.to_string(),
        source: volume.to_string(),
    };
    let current = disk_device(app, TAILSCALE_STATE_DEVICE_NAME)?;
    Ok(current.is_some_and(|value| value == expected))
}

pub fn ensure_tailscale_state_mount(app: &str, pool: &str, volume: &str) -> Result<(), String> {
    ensure_disk_mount(
        app,
        TAILSCALE_STATE_DEVICE_NAME,
        TAILSCALE_STATE_MOUNT_PATH,
        pool,
        volume,
        "tailscale-state",
    )
}

pub fn remove_tailscale_state_mount(app: &str) -> Result<(), String> {
    remove_disk_mount(app, TAILSCALE_STATE_DEVICE_NAME, "tailscale-state")
}

pub fn ensure_tailscale_state_seed_mount(
    app: &str,
    pool: &str,
    volume: &str,
) -> Result<(), String> {
    ensure_disk_mount(
        app,
        TAILSCALE_STATE_SEED_DEVICE_NAME,
        TAILSCALE_STATE_SEED_MOUNT_PATH,
        pool,
        volume,
        "tailscale-state-seed",
    )
}

pub fn remove_tailscale_state_seed_mount(app: &str) -> Result<(), String> {
    remove_disk_mount(
        app,
        TAILSCALE_STATE_SEED_DEVICE_NAME,
        "tailscale-state-seed",
    )
}

fn launch_with_project(name: &str, image: &str, project: Option<&str>) -> Result<(), String> {
    let mut command = Command::new("incus");
    if let Some(project) = project {
        command.arg("--project").arg(project);
    }
    let mut child = command
        .args(["launch", image])
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run incus launch {image} {name}: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture incus launch stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture incus launch stderr".to_string())?;

    let stdout_thread = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if let Ok(line) = line {
                if line.trim().is_empty() {
                    continue;
                }
                std::println!("{line}");
                deploy_log::append("container", &line);
            }
        }
    });
    let stderr_thread = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            if let Ok(line) = line {
                if line.trim().is_empty() {
                    continue;
                }
                eprintln!("{line}");
            }
        }
    });

    let started_at = Instant::now();
    let mut next_progress = CONTAINER_CREATE_PROGRESS_SECS;

    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("failed to wait for incus launch {image} {name}: {e}"))?
        {
            stdout_thread
                .join()
                .map_err(|_| "incus launch stdout reader panicked".to_string())?;
            stderr_thread
                .join()
                .map_err(|_| "incus launch stderr reader panicked".to_string())?;
            if status.success() {
                return Ok(());
            }
            return Err(format!("incus launch {image} {name} failed"));
        }

        let elapsed = started_at.elapsed().as_secs();
        if elapsed >= CONTAINER_CREATE_TIMEOUT_SECS {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            let project_hint = project
                .map(|p| format!("--project {p} "))
                .unwrap_or_default();
            return Err(format!(
                "incus launch timed out after {}s while creating '{name}' from '{image}'. Repro: sudo -u psht incus {}launch {} {}",
                CONTAINER_CREATE_TIMEOUT_SECS, project_hint, image, name
            ));
        }

        if elapsed >= next_progress {
            eprintln!("       Still creating container... ({elapsed}s elapsed)");
            next_progress += CONTAINER_CREATE_PROGRESS_SECS;
        }

        std::thread::sleep(Duration::from_secs(1));
    }
}

fn create_with_project(app: &str, project: Option<&str>) -> Result<(), String> {
    let name = container_name(app);
    launch_with_project(&name, "images:ubuntu/24.04", project)
}

#[allow(dead_code)]
pub fn create(app: &str) -> Result<(), String> {
    create_with_project(app, None)
}

pub fn create_in_project(app: &str, project: &str) -> Result<(), String> {
    create_with_project(app, Some(project))
}

pub fn push_code(app: &str, source_dir: &str) -> Result<(), String> {
    exec_cmd(app, "rm -rf /app && mkdir -p /app")?;
    let name = resolved_instance_name(app);
    let mut push_command = Command::new("incus");
    if let Some(target) = resolved_target(app) {
        push_command.arg("--project").arg(target.project.name);
    }
    let mut tar = Command::new("tar")
        .args(["-C", source_dir, "-cf", "-", "."])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to tar code: {e}"))?;
    let stdin = tar
        .stdout
        .take()
        .ok_or_else(|| "failed to capture tar stdout".to_string())?;
    let status = push_command
        .arg("exec")
        .arg(&name)
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
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .arg("exec")
        .arg(&name)
        .args(&["--force-noninteractive", "--", "sh", "-c", cmd])
        .run()
}

pub fn exec_cmd_rolling(app: &str, cmd: &str, window: usize) -> Result<(), String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .arg("exec")
        .arg(&name)
        .args(&["--force-noninteractive", "--", "sh", "-c", cmd])
        .run_rolling(window)
}

pub fn exec_output(app: &str, cmd: &str) -> Result<String, String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .arg("exec")
        .arg(&name)
        .args(&["--force-noninteractive", "--", "sh", "-c", cmd])
        .output()
}

pub fn push_file(app: &str, local_path: &str, remote_path: &str) -> Result<(), String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .args(&["file", "push"])
        .arg(local_path)
        .arg(format!("{}{}", name, remote_path))
        .run()
}

pub fn add_proxy_in_project(
    instance_name: &str,
    host_port: u16,
    container_port: u16,
    project: &str,
) -> Result<(), String> {
    incus()
        .arg("--project")
        .arg(project)
        .args(&["config", "device", "add"])
        .arg(instance_name)
        .arg("port")
        .arg("proxy")
        .arg(format!("listen=tcp:0.0.0.0:{host_port}"))
        .arg(format!("connect=tcp:127.0.0.1:{container_port}"))
        .run()
}

pub fn remove_proxy_in_project(instance_name: &str, project: &str) -> Result<(), String> {
    let output = incus()
        .arg("--project")
        .arg(project)
        .args(&["config", "device", "remove"])
        .arg(instance_name)
        .arg("port")
        .build()
        .output()
        .map_err(|e| {
            format!(
                "failed to run incus --project {project} config device remove {instance_name} port: {e}"
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_missing_device_error(&stderr) {
        return Ok(());
    }
    Err(format!(
        "incus --project {project} config device remove {instance_name} port failed: {stderr}"
    ))
}

pub fn rename_instance_in_project(
    old_instance_name: &str,
    new_instance_name: &str,
    project: &str,
) -> Result<(), String> {
    incus()
        .arg("--project")
        .arg(project)
        .arg("rename")
        .arg(old_instance_name)
        .arg(new_instance_name)
        .run()
}

fn proxy_device_listen_value(device: &Value) -> Option<&str> {
    let Value::Object(map) = device else {
        return None;
    };
    let dev_type = map.get("type").and_then(Value::as_str).unwrap_or("");
    if !dev_type.eq_ignore_ascii_case("proxy") {
        return None;
    }
    map.get("listen").and_then(Value::as_str).map(str::trim)
}

fn listen_value_matches_port(listen: &str, host_port: u16) -> bool {
    let listen = listen.trim();
    if !listen.starts_with("tcp:") {
        return false;
    }
    listen
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        == Some(host_port)
}

fn proxy_port_owners_from_list_json(
    list_json: &str,
    host_port: u16,
) -> Result<Vec<String>, String> {
    let instances: Vec<Value> = serde_json::from_str(list_json)
        .map_err(|e| format!("failed to parse incus list for proxy owner detection: {e}"))?;
    let mut owners = BTreeSet::new();
    for instance in instances {
        let Value::Object(obj) = instance else {
            continue;
        };
        let Some(name) = obj.get("name").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        for key in ["expanded_devices", "devices"] {
            let Some(Value::Object(devices)) = obj.get(key) else {
                continue;
            };
            for device in devices.values() {
                let Some(listen) = proxy_device_listen_value(device) else {
                    continue;
                };
                if listen_value_matches_port(listen, host_port) {
                    owners.insert(name.to_string());
                }
            }
        }
    }
    Ok(owners.into_iter().collect())
}

pub fn proxy_port_owners_in_project(project: &str, host_port: u16) -> Result<Vec<String>, String> {
    let output = incus()
        .arg("--project")
        .arg(project)
        .args(&["list", "--format=json"])
        .output()?;
    proxy_port_owners_from_list_json(&output, host_port)
}

#[allow(dead_code)]
pub fn rename_app(old_app: &str, new_app: &str) -> Result<(), String> {
    let old_name = container_name(old_app);
    let new_name = container_name(new_app);
    incus().arg("rename").arg(old_name).arg(new_name).run()
}

pub fn stop(app: &str) -> Result<(), String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app).arg("stop").arg(&name).run()
}

pub fn start(app: &str) -> Result<(), String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app).arg("start").arg(&name).run()
}

pub fn delete(app: &str) -> Result<(), String> {
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app).arg("delete").arg(&name).run()
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    let cmd = if follow {
        "tail -f /var/psht/app.log"
    } else {
        "cat /var/psht/app.log"
    };
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .arg("exec")
        .arg(&name)
        .args(&["--", "sh", "-c", cmd])
        .run()
}

pub fn list() -> Result<Vec<ContainerInfo>, String> {
    let output = runtime_graph::RuntimeProject::current()
        .map(|project| incus().arg("--project").arg(project.name))
        .unwrap_or_else(|_| incus())
        .args(&["list", "--format=json"])
        .output()?;
    let all: Vec<ContainerInfo> =
        serde_json::from_str(&output).map_err(|e| format!("failed to parse incus list: {e}"))?;
    Ok(all
        .into_iter()
        .filter(|c| c.name.starts_with("psht-"))
        .collect())
}

pub fn is_running(app: &str) -> Result<bool, String> {
    let target = container_name(app);
    let containers = list()?;
    Ok(containers
        .into_iter()
        .find(|c| c.name == target)
        .map(|c| c.status.eq_ignore_ascii_case("running"))
        .unwrap_or(false))
}

fn stack_image_alias(stack: &str, hash: &str) -> String {
    format!("psht-stack-{stack}-{hash}")
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn create_from_image(app: &str, stack: &str, hash: &str) -> Result<(), String> {
    let alias = stack_image_alias(stack, hash);
    let name = container_name(app);
    launch_with_project(&name, &alias, None)
}

#[allow(dead_code)]
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
    let name = resolved_instance_name(app);
    scoped_incus_for_app(app)
        .arg("info")
        .arg(&name)
        .build()
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn exists_instance_in_project(instance_name: &str, project: &str) -> bool {
    incus()
        .arg("--project")
        .arg(project)
        .arg("info")
        .arg(instance_name)
        .build()
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn force_stop_instance_in_project(instance_name: &str, project: &str) -> Result<(), String> {
    incus()
        .arg("--project")
        .arg(project)
        .arg("stop")
        .arg(instance_name)
        .arg("--force")
        .run()
}

pub fn delete_instance_in_project(instance_name: &str, project: &str) -> Result<(), String> {
    incus()
        .arg("--project")
        .arg(project)
        .arg("delete")
        .arg(instance_name)
        .arg("--force")
        .run()
}

pub fn exec_cmd_in_instance_project(
    instance_name: &str,
    project: &str,
    cmd: &str,
) -> Result<(), String> {
    incus()
        .arg("--project")
        .arg(project)
        .arg("exec")
        .arg(instance_name)
        .args(&["--force-noninteractive", "--", "sh", "-c", cmd])
        .run()
}

fn instance_name_from_resource(resource: &str) -> Option<String> {
    let trimmed = resource.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = trimmed
        .split('?')
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let suffix = if let Some(v) = path.strip_prefix("/1.0/instances/") {
        v
    } else if let Some(v) = path.strip_prefix("/1.0/containers/") {
        v
    } else {
        return None;
    };
    let name = suffix.split('/').next().unwrap_or("").trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn operation_resource_matches_container(resource: &str, container_name: &str) -> bool {
    matches!(
        instance_name_from_resource(resource).as_deref(),
        Some(name) if name == container_name
    )
}

fn operation_id(op: &OperationInfo) -> Option<String> {
    if !op.id.trim().is_empty() {
        return Some(op.id.trim().to_string());
    }
    let from_url = op
        .url
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if from_url.is_empty() {
        None
    } else {
        Some(from_url.to_string())
    }
}

fn blocking_operations_in(
    operations_json: &str,
    container_name: &str,
) -> Result<Vec<BlockingOperation>, String> {
    let ops: Vec<OperationInfo> = serde_json::from_str(operations_json)
        .map_err(|e| format!("failed to parse incus operation list: {e}"))?;
    let mut blocking = Vec::new();
    for op in ops {
        let running = op.status.eq_ignore_ascii_case("running") || op.status_code == 103;
        if !running {
            continue;
        }
        let mut matched_resources = Vec::new();
        let mut matched_instances = BTreeSet::new();
        for resources in op.resources.values() {
            for resource in resources {
                if operation_resource_matches_container(resource, container_name) {
                    matched_resources.push(resource.to_string());
                    if let Some(instance_name) = instance_name_from_resource(resource) {
                        matched_instances.insert(instance_name);
                    }
                }
            }
        }
        if matched_resources.is_empty() {
            continue;
        }
        let id = operation_id(&op).unwrap_or_else(|| {
            format!(
                "unknown-{}-{}",
                op.class.trim().to_lowercase(),
                op.status_code
            )
        });
        blocking.push(BlockingOperation {
            id,
            status: op.status,
            status_code: op.status_code,
            class: op.class,
            description: op.description,
            may_cancel: op.may_cancel,
            resources: matched_resources,
            instance_names: matched_instances.into_iter().collect(),
        });
    }
    Ok(blocking)
}

fn has_running_operation_in(operations_json: &str, container_name: &str) -> Result<bool, String> {
    Ok(!blocking_operations_in(operations_json, container_name)?.is_empty())
}

#[allow(dead_code)]
pub fn has_running_operation(app: &str) -> Result<bool, String> {
    let name = container_name(app);
    let operations = incus()
        .args(&["operation", "list", "--format=json"])
        .output()?;
    has_running_operation_in(&operations, &name)
}

pub fn list_blocking_operations_in_project(
    app: &str,
    project: &str,
) -> Result<Vec<BlockingOperation>, String> {
    let name = container_name(app);
    let operations = incus()
        .arg("--project")
        .arg(project)
        .args(&["operation", "list", "--format=json"])
        .output()?;
    blocking_operations_in(&operations, &name)
}

pub fn cancel_operation_in_project(project: &str, operation_id: &str) -> Result<(), String> {
    let id = operation_id.trim();
    if id.is_empty() {
        return Err("cannot cancel operation with empty id".to_string());
    }
    incus()
        .arg("--project")
        .arg(project)
        .arg("operation")
        .arg("cancel")
        .arg(id)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{self, AppRuntimeState};

    fn unique_app(prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            crate::sqlite_store::next_app_generation(prefix).unwrap_or(1)
        )
    }

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
    fn scoped_instance_commands_include_runtime_project_from_model() {
        let app = unique_app("container-runtime-project");
        control_plane::write_app_runtime_state(
            &app,
            &AppRuntimeState {
                active_instance: "psht-demo".to_string(),
                previous_instance: None,
                runtime_project: Some("user-4242".to_string()),
                updated_at: 1,
            },
        )
        .unwrap();

        let cmd = scoped_incus_for_app("demo")
            .arg("exec")
            .arg(resolved_instance_name("demo"))
            .args(&["--", "sh", "-c", "true"])
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "--project",
                "user-4242",
                "exec",
                "psht-demo",
                "--",
                "sh",
                "-c",
                "true"
            ]
        );

        control_plane::clear_app_runtime_state(&app).unwrap();
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
    fn incus_proxy_remove_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "remove"])
            .arg(&name)
            .arg("port")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["config", "device", "remove", "psht-myapp", "port"]
        );
    }

    #[test]
    fn incus_rename_command_builds_correctly() {
        let old_name = container_name("myapp");
        let new_name = container_name("myapp-next");
        let cmd = incus().arg("rename").arg(&old_name).arg(&new_name).build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec!["rename", "psht-myapp", "psht-myapp-next"]);
    }

    #[test]
    fn incus_project_rename_command_builds_correctly() {
        let cmd = incus()
            .arg("--project")
            .arg("user-1000")
            .arg("rename")
            .arg("psht-myapp-build-1")
            .arg("psht-myapp-failed-1")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "--project",
                "user-1000",
                "rename",
                "psht-myapp-build-1",
                "psht-myapp-failed-1"
            ]
        );
    }

    #[test]
    fn incus_storage_add_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "add"])
            .arg(&name)
            .arg("storage")
            .arg("disk")
            .arg("path=/storage")
            .arg("pool=default")
            .arg("source=psht-storage-myapp")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "add",
                "psht-myapp",
                "storage",
                "disk",
                "path=/storage",
                "pool=default",
                "source=psht-storage-myapp"
            ]
        );
    }

    #[test]
    fn incus_storage_remove_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "remove"])
            .arg(&name)
            .arg("storage")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec!["config", "device", "remove", "psht-myapp", "storage"]
        );
    }

    #[test]
    fn incus_tailscale_state_add_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "add"])
            .arg(&name)
            .arg(TAILSCALE_STATE_DEVICE_NAME)
            .arg("disk")
            .arg(format!("path={TAILSCALE_STATE_MOUNT_PATH}"))
            .arg("pool=default")
            .arg("source=psht-tailscale-myapp")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "add",
                "psht-myapp",
                "tailscale-state",
                "disk",
                "path=/var/lib/tailscale",
                "pool=default",
                "source=psht-tailscale-myapp"
            ]
        );
    }

    #[test]
    fn incus_tailscale_state_remove_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "remove"])
            .arg(&name)
            .arg(TAILSCALE_STATE_DEVICE_NAME)
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "remove",
                "psht-myapp",
                "tailscale-state"
            ]
        );
    }

    #[test]
    fn incus_tailscale_seed_add_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "add"])
            .arg(&name)
            .arg(TAILSCALE_STATE_SEED_DEVICE_NAME)
            .arg("disk")
            .arg(format!("path={TAILSCALE_STATE_SEED_MOUNT_PATH}"))
            .arg("pool=default")
            .arg("source=psht-tailscale-myapp")
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "add",
                "psht-myapp",
                "tailscale-state-seed",
                "disk",
                "path=/var/lib/psht-tailscale-state",
                "pool=default",
                "source=psht-tailscale-myapp"
            ]
        );
    }

    #[test]
    fn incus_tailscale_seed_remove_command_builds_correctly() {
        let name = container_name("myapp");
        let cmd = incus()
            .args(&["config", "device", "remove"])
            .arg(&name)
            .arg(TAILSCALE_STATE_SEED_DEVICE_NAME)
            .build();
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                "config",
                "device",
                "remove",
                "psht-myapp",
                "tailscale-state-seed"
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
    fn operation_resource_matches_container_handles_instance_paths() {
        assert!(operation_resource_matches_container(
            "/1.0/instances/psht-demo?project=user-1001",
            "psht-demo"
        ));
        assert!(operation_resource_matches_container(
            "/1.0/containers/psht-demo",
            "psht-demo"
        ));
        assert!(!operation_resource_matches_container(
            "/1.0/instances/psht-other",
            "psht-demo"
        ));
    }

    #[test]
    fn instance_name_from_resource_extracts_instances_and_containers() {
        assert_eq!(
            instance_name_from_resource("/1.0/instances/psht-demo?project=user-1001").as_deref(),
            Some("psht-demo")
        );
        assert_eq!(
            instance_name_from_resource("/1.0/containers/psht-demo/").as_deref(),
            Some("psht-demo")
        );
        assert_eq!(
            instance_name_from_resource("/1.0/networks/br0").as_deref(),
            None
        );
    }

    #[test]
    fn proxy_port_owners_from_list_json_finds_proxy_listeners() {
        let json = r#"[
  {
    "name": "psht-hyperlinked",
    "expanded_devices": {
      "eth0": {"type":"nic"},
      "port": {
        "type":"proxy",
        "listen":"tcp:0.0.0.0:3430",
        "connect":"tcp:127.0.0.1:3430"
      }
    }
  },
  {
    "name": "psht-other",
    "devices": {
      "port": {
        "type":"proxy",
        "listen":"tcp:[::]:3430",
        "connect":"tcp:127.0.0.1:3430"
      }
    }
  },
  {
    "name": "psht-unrelated",
    "expanded_devices": {
      "port": {
        "type":"proxy",
        "listen":"tcp:0.0.0.0:3555",
        "connect":"tcp:127.0.0.1:3555"
      }
    }
  }
]"#;
        let owners = proxy_port_owners_from_list_json(json, 3430).unwrap();
        assert_eq!(owners, vec!["psht-hyperlinked", "psht-other"]);
    }

    #[test]
    fn has_running_operation_in_detects_busy_container() {
        let json = r#"[{
  "id":"op-1",
  "status":"Running",
  "status_code":103,
  "may_cancel":true,
  "resources":{"instances":["/1.0/instances/psht-demo?project=user-1001"]}
}]"#;
        let busy = has_running_operation_in(json, "psht-demo").unwrap();
        assert!(busy);
    }

    #[test]
    fn has_running_operation_in_ignores_non_running_operations() {
        let json = r#"[{
  "status":"Success",
  "status_code":200,
  "resources":{"instances":["/1.0/instances/psht-demo?project=user-1001"]}
}]"#;
        let busy = has_running_operation_in(json, "psht-demo").unwrap();
        assert!(!busy);
    }

    #[test]
    fn blocking_operations_in_extracts_id_from_url() {
        let json = r#"[{
  "url":"/1.0/operations/7f4f3e8d",
  "status":"Running",
  "status_code":103,
  "class":"task",
  "description":"Updating instance",
  "may_cancel":true,
  "resources":{"instances":["/1.0/instances/psht-demo?project=user-1001"]}
}]"#;
        let ops = blocking_operations_in(json, "psht-demo").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, "7f4f3e8d");
        assert!(ops[0].may_cancel);
        assert_eq!(ops[0].class, "task");
        assert_eq!(ops[0].instance_names, vec!["psht-demo"]);
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
