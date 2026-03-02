use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::caddy;
use crate::commands;
use crate::container;

// RAII guard: stops + deletes the container on drop, even on panic.
struct ContainerGuard {
    app: String,
}

impl ContainerGuard {
    fn new(app: &str) -> Result<Self, String> {
        container::create(app)?;
        Ok(Self {
            app: app.to_string(),
        })
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = container::stop(&self.app);
        let _ = container::delete(&self.app);
    }
}

struct StorageVolumeGuard {
    pool: String,
    volume: String,
}

impl StorageVolumeGuard {
    fn new(pool: impl Into<String>, volume: impl Into<String>) -> Self {
        Self {
            pool: pool.into(),
            volume: volume.into(),
        }
    }
}

impl Drop for StorageVolumeGuard {
    fn drop(&mut self) {
        let _ = delete_storage_volume_if_exists(&self.pool, &self.volume);
    }
}

fn exec_output(app: &str, cmd: &str) -> Result<String, String> {
    let name = format!("psht-{app}");
    let output = Command::new("incus")
        .args(["exec", &name, "--", "sh", "-c", cmd])
        .output()
        .map_err(|e| format!("exec failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("exec `{cmd}` failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn wait_for_container_network(app: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if container::exec_cmd(app, "echo ready").is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!("container {app} network not ready after 30s"))
}

fn default_storage_pool_name() -> Result<String, String> {
    let output = Command::new("incus")
        .args(["profile", "device", "get", "default", "root", "pool"])
        .output()
        .map_err(|e| format!("failed to get default root pool: {e}"))?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }

    let output = Command::new("incus")
        .args(["storage", "list", "--format=json"])
        .output()
        .map_err(|e| format!("failed to list storage pools: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to list storage pools: {}", stderr.trim()));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse storage list: {e}"))?;
    let pools = value
        .as_array()
        .ok_or_else(|| "unexpected storage list response".to_string())?;
    for pool in pools {
        if let Some(name) = pool.get("name").and_then(serde_json::Value::as_str)
            && !name.is_empty()
        {
            return Ok(name.to_string());
        }
    }
    Err("no storage pool found".to_string())
}

fn storage_volume_name(app: &str) -> String {
    format!("psht-storage-{app}")
}

fn storage_volume_exists(pool: &str, volume: &str) -> bool {
    Command::new("incus")
        .args(["storage", "volume", "show", pool, volume])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ensure_storage_volume(pool: &str, volume: &str) -> Result<(), String> {
    if storage_volume_exists(pool, volume) {
        return Ok(());
    }
    let output = Command::new("incus")
        .args(["storage", "volume", "create", pool, volume])
        .output()
        .map_err(|e| format!("failed to create storage volume {pool}/{volume}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "failed to create storage volume {pool}/{volume}: {}",
        stderr.trim()
    ))
}

fn delete_storage_volume_if_exists(pool: &str, volume: &str) -> Result<(), String> {
    if !storage_volume_exists(pool, volume) {
        return Ok(());
    }
    let output = Command::new("incus")
        .args(["storage", "volume", "delete", pool, volume])
        .output()
        .map_err(|e| format!("failed to delete storage volume {pool}/{volume}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "failed to delete storage volume {pool}/{volume}: {}",
        stderr.trim()
    ))
}

fn container_ip(app: &str) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(output) = exec_output(app, "hostname -I") {
            // Prefer IPv4; fall back to first address
            let ip = output
                .split_whitespace()
                .find(|s| s.contains('.'))
                .or_else(|| output.split_whitespace().next())
                .unwrap_or("")
                .to_string();
            if !ip.is_empty() {
                return Ok(ip);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!("could not get container IP for {app}"))
}

fn debug_info(app: &str) -> String {
    let log = exec_output(app, "cat /var/psht/app.log 2>/dev/null").unwrap_or_default();
    let ps = exec_output(app, "ps aux 2>/dev/null").unwrap_or_default();
    format!("app.log:\n{log}\n\nps aux:\n{ps}")
}

fn wait_for_http(addr: &str, port: u16, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    let connect_addr = if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    };
    while Instant::now() < deadline {
        if let Ok(mut stream) = std::net::TcpStream::connect(&connect_addr) {
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
            let request = format!("GET / HTTP/1.0\r\nHost: {addr}\r\n\r\n");
            if std::io::Write::write_all(&mut stream, request.as_bytes()).is_ok() {
                let mut response = String::new();
                if stream.read_to_string(&mut response).is_ok() && response.contains("ok") {
                    return Ok(response);
                }
                last_err = format!("response did not contain 'ok': {response}");
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(format!(
        "no HTTP response on {addr}:{port} within {timeout:?}: {last_err}"
    ))
}

fn install_runtime(app: &str, stack: &str) -> Result<(), String> {
    match stack {
        "node" => container::exec_cmd(app, "apt-get update && apt-get install -y nodejs npm"),
        "go" => container::exec_cmd(
            app,
            "apt-get update && apt-get install -y curl && curl -fsSL https://go.dev/dl/go1.23.6.linux-amd64.tar.gz | tar -C /usr/local -xzf -",
        ),
        "rust" => container::exec_cmd(
            app,
            "apt-get update && apt-get install -y curl build-essential && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y",
        ),
        _ => Ok(()),
    }
}

fn write_file(app: &str, path: &str, content: &str) -> Result<(), String> {
    // Use printf to avoid echo interpreting escape sequences
    let cmd = format!(
        "mkdir -p $(dirname {path}) && printf '%s' {} > {path}",
        shell_escape(content),
    );
    container::exec_cmd(app, &cmd)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn launch_app_background(app: &str, cmd: &str) -> Result<(), String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err("launch command is empty".to_string());
    }

    let escaped = shell_escape(cmd);
    let launch_cmd = format!(
        "mkdir -p /var/psht && cd /app && export PORT={APP_PORT} && {{ setsid sh -c {escaped} > /var/psht/app.log 2>&1 < /dev/null & echo $! > /var/psht/app.pid; }}"
    );
    container::exec_cmd(app, &launch_cmd)?;

    // Confirm the process is actually alive, not just that a PID was written.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if exec_output(
            app,
            "test -s /var/psht/app.pid && kill -0 $(cat /var/psht/app.pid) && echo ok",
        )
        .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }

    Err(format!(
        "app process failed to stay up after launch: {cmd}\n{}",
        debug_info(app)
    ))
}

fn scaffold_static(app: &str) -> Result<(), String> {
    write_file(app, "/app/index.html", "<html><body>ok</body></html>")
}

fn scaffold_python(app: &str) -> Result<(), String> {
    write_file(app, "/app/requirements.txt", "")?;
    write_file(
        app,
        "/app/app.py",
        concat!(
            "import http.server, os\n",
            "\n",
            "class H(http.server.BaseHTTPRequestHandler):\n",
            "    def do_GET(self):\n",
            "        self.send_response(200)\n",
            "        self.end_headers()\n",
            "        self.wfile.write(b'ok')\n",
            "    def log_message(self, *a):\n",
            "        pass\n",
            "\n",
            "port = int(os.environ.get('PORT', '8000'))\n",
            "http.server.HTTPServer(('', port), H).serve_forever()\n",
        ),
    )
}

fn scaffold_node(app: &str) -> Result<(), String> {
    write_file(
        app,
        "/app/package.json",
        r#"{"name":"inttest","version":"1.0.0","scripts":{"start":"node server.js"}}"#,
    )?;
    write_file(
        app,
        "/app/server.js",
        concat!(
            "const http = require('http');\n",
            "const port = process.env.PORT || 8000;\n",
            "http.createServer((req, res) => { res.end('ok'); }).listen(port);\n",
        ),
    )
}

fn scaffold_go(app: &str) -> Result<(), String> {
    write_file(app, "/app/go.mod", "module inttest\n\ngo 1.23\n")?;
    write_file(
        app,
        "/app/main.go",
        concat!(
            "package main\n",
            "\n",
            "import (\n",
            "\t\"fmt\"\n",
            "\t\"net/http\"\n",
            "\t\"os\"\n",
            ")\n",
            "\n",
            "func main() {\n",
            "\tport := os.Getenv(\"PORT\")\n",
            "\tif port == \"\" {\n",
            "\t\tport = \"8000\"\n",
            "\t}\n",
            "\thttp.HandleFunc(\"/\", func(w http.ResponseWriter, r *http.Request) {\n",
            "\t\tfmt.Fprint(w, \"ok\")\n",
            "\t})\n",
            "\thttp.ListenAndServe(\":\"+port, nil)\n",
            "}\n",
        ),
    )
}

fn scaffold_rust(app: &str) -> Result<(), String> {
    write_file(
        app,
        "/app/Cargo.toml",
        concat!(
            "[package]\n",
            "name = \"app\"\n",
            "version = \"0.1.0\"\n",
            "edition = \"2021\"\n",
        ),
    )?;
    write_file(
        app,
        "/app/src/main.rs",
        concat!(
            "use std::io::{Read, Write};\n",
            "use std::net::TcpListener;\n",
            "\n",
            "fn main() {\n",
            "    let port = std::env::var(\"PORT\").unwrap_or_else(|_| \"8000\".into());\n",
            "    let listener = TcpListener::bind(format!(\"0.0.0.0:{port}\")).unwrap();\n",
            "    for stream in listener.incoming() {\n",
            "        if let Ok(mut s) = stream {\n",
            "            let mut buf = [0u8; 1024];\n",
            "            let _ = s.read(&mut buf);\n",
            "            let body = \"ok\";\n",
            "            let resp = format!(\n",
            "                \"HTTP/1.0 200 OK\\r\\nContent-Length: {}\\r\\n\\r\\n{}\",\n",
            "                body.len(),\n",
            "                body\n",
            "            );\n",
            "            let _ = s.write_all(resp.as_bytes());\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    )
}

fn scaffold_binary(app: &str) -> Result<(), String> {
    write_file(app, "/app/index.html", "<html><body>ok</body></html>")?;
    write_file(
        app,
        "/app/app",
        concat!("#!/bin/sh\n", "exec python3 -m http.server \"$PORT\"\n",),
    )?;
    container::exec_cmd(app, "chmod 755 /app/app")?;
    write_file(app, "/app/.psht-start-command", "./app\n")
}

const APP_PORT: u16 = 8080;

fn deploy_stack(app: &str, stack: &str) -> Result<(ContainerGuard, String), String> {
    let guard = ContainerGuard::new(app)?;
    wait_for_container_network(app)?;

    install_runtime(app, stack)?;
    container::exec_cmd(app, "mkdir -p /var/psht")?;

    match stack {
        "binary" => scaffold_binary(app)?,
        "static" => scaffold_static(app)?,
        "python" => scaffold_python(app)?,
        "node" => scaffold_node(app)?,
        "go" => scaffold_go(app)?,
        "rust" => scaffold_rust(app)?,
        _ => return Err(format!("unknown stack: {stack}")),
    }

    // Install dependencies
    match stack {
        "node" => container::exec_cmd(app, "cd /app && npm install")?,
        "go" => container::exec_cmd(
            app,
            "export PATH=$PATH:/usr/local/go/bin && cd /app && go build -o app .",
        )?,
        "rust" => container::exec_cmd(
            app,
            ". $HOME/.cargo/env && cd /app && cargo build --release",
        )?,
        _ => {}
    }

    // Start the app
    let start_cmd = match stack {
        "binary" => "./app",
        "static" => "python3 -m http.server \"$PORT\"",
        "python" => "python3 app.py",
        "node" => "node server.js",
        "go" => "./app",
        "rust" => "./target/release/app",
        _ => unreachable!(),
    };
    launch_app_background(app, start_cmd)?;

    // Brief pause to let the app bind its port
    thread::sleep(Duration::from_secs(2));

    // Dump app log for debugging if app fails to start
    if let Ok(log) = exec_output(app, "cat /var/psht/app.log 2>/dev/null") {
        if !log.is_empty() {
            eprintln!("--- app.log for {app} ---\n{log}\n---");
        }
    }

    let ip = container_ip(app)?;
    Ok((guard, ip))
}

fn caddy_debug_info(app: &str) -> String {
    let log = exec_output(app, "cat /var/psht/caddy.log 2>/dev/null").unwrap_or_default();
    let ps = exec_output(app, "ps aux | grep [c]addy 2>/dev/null").unwrap_or_default();
    format!("caddy.log:\n{log}\n\ncaddy ps:\n{ps}")
}

fn wait_for_tcp(addr: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let connect_addr = if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    };
    while Instant::now() < deadline {
        if TcpStream::connect(&connect_addr).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "TCP port did not open on {addr}:{port} within {timeout:?}"
    ))
}

fn wait_for_http_with_host(
    connect_addr: &str,
    port: u16,
    host_header: &str,
    path: &str,
    expected_body: &str,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    let dial = if connect_addr.contains(':') {
        format!("[{connect_addr}]:{port}")
    } else {
        format!("{connect_addr}:{port}")
    };
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(&dial) {
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = String::new();
                if stream.read_to_string(&mut response).is_ok() && response.contains(expected_body)
                {
                    return Ok(response);
                }
                last_err = response;
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "no matching HTTP response on {connect_addr}:{port} with Host {host_header} within {timeout:?}; last response: {last_err}"
    ))
}

