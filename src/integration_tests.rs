use std::io::Read as _;
use std::process::Command;
use std::time::{Duration, Instant};
use std::thread;

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
            let ip = output.split_whitespace().next().unwrap_or("").to_string();
            if !ip.is_empty() {
                return Ok(ip);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!("could not get container IP for {app}"))
}

fn wait_for_http(addr: &str, port: u16, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = std::net::TcpStream::connect(format!("{addr}:{port}")) {
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
        "node" => container::exec_cmd(
            app,
            "apt-get update && apt-get install -y nodejs npm",
        ),
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
            "cd /app && PORT={APP_PORT} nohup python3 -m http.server {APP_PORT} > /app/app.log 2>&1 &"
        ),
        "python" => format!(
            "cd /app && PORT={APP_PORT} nohup python3 app.py > /app/app.log 2>&1 &"
        ),
        "node" => format!(
            "cd /app && PORT={APP_PORT} nohup node server.js > /app/app.log 2>&1 &"
        ),
        "go" => format!(
            "cd /app && PORT={APP_PORT} nohup ./app > /app/app.log 2>&1 &"
        ),
        "rust" => format!(
            "cd /app && PORT={APP_PORT} nohup ./target/release/app > /app/app.log 2>&1 &"
        ),
        _ => unreachable!(),
    };
    container::exec_cmd(app, &start_cmd)?;

    let ip = container_ip(app)?;
    Ok((guard, ip))
}

#[test]
fn integration_static() {
    let app = "inttest-static";
    let (_guard, ip) = deploy_stack(app, "static").expect("deploy static failed");
    let resp =
        wait_for_http(&ip, APP_PORT, Duration::from_secs(30)).expect("static app not reachable");
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_python() {
    let app = "inttest-python";
    let (_guard, ip) = deploy_stack(app, "python").expect("deploy python failed");
    let resp =
        wait_for_http(&ip, APP_PORT, Duration::from_secs(30)).expect("python app not reachable");
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_node() {
    let app = "inttest-node";
    let (_guard, ip) = deploy_stack(app, "node").expect("deploy node failed");
    let resp =
        wait_for_http(&ip, APP_PORT, Duration::from_secs(60)).expect("node app not reachable");
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_go() {
    let app = "inttest-go";
    let (_guard, ip) = deploy_stack(app, "go").expect("deploy go failed");
    let resp =
        wait_for_http(&ip, APP_PORT, Duration::from_secs(60)).expect("go app not reachable");
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}

#[test]
fn integration_rust() {
    let app = "inttest-rust";
    let (_guard, ip) = deploy_stack(app, "rust").expect("deploy rust failed");
    let resp =
        wait_for_http(&ip, APP_PORT, Duration::from_secs(120)).expect("rust app not reachable");
    assert!(resp.contains("ok"), "expected 'ok' in response: {resp}");
}
