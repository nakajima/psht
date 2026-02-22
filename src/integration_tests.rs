use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::caddy;
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

const APP_PORT: u16 = 8080;

fn deploy_stack(app: &str, stack: &str) -> Result<(ContainerGuard, String), String> {
    let guard = ContainerGuard::new(app)?;
    wait_for_container_network(app)?;

    install_runtime(app, stack)?;
    container::exec_cmd(app, "mkdir -p /var/psht")?;

    match stack {
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
        "static" => format!(
            "cd /app && PORT={APP_PORT} nohup python3 -m http.server {APP_PORT} > /var/psht/app.log 2>&1 &"
        ),
        "python" => {
            format!("cd /app && PORT={APP_PORT} nohup python3 app.py > /var/psht/app.log 2>&1 &")
        }
        "node" => {
            format!("cd /app && PORT={APP_PORT} nohup node server.js > /var/psht/app.log 2>&1 &")
        }
        "go" => format!("cd /app && PORT={APP_PORT} nohup ./app > /var/psht/app.log 2>&1 &"),
        "rust" => format!(
            "cd /app && PORT={APP_PORT} nohup ./target/release/app > /var/psht/app.log 2>&1 &"
        ),
        _ => unreachable!(),
    };
    container::exec_cmd(app, &start_cmd)?;

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

struct CaddyEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    old_api_url: Option<String>,
    old_domain: Option<String>,
}

impl CaddyEnvGuard {
    fn set(api_url: &str, domain: Option<&str>) -> Self {
        let lock = env_lock().lock().expect("env lock poisoned");
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

#[test]
fn integration_caddy_end_to_end_with_real_container() {
    let app = "inttest-caddy-app";
    let caddy_app = "inttest-caddy";

    let (_app_guard, app_ip) = deploy_stack(app, "static").expect("deploy app failed");
    let (_caddy_guard, caddy_ip) =
        setup_caddy_container(caddy_app, app, &app_ip).expect("setup caddy failed");

    let api_url = format!("http://{caddy_ip}:2019");
    let route_host = format!("{app}.example.com");

    {
        let _env = CaddyEnvGuard::set(&api_url, Some("example.com"));
        if let Err(e) = caddy::add(app, APP_PORT) {
            panic!(
                "caddy add failed: {e}\ncaddy debug:\n{}",
                caddy_debug_info(caddy_app)
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
            debug_info(app),
            caddy_debug_info(caddy_app)
        )
    });
    assert!(resp.contains("ok"), "expected proxied app response: {resp}");

    {
        let _env = CaddyEnvGuard::set(&api_url, None);
        caddy::remove(app).expect("caddy remove failed");
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
            caddy_debug_info(caddy_app)
        )
    });
    assert!(
        removed.contains("Content-Length: 0"),
        "expected fallback response after remove: {removed}"
    );
}

#[test]
fn integration_static() {
    let app = "inttest-static";
    let (_guard, ip) = deploy_stack(app, "static").expect("deploy static failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("static app not reachable: {e}\n{}", debug_info(app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_python() {
    let app = "inttest-python";
    let (_guard, ip) = deploy_stack(app, "python").expect("deploy python failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("python app not reachable: {e}\n{}", debug_info(app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_node() {
    let app = "inttest-node";
    let (_guard, ip) = deploy_stack(app, "node").expect("deploy node failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("node app not reachable: {e}\n{}", debug_info(app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_go() {
    let app = "inttest-go";
    let (_guard, ip) = deploy_stack(app, "go").expect("deploy go failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(60))
        .unwrap_or_else(|e| panic!("go app not reachable: {e}\n{}", debug_info(app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_rust() {
    let app = "inttest-rust";
    let (_guard, ip) = deploy_stack(app, "rust").expect("deploy rust failed");
    let resp = wait_for_http(&ip, APP_PORT, Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("rust app not reachable: {e}\n{}", debug_info(app)));
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
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
    let vm_name = "inttest-bootstrap";
    let _guard = VmGuard::new(vm_name).expect("failed to launch VM");

    // Wait for the VM agent to be ready (VMs take longer than containers)
    wait_for_vm_agent(vm_name, Duration::from_secs(120)).expect("VM agent not ready");

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
    vm_exec(vm_name, "mkdir -p /opt/psht/bin").expect("failed to create /opt/psht/bin");
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
    vm_exec(vm_name, "chmod 755 /opt/psht/bin/psht-server").expect("failed to chmod psht-server");

    // Run bootstrap with Tailscale skipped
    let output = Command::new("incus")
        .args([
            "exec",
            vm_name,
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
    vm_exec(vm_name, "id psht").expect("user psht should exist");
    vm_exec(
        vm_name,
        "stat -c '%U:%G' /home/psht | grep -q '^psht:psht$'",
    )
    .expect("/home/psht should be owned by psht");
    vm_exec(
        vm_name,
        "su -s /bin/sh -c 'mkdir -p /home/psht/.config/incus && test -w /home/psht/.config/incus' psht",
    )
    .expect("psht should be able to create ~/.config/incus");

    // Verify: stacks directory has 6 .sh files
    let count =
        vm_exec(vm_name, "ls /home/psht/stacks/*.sh | wc -l").expect("failed to count stacks");
    assert_eq!(count.trim(), "6", "expected 6 stack scripts, got {count}");

    // Verify: repos and builds directories exist
    vm_exec(vm_name, "test -d /home/psht/repos").expect("/home/psht/repos should exist");
    vm_exec(vm_name, "test -d /home/psht/builds").expect("/home/psht/builds should exist");
    vm_exec(vm_name, "test -f /home/psht/.hushlogin").expect("/home/psht/.hushlogin should exist");

    // Verify: psht shell path is the dropped binary path.
    vm_exec(
        vm_name,
        "getent passwd psht | grep -q ':/opt/psht/bin/psht-server$'",
    )
    .expect("psht user shell should be /opt/psht/bin/psht-server");
    vm_exec(vm_name, "grep -qx /opt/psht/bin/psht-server /etc/shells")
        .expect("/opt/psht/bin/psht-server should be in /etc/shells");

    // Verify: incus is installed
    vm_exec(vm_name, "command -v incus").expect("incus should be installed");
    vm_exec(
        vm_name,
        "su -s /bin/sh -c 'incus launch images:ubuntu/24.04 psht-bootstrap-check && incus exec psht-bootstrap-check -- sh -c \"ip link show eth0 >/dev/null 2>&1\" && incus delete -f psht-bootstrap-check' psht",
    )
    .expect("psht should be able to launch a container with a network device");
}