fn setup_caddy_container(
    caddy_app: &str,
    upstream_name: &str,
    upstream_ip: &str,
) -> Result<(ContainerGuard, String), String> {
    let guard = ContainerGuard::new(caddy_app)?;
    wait_for_container_network(caddy_app)?;

    container::exec_cmd(caddy_app, "apt-get update && apt-get install -y caddy")?;
    container::exec_cmd(caddy_app, "mkdir -p /var/psht /etc/caddy")?;

    let caddyfile = concat!(
        "{\n",
        "    admin 0.0.0.0:2019\n",
        "    auto_https off\n",
        "}\n",
        "\n",
        ":80 {\n",
        "}\n",
    );
    write_file(caddy_app, "/etc/caddy/Caddyfile", caddyfile)?;
    container::exec_cmd(
        caddy_app,
        &format!(
            "grep -q ' {upstream_name}$' /etc/hosts || echo '{upstream_ip} {upstream_name}' >> /etc/hosts"
        ),
    )?;
    container::exec_cmd(
        caddy_app,
        "pkill caddy 2>/dev/null || true; nohup caddy run --config /etc/caddy/Caddyfile --adapter caddyfile > /var/psht/caddy.log 2>&1 &",
    )?;

    let caddy_ip = container_ip(caddy_app)?;
    wait_for_tcp(&caddy_ip, 2019, Duration::from_secs(30))?;
    wait_for_http_with_host(
        &caddy_ip,
        80,
        "bootstrap.example.com",
        "/",
        "HTTP/1.1 200 OK",
        Duration::from_secs(30),
    )?;

    Ok((guard, caddy_ip))
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn integration_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("warning: integration lock poisoned; continuing");
            poisoned.into_inner()
        }
    }
}

fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{pid}-{n}", pid = std::process::id())
}

struct CaddyEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    old_api_url: Option<String>,
    old_domain: Option<String>,
}

impl CaddyEnvGuard {
    fn set(api_url: &str, domain: Option<&str>) -> Self {
        let lock = match env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("warning: env lock poisoned; continuing");
                poisoned.into_inner()
            }
        };
        let old_api_url = std::env::var("CADDY_API_URL").ok();
        let old_domain = std::env::var("CADDY_DOMAIN").ok();

        // SAFETY: environment mutation is serialized by a global mutex for these tests.
        unsafe {
            std::env::set_var("CADDY_API_URL", api_url);
            match domain {
                Some(value) => std::env::set_var("CADDY_DOMAIN", value),
                None => std::env::remove_var("CADDY_DOMAIN"),
            }
        }

        Self {
            _lock: lock,
            old_api_url,
            old_domain,
        }
    }
}

impl Drop for CaddyEnvGuard {
    fn drop(&mut self) {
        // SAFETY: environment mutation is serialized by a global mutex for these tests.
        unsafe {
            match &self.old_api_url {
                Some(value) => std::env::set_var("CADDY_API_URL", value),
                None => std::env::remove_var("CADDY_API_URL"),
            }
            match &self.old_domain {
                Some(value) => std::env::set_var("CADDY_DOMAIN", value),
                None => std::env::remove_var("CADDY_DOMAIN"),
            }
        }
    }
}

struct DeployEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    old_home: Option<String>,
    old_skip_tailscale: Option<String>,
    old_incus_project: Option<String>,
}

impl DeployEnvGuard {
    fn set(home: &Path) -> Self {
        let lock = match env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("warning: env lock poisoned; continuing");
                poisoned.into_inner()
            }
        };
        let old_home = std::env::var("HOME").ok();
        let old_skip_tailscale = std::env::var("PSHT_SKIP_TAILSCALE").ok();
        let old_incus_project = std::env::var("INCUS_PROJECT").ok();
        let home_value = home.to_string_lossy().to_string();
        let uid_output = Command::new("id").args(["-u"]).output().ok();
        let user_project = uid_output.and_then(|output| {
            if output.status.success() {
                let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if uid.is_empty() {
                    None
                } else {
                    Some(format!("user-{uid}"))
                }
            } else {
                None
            }
        });
        // SAFETY: environment mutation is serialized by a global mutex for these tests.
        unsafe {
            std::env::set_var("HOME", home_value);
            std::env::set_var("PSHT_SKIP_TAILSCALE", "1");
            if let Some(project) = user_project.as_deref() {
                std::env::set_var("INCUS_PROJECT", project);
            }
        }
        Self {
            _lock: lock,
            old_home,
            old_skip_tailscale,
            old_incus_project,
        }
    }
}

impl Drop for DeployEnvGuard {
    fn drop(&mut self) {
        // SAFETY: environment mutation is serialized by a global mutex for these tests.
        unsafe {
            match &self.old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.old_skip_tailscale {
                Some(value) => std::env::set_var("PSHT_SKIP_TAILSCALE", value),
                None => std::env::remove_var("PSHT_SKIP_TAILSCALE"),
            }
            match &self.old_incus_project {
                Some(value) => std::env::set_var("INCUS_PROJECT", value),
                None => std::env::remove_var("INCUS_PROJECT"),
            }
        }
    }
}

struct AppFamilyGuard {
    app: String,
}

impl AppFamilyGuard {
    fn new(app: &str) -> Self {
        Self {
            app: app.to_string(),
        }
    }
}

impl Drop for AppFamilyGuard {
    fn drop(&mut self) {
        cleanup_app_family(&self.app);
    }
}

fn run_git(args: &[&str], cwd: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {} failed: {e}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(format!("git {} failed: {stderr}", args.join(" ")))
}

fn git_output(args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {} failed: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_user_project_exists() -> Result<String, String> {
    let uid_output = Command::new("id")
        .args(["-u"])
        .output()
        .map_err(|e| format!("failed to run id -u: {e}"))?;
    if !uid_output.status.success() {
        let stderr = String::from_utf8_lossy(&uid_output.stderr)
            .trim()
            .to_string();
        return Err(format!("id -u failed: {stderr}"));
    }
    let uid = String::from_utf8_lossy(&uid_output.stdout)
        .trim()
        .to_string();
    let project = format!("user-{uid}");
    let show = Command::new("incus")
        .args(["project", "show", &project])
        .output()
        .map_err(|e| format!("failed to inspect incus project {project}: {e}"))?;
    if !show.status.success() {
        let create = Command::new("incus")
            .args(["project", "create", &project])
            .output()
            .map_err(|e| format!("failed to create incus project {project}: {e}"))?;
        if !create.status.success() {
            let stderr = String::from_utf8_lossy(&create.stderr).trim().to_string();
            return Err(format!(
                "failed to create incus project {project}: {stderr}"
            ));
        }
    }
    let set_proxy = Command::new("incus")
        .args(["project", "set", &project, "restricted.devices.proxy=allow"])
        .output()
        .map_err(|e| format!("failed to update project {project} proxy policy: {e}"))?;
    if !set_proxy.status.success() {
        let stderr = String::from_utf8_lossy(&set_proxy.stderr)
            .trim()
            .to_string();
        return Err(format!(
            "failed to set restricted.devices.proxy=allow for project {project}: {stderr}"
        ));
    }
    Ok(project)
}

fn app_family_instances(app: &str) -> Result<Vec<String>, String> {
    let family_prefix = format!("psht-{app}");
    Ok(container::list()?
        .into_iter()
        .filter_map(|container| {
            if container.name == family_prefix
                || container.name.starts_with(&format!("{family_prefix}-"))
            {
                Some(container.name)
            } else {
                None
            }
        })
        .collect())
}

fn cleanup_app_family(app: &str) {
    let Ok(instances) = app_family_instances(app) else {
        return;
    };
    for instance in instances {
        let _ = Command::new("incus")
            .args(["stop", "--force", &instance])
            .status();
        let _ = Command::new("incus")
            .args(["delete", "--force", &instance])
            .status();
    }
}

fn deploy_repo_init(
    home: &Path,
    app: &str,
    initial_body: &str,
) -> Result<(PathBuf, String), String> {
    let repos_dir = home.join("repos");
    fs::create_dir_all(&repos_dir)
        .map_err(|e| format!("failed to create repos dir {}: {e}", repos_dir.display()))?;
    let work = home.join(format!("{app}-work"));
    fs::create_dir_all(&work)
        .map_err(|e| format!("failed to create work dir {}: {e}", work.display()))?;

    run_git(&["init"], &work)?;
    run_git(&["config", "user.name", "psht integration tests"], &work)?;
    run_git(
        &["config", "user.email", "psht-integration-tests@example.com"],
        &work,
    )?;
    fs::write(work.join("index.html"), initial_body)
        .map_err(|e| format!("failed to write initial index.html: {e}"))?;
    run_git(&["add", "index.html"], &work)?;
    run_git(&["commit", "-m", "initial"], &work)?;
    run_git(&["branch", "-M", "main"], &work)?;

    let remote = repos_dir.join(format!("{app}.git"));
    let remote_str = remote.to_string_lossy().to_string();
    run_git(&["init", "--bare", &remote_str], home)?;
    run_git(&["remote", "add", "origin", &remote_str], &work)?;
    run_git(&["push", "-u", "origin", "main"], &work)?;

    let symbolic = Command::new("git")
        .args([
            "--git-dir",
            &remote_str,
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ])
        .output()
        .map_err(|e| format!("failed to set bare HEAD to main: {e}"))?;
    if !symbolic.status.success() {
        let stderr = String::from_utf8_lossy(&symbolic.stderr).trim().to_string();
        return Err(format!("failed to set bare HEAD to main: {stderr}"));
    }

    let sha = git_output(&["rev-parse", "HEAD"], &work)?;
    Ok((work, sha))
}

fn deploy_repo_commit(
    work: &Path,
    body: &str,
    procfile: Option<&str>,
    message: &str,
) -> Result<String, String> {
    fs::write(work.join("index.html"), body)
        .map_err(|e| format!("failed to update index.html: {e}"))?;
    let procfile_path = work.join("Procfile");
    match procfile {
        Some(contents) => fs::write(&procfile_path, contents)
            .map_err(|e| format!("failed to write Procfile: {e}"))?,
        None => {
            if let Err(e) = fs::remove_file(&procfile_path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("failed to remove Procfile: {e}"));
            }
        }
    }
    run_git(&["add", "-A"], work)?;
    run_git(&["commit", "-m", message], work)?;
    run_git(&["push", "origin", "main"], work)?;
    git_output(&["rev-parse", "HEAD"], work)
}

fn deploy_port(app: &str) -> u16 {
    let hash: u32 = app
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    3001 + (hash % 1000) as u16
}

#[test]
fn integration_caddy_end_to_end_with_real_container() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-caddy-app");
    let caddy_app = unique_name("inttest-caddy");

    let (_app_guard, app_ip) = deploy_stack(&app, "static").expect("deploy app failed");
    let (_caddy_guard, caddy_ip) =
        setup_caddy_container(&caddy_app, &app, &app_ip).expect("setup caddy failed");

    let api_url = format!("http://{caddy_ip}:2019");
    let route_host = format!("{app}.example.com");

    {
        let _env = CaddyEnvGuard::set(&api_url, Some("example.com"));
        if let Err(e) = caddy::add(&app, APP_PORT) {
            panic!(
                "caddy add failed: {e}\ncaddy debug:\n{}",
                caddy_debug_info(&caddy_app)
            );
        }
    }

    let resp = wait_for_http_with_host(
        &caddy_ip,
        80,
        &route_host,
        "/",
        "ok",
        Duration::from_secs(30),
    )
    .unwrap_or_else(|e| {
        panic!(
            "caddy route did not proxy app: {e}\napp debug:\n{}\ncaddy debug:\n{}",
            debug_info(&app),
            caddy_debug_info(&caddy_app)
        )
    });
    assert!(resp.contains("ok"), "expected proxied app response: {resp}");

    {
        let _env = CaddyEnvGuard::set(&api_url, None);
        caddy::remove(&app).expect("caddy remove failed");
    }

    let removed = wait_for_http_with_host(
        &caddy_ip,
        80,
        &route_host,
        "/",
        "Content-Length: 0",
        Duration::from_secs(30),
    )
    .unwrap_or_else(|e| {
        panic!(
            "caddy route was not removed: {e}\ncaddy debug:\n{}",
            caddy_debug_info(&caddy_app)
        )
    });
    assert!(
        removed.contains("Content-Length: 0"),
        "expected fallback response after remove: {removed}"
    );
}

#[test]
fn integration_static() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-static");
    let (_guard, ip) = deploy_stack(&app, "static").expect("deploy static failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("static app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_binary_no_procfile() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-binary");
    let (_guard, ip) = deploy_stack(&app, "binary").expect("deploy binary failed");

    let marker = exec_output(&app, "cat /app/.psht-start-command").expect("read marker failed");
    assert_eq!(marker.trim(), "./app", "expected start-command marker");
    exec_output(&app, "test ! -f /app/Procfile").expect("Procfile should not exist");

    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("binary app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_python() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-python");
    let (_guard, ip) = deploy_stack(&app, "python").expect("deploy python failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("python app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_node() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-node");
    let (_guard, ip) = deploy_stack(&app, "node").expect("deploy node failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("node app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_go() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-go");
    let (_guard, ip) = deploy_stack(&app, "go").expect("deploy go failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("go app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_rust() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-rust");
    let (_guard, ip) = deploy_stack(&app, "rust").expect("deploy rust failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("rust app not reachable: {e}\n{}", debug_info(&app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_deploy_blue_green_switches_revision_and_cleans_staging_containers() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-deploy-bluegreen");
    let home = tempfile::tempdir().expect("failed to create temp HOME");
    let _env = DeployEnvGuard::set(home.path());
    let _family_guard = AppFamilyGuard::new(&app);

    if let Err(e) = ensure_user_project_exists() {
        eprintln!("skipping blue/green integration test: {e}");
        return;
    }

    let pool = default_storage_pool_name().expect("failed to get storage pool");
    let volume = storage_volume_name(&app);
    delete_storage_volume_if_exists(&pool, &volume).expect("failed to clear stale storage volume");
    let _volume_guard = StorageVolumeGuard::new(&pool, &volume);

    let (work, first_sha) = deploy_repo_init(
        home.path(),
        &app,
        "<html><body>ok-bluegreen-v1</body></html>\n",
    )
    .expect("failed to initialize deploy repository");

    commands::deploy(&app, Some("refs/heads/main"), Some(&first_sha), true)
        .expect("initial deploy failed");
    let port = deploy_port(&app);
    wait_for_http_with_host(
        "127.0.0.1",
        port,
        "localhost",
        "/",
        "ok-bluegreen-v1",
        Duration::from_secs(90),
    )
    .unwrap_or_else(|e| panic!("initial revision not reachable: {e}\n{}", debug_info(&app)));

    let second_sha = deploy_repo_commit(
        &work,
        "<html><body>ok-bluegreen-v2</body></html>\n",
        None,
        "update to v2",
    )
    .expect("failed to commit second revision");

    commands::deploy(&app, Some("refs/heads/main"), Some(&second_sha), false)
        .expect("blue/green deploy failed");

    let response = wait_for_http_with_host(
        "127.0.0.1",
        port,
        "localhost",
        "/",
        "ok-bluegreen-v2",
        Duration::from_secs(90),
    )
    .unwrap_or_else(|e| panic!("second revision not reachable: {e}\n{}", debug_info(&app)));
    assert!(
        response.contains("ok-bluegreen-v2"),
        "expected v2 body, got response: {response}"
    );

    let instances = app_family_instances(&app).expect("failed to list app family instances");
    assert!(
        instances.len() == 1,
        "expected only one active instance after cutover, got: {instances:?}"
    );
    assert!(
        instances
            .iter()
            .any(|name| name.starts_with(&format!("psht-{app}-build-"))),
        "expected active instance to be candidate-style name, got: {instances:?}"
    );
    assert!(
        !instances
            .iter()
            .any(|name| { name.contains("-prev-") || name.contains("-failed-") }),
        "staging instances should be cleaned after successful cutover: {instances:?}"
    );
}

#[test]
fn integration_deploy_blue_green_rolls_back_when_new_revision_fails_to_start() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-deploy-rollback");
    let home = tempfile::tempdir().expect("failed to create temp HOME");
    let _env = DeployEnvGuard::set(home.path());
    let _family_guard = AppFamilyGuard::new(&app);

    if let Err(e) = ensure_user_project_exists() {
        eprintln!("skipping blue/green rollback integration test: {e}");
        return;
    }

    let pool = default_storage_pool_name().expect("failed to get storage pool");
    let volume = storage_volume_name(&app);
    delete_storage_volume_if_exists(&pool, &volume).expect("failed to clear stale storage volume");
    let _volume_guard = StorageVolumeGuard::new(&pool, &volume);

    let (work, first_sha) = deploy_repo_init(
        home.path(),
        &app,
        "<html><body>ok-rollback-v1</body></html>\n",
    )
    .expect("failed to initialize deploy repository");

    commands::deploy(&app, Some("refs/heads/main"), Some(&first_sha), true)
        .expect("initial deploy failed");
    let port = deploy_port(&app);
    wait_for_http_with_host(
        "127.0.0.1",
        port,
        "localhost",
        "/",
        "ok-rollback-v1",
        Duration::from_secs(90),
    )
    .unwrap_or_else(|e| panic!("initial revision not reachable: {e}\n{}", debug_info(&app)));

    let bad_sha = deploy_repo_commit(
        &work,
        "<html><body>ok-rollback-v2</body></html>\n",
        Some("web: ./target/release/definitely-missing"),
        "introduce missing startup binary",
    )
    .expect("failed to commit broken revision");

    let err = commands::deploy(&app, Some("refs/heads/main"), Some(&bad_sha), false)
        .expect_err("broken revision should fail and rollback");
    assert!(
        err.contains("rollback was applied"),
        "expected rollback error marker, got: {err}"
    );
    assert!(
        err.contains("Last app log lines"),
        "expected deploy error to include candidate app logs for diagnosis, got: {err}"
    );

    let response = wait_for_http_with_host(
        "127.0.0.1",
        port,
        "localhost",
        "/",
        "ok-rollback-v1",
        Duration::from_secs(90),
    )
    .unwrap_or_else(|e| panic!("rollback did not restore v1: {e}\n{}", debug_info(&app)));
    assert!(
        response.contains("ok-rollback-v1"),
        "expected rollback to previous body, got response: {response}"
    );

    let instances = app_family_instances(&app).expect("failed to list app family instances");
    assert!(
        instances.iter().any(|name| name == &format!("psht-{app}")),
        "expected active instance psht-{app}, got: {instances:?}"
    );
    assert!(
        !instances.iter().any(|name| name.contains("-prev-")),
        "previous instance should be cleaned after rollback: {instances:?}"
    );
    assert!(
        !instances.iter().any(|name| name.contains("-build-")),
        "candidate instance should be cleaned after rollback: {instances:?}"
    );

    let recovered_sha = deploy_repo_commit(
        &work,
        "<html><body>ok-rollback-v3</body></html>\n",
        None,
        "recover after failed deploy",
    )
    .expect("failed to commit recovery revision");

    commands::deploy(&app, Some("refs/heads/main"), Some(&recovered_sha), false)
        .expect("recovery deploy after rollback failed");

    let recovered_response = wait_for_http_with_host(
        "127.0.0.1",
        port,
        "localhost",
        "/",
        "ok-rollback-v3",
        Duration::from_secs(90),
    )
    .unwrap_or_else(|e| {
        panic!(
            "recovery deploy did not restore service: {e}\n{}",
            debug_info(&app)
        )
    });
    assert!(
        recovered_response.contains("ok-rollback-v3"),
        "expected recovery body, got response: {recovered_response}"
    );

    let recovered_instances =
        app_family_instances(&app).expect("failed to list app family instances after recovery");
    assert!(
        recovered_instances.len() == 1,
        "expected one active instance after recovery deploy, got: {recovered_instances:?}"
    );
    assert!(
        recovered_instances
            .iter()
            .any(|name| name.starts_with(&format!("psht-{app}-build-"))),
        "expected active instance to be candidate-style name after recovery, got: {recovered_instances:?}"
    );
    assert!(
        !recovered_instances
            .iter()
            .any(|name| name.contains("-failed-") || name.contains("-prev-")),
        "stale failed/previous instances should be cleaned after recovery deploy: {recovered_instances:?}"
    );
}

// RAII guard: launches an Incus VM; force-stops + deletes on drop.
struct VmGuard {
    name: String,
}

impl VmGuard {
    fn new(name: &str) -> Result<Self, String> {
        let status = Command::new("incus")
            .args(["launch", "images:ubuntu/24.04", name, "--vm"])
            .status()
            .map_err(|e| format!("failed to launch VM: {e}"))?;
        if !status.success() {
            return Err(format!("incus launch --vm failed for {name}"));
        }
        Ok(Self {
            name: name.to_string(),
        })
    }
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        let _ = Command::new("incus")
            .args(["stop", "--force", &self.name])
            .status();
        let _ = Command::new("incus").args(["delete", &self.name]).status();
    }
}

fn wait_for_vm_agent(name: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let output = Command::new("incus")
            .args(["exec", name, "--", "echo", "ready"])
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(format!("VM agent for {name} not ready after {timeout:?}"))
}

fn vm_exec(name: &str, cmd: &str) -> Result<String, String> {
    let output = Command::new("incus")
        .args(["exec", name, "--", "sh", "-c", cmd])
        .output()
        .map_err(|e| format!("vm exec failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("vm exec `{cmd}` failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn integration_bootstrap() {
    let _serial = integration_test_lock();
    let vm_name = unique_name("inttest-bootstrap");
    let _guard = VmGuard::new(&vm_name).expect("failed to launch VM");

    // Wait for the VM agent to be ready (VMs take longer than containers)
    wait_for_vm_agent(&vm_name, Duration::from_secs(120)).expect("VM agent not ready");

    // Build and push the latest psht-server binary into the VM at a non-standard path.
    let build_status = Command::new("cargo")
        .args(["build", "--bin", "psht-server"])
        .status()
        .expect("failed to build psht-server binary");
    assert!(
        build_status.success(),
        "cargo build --bin psht-server failed"
    );

    let psht_bin = format!("{}/target/debug/psht-server", env!("CARGO_MANIFEST_DIR"));
    vm_exec(&vm_name, "mkdir -p /opt/psht/bin").expect("failed to create /opt/psht/bin");
    let status = Command::new("incus")
        .args([
            "file",
            "push",
            &psht_bin,
            &format!("{vm_name}/opt/psht/bin/psht-server"),
        ])
        .status()
        .expect("failed to push binary");
    assert!(status.success(), "incus file push failed");

    // Make it executable
    vm_exec(&vm_name, "chmod 755 /opt/psht/bin/psht-server").expect("failed to chmod psht-server");

    // Run bootstrap with Tailscale skipped
    let output = Command::new("incus")
        .args([
            "exec",
            &vm_name,
            "--",
            "sh",
            "-c",
            "PSHT_SKIP_TAILSCALE=1 /opt/psht/bin/psht-server bootstrap",
        ])
        .output()
        .expect("failed to run bootstrap");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bootstrap failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify: user psht exists
    vm_exec(&vm_name, "id psht").expect("user psht should exist");
    vm_exec(
        &vm_name,
        "stat -c '%U:%G' /home/psht | grep -q '^psht:psht$'",
    )
    .expect("/home/psht should be owned by psht");
    vm_exec(
        &vm_name,
        "su -s /bin/sh -c 'mkdir -p /home/psht/.config/incus && test -w /home/psht/.config/incus' psht",
    )
    .expect("psht should be able to create ~/.config/incus");

    // Verify: stacks directory has 7 .sh files
    let count =
        vm_exec(&vm_name, "ls /home/psht/stacks/*.sh | wc -l").expect("failed to count stacks");
    assert_eq!(count.trim(), "7", "expected 7 stack scripts, got {count}");

    // Verify: repos and builds directories exist
    vm_exec(&vm_name, "test -d /home/psht/repos").expect("/home/psht/repos should exist");
    vm_exec(&vm_name, "test -d /home/psht/builds").expect("/home/psht/builds should exist");
    vm_exec(&vm_name, "test -f /home/psht/.hushlogin").expect("/home/psht/.hushlogin should exist");

    // Verify: psht shell path is the dropped binary path.
    vm_exec(
        &vm_name,
        "getent passwd psht | grep -q ':/opt/psht/bin/psht-server$'",
    )
    .expect("psht user shell should be /opt/psht/bin/psht-server");
    vm_exec(&vm_name, "grep -qx /opt/psht/bin/psht-server /etc/shells")
        .expect("/opt/psht/bin/psht-server should be in /etc/shells");

    // Verify: incus is installed
    vm_exec(&vm_name, "command -v incus").expect("incus should be installed");
    vm_exec(
        &vm_name,
        "su -s /bin/sh -c 'incus launch images:ubuntu/24.04 psht-bootstrap-check && incus exec psht-bootstrap-check -- sh -c \"ip link show eth0 >/dev/null 2>&1\" && incus delete -f psht-bootstrap-check' psht",
    )
    .expect("psht should be able to launch a container with a network device");
}

#[test]
fn integration_start_launches_app_process_from_metadata() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-start");
    let _guard = ContainerGuard::new(&app).expect("create container failed");
    wait_for_container_network(&app).expect("container network not ready");

    container::exec_cmd(
        &app,
        "mkdir -p /app /var/psht /etc && printf '%s\\n' '#!/bin/sh' 'while true; do sleep 60; done' > /app/run.sh && chmod 755 /app/run.sh",
    )
    .expect("failed to prepare app script");
    container::exec_cmd(&app, "printf '%s\\n' './run.sh' > /etc/psht-start-command")
        .expect("failed to write start metadata");
    container::stop(&app).expect("failed to stop container before start test");

    commands::start(&app).expect("commands::start failed");

    let alive = container::exec_output(
        &app,
        "test -s /var/psht/app.pid && kill -0 $(cat /var/psht/app.pid) && echo ok",
    )
    .expect("app process not running");
    assert_eq!(alive.trim(), "ok");
}

#[test]
fn integration_start_is_noop_when_app_process_already_running() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-start-noop");
    let _guard = ContainerGuard::new(&app).expect("create container failed");
    wait_for_container_network(&app).expect("container network not ready");

    container::exec_cmd(
        &app,
        "mkdir -p /app /var/psht /etc && printf '%s\\n' '#!/bin/sh' 'while true; do sleep 60; done' > /app/run.sh && chmod 755 /app/run.sh",
    )
    .expect("failed to prepare app script");
    container::exec_cmd(&app, "printf '%s\\n' './run.sh' > /etc/psht-start-command")
        .expect("failed to write start metadata");
    container::stop(&app).expect("failed to stop container before start test");

    commands::start(&app).expect("first commands::start failed");
    let first_pid = exec_output(&app, "cat /var/psht/app.pid").expect("failed to read first pid");

    commands::start(&app).expect("second commands::start failed");
    let second_pid = exec_output(&app, "cat /var/psht/app.pid").expect("failed to read second pid");

    assert_eq!(
        second_pid.trim(),
        first_pid.trim(),
        "start should no-op when app process is already running"
    );
}

#[test]
fn integration_storage_persists_across_container_rebuild() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-storage-persist");
    let pool = default_storage_pool_name().expect("failed to get storage pool");
    let volume = storage_volume_name(&app);
    delete_storage_volume_if_exists(&pool, &volume).expect("failed to clear stale storage volume");
    let _volume_guard = StorageVolumeGuard::new(&pool, &volume);

    let _guard = ContainerGuard::new(&app).expect("create container failed");
    wait_for_container_network(&app).expect("container network not ready");

    ensure_storage_volume(&pool, &volume).expect("failed to create app storage volume");
    container::ensure_storage_mount(&app, &pool, &volume).expect("failed to attach storage");
    container::exec_cmd(
        &app,
        "mkdir -p /storage && printf '%s\\n' 'sentinel' > /storage/persist.txt",
    )
    .expect("failed to write storage sentinel");

    container::stop(&app).expect("failed to stop first container");
    container::delete(&app).expect("failed to delete first container");
    container::create(&app).expect("failed to recreate container");
    wait_for_container_network(&app).expect("recreated container network not ready");
    container::ensure_storage_mount(&app, &pool, &volume).expect("failed to reattach storage");

    let value = exec_output(&app, "cat /storage/persist.txt").expect("failed to read sentinel");
    assert_eq!(value.trim(), "sentinel");
}

#[test]
fn integration_destroy_removes_app_storage_volume() {
    let _serial = integration_test_lock();
    let app = unique_name("inttest-storage-destroy");
    let pool = default_storage_pool_name().expect("failed to get storage pool");
    let volume = storage_volume_name(&app);
    delete_storage_volume_if_exists(&pool, &volume).expect("failed to clear stale storage volume");

    let _guard = ContainerGuard::new(&app).expect("create container failed");
    wait_for_container_network(&app).expect("container network not ready");
    ensure_storage_volume(&pool, &volume).expect("failed to create app storage volume");
    container::ensure_storage_mount(&app, &pool, &volume).expect("failed to attach storage");
    assert!(
        storage_volume_exists(&pool, &volume),
        "storage volume should exist before destroy"
    );

    commands::destroy(&app).expect("commands::destroy failed");
    assert!(
        !storage_volume_exists(&pool, &volume),
        "storage volume should be removed by destroy"
    );
}
