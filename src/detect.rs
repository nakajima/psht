use std::fs;
use std::path::Path;

use serde::Deserialize;
use toml::Value;

#[derive(Debug, PartialEq)]
pub enum AppType {
    Binary,
    Bun,
    Node,
    Python,
    Rust,
    Go,
    Static,
}

#[derive(Debug, PartialEq)]
pub struct AppConfig {
    pub app_type: AppType,
    pub start_command: String,
    pub install_command: String,
    pub preinstall_command: Option<String>,
    pub postinstall_command: Option<String>,
    pub apt_packages: Vec<String>,
    pub required_env: Vec<String>,
}

impl AppType {
    fn default_start_command(&self) -> &str {
        match self {
            AppType::Binary => "./app",
            AppType::Bun => "bun run index.ts",
            AppType::Node => "npm start",
            AppType::Python => "python app.py",
            AppType::Rust => "",
            AppType::Go => "./app",
            AppType::Static => "python3 -m http.server $PORT",
        }
    }

    pub fn stack(&self) -> &str {
        match self {
            AppType::Binary => "binary",
            AppType::Bun => "bun",
            AppType::Node => "node",
            AppType::Python => "python",
            AppType::Rust => "rust",
            AppType::Go => "go",
            AppType::Static => "static",
        }
    }

    fn install_command(&self) -> &str {
        match self {
            AppType::Binary => "",
            AppType::Bun => "bun install",
            AppType::Node => "npm install",
            AppType::Python => "pip install -r requirements.txt",
            AppType::Rust => "cargo build --release",
            AppType::Go => "go build -o app .",
            AppType::Static => "",
        }
    }
}

impl AppConfig {
    pub fn stack(&self) -> &str {
        self.app_type.stack()
    }
}

pub fn detect(dir: &Path) -> Result<AppConfig, String> {
    let hooks = read_deploy_hooks(dir)?;
    let procfile_start_command = read_procfile(dir);

    if let Some(start_command) = read_start_command_file(dir)? {
        let start = hooks.start.clone().unwrap_or(start_command);
        return Ok(AppConfig {
            app_type: AppType::Binary,
            start_command: start,
            install_command: "".to_string(),
            preinstall_command: hooks.preinstall,
            postinstall_command: hooks.postinstall,
            apt_packages: hooks.apt_packages,
            required_env: hooks.required_env,
        });
    }

    let start_command = hooks.start.clone().or(procfile_start_command);

    if is_bun_project(dir) {
        let start = start_command
            .clone()
            .unwrap_or_else(|| AppType::Bun.default_start_command().to_string());
        let install = if dir.join("package.json").exists() {
            AppType::Bun.install_command()
        } else {
            ""
        };
        return Ok(AppConfig {
            app_type: AppType::Bun,
            start_command: start,
            install_command: install.to_string(),
            preinstall_command: hooks.preinstall.clone(),
            postinstall_command: hooks.postinstall.clone(),
            apt_packages: hooks.apt_packages.clone(),
            required_env: hooks.required_env.clone(),
        });
    }

    let markers: &[(&str, AppType)] = &[
        ("Cargo.toml", AppType::Rust),
        ("package.json", AppType::Node),
        ("requirements.txt", AppType::Python),
        ("Pipfile", AppType::Python),
        ("go.mod", AppType::Go),
        ("index.html", AppType::Static),
    ];

    for (file, app_type) in markers {
        if dir.join(file).exists() {
            let start = match start_command.clone() {
                Some(start) => start,
                None => default_start_command_for(*app_type, dir)?,
            };
            let install = match app_type {
                AppType::Bun if !dir.join("package.json").exists() => "",
                _ => app_type.install_command(),
            };
            return Ok(AppConfig {
                app_type: *app_type,
                start_command: start,
                install_command: install.to_string(),
                preinstall_command: hooks.preinstall.clone(),
                postinstall_command: hooks.postinstall.clone(),
                apt_packages: hooks.apt_packages.clone(),
                required_env: hooks.required_env.clone(),
            });
        }
    }

    Err("could not detect app type".to_string())
}

fn is_bun_project(dir: &Path) -> bool {
    let has_bun_marker = dir.join("bun.lockb").exists() || dir.join("bunfig.toml").exists();
    let has_index_ts = dir.join("index.ts").exists();
    has_bun_marker || (has_index_ts && !dir.join("package.json").exists())
}

fn read_start_command_file(dir: &Path) -> Result<Option<String>, String> {
    let marker = dir.join(".psht-start-command");
    if !marker.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&marker)
        .map_err(|e| format!("failed to read {}: {e}", marker.display()))?;
    let cmd = content.trim();
    if cmd.is_empty() {
        return Err(".psht-start-command is empty".to_string());
    }
    Ok(Some(cmd.to_string()))
}

fn read_procfile(dir: &Path) -> Option<String> {
    let procfile = dir.join("Procfile");
    let content = fs::read_to_string(procfile).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(cmd) = line.strip_prefix("web:") {
            return Some(cmd.trim().to_string());
        }
    }
    None
}

fn default_start_command_for(app_type: AppType, dir: &Path) -> Result<String, String> {
    match app_type {
        AppType::Rust => rust_default_start_command(dir),
        _ => Ok(app_type.default_start_command().to_string()),
    }
}

fn rust_default_start_command(dir: &Path) -> Result<String, String> {
    let cargo_toml = dir.join("Cargo.toml");
    let content = fs::read_to_string(&cargo_toml)
        .map_err(|e| format!("failed to read {}: {e}", cargo_toml.display()))?;
    let parsed: Value = toml::from_str(&content)
        .map_err(|e| format!("failed to parse {}: {e}", cargo_toml.display()))?;

    let package = parsed.get("package").and_then(Value::as_table);
    let package_name = package
        .and_then(|pkg| pkg.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);
    let default_run = package
        .and_then(|pkg| pkg.get("default-run"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);

    if let Some(name) = default_run {
        return Ok(format!("./target/release/{name}"));
    }

    let mut bin_names = Vec::new();
    if let Some(entries) = parsed.get("bin").and_then(Value::as_array) {
        for entry in entries {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || bin_names.iter().any(|v| v == name) {
                continue;
            }
            bin_names.push(name.to_string());
        }
    }

    if let Some(package_name) = package_name {
        if bin_names.is_empty() || bin_names.iter().any(|name| name == &package_name) {
            return Ok(format!("./target/release/{package_name}"));
        }
        if bin_names.len() == 1 {
            return Ok(format!("./target/release/{}", bin_names[0]));
        }
        return Err(
            "could not infer Rust start binary (multiple [[bin]] entries). Set `start` in psht.toml"
                .to_string(),
        );
    }

    if bin_names.len() == 1 {
        return Ok(format!("./target/release/{}", bin_names[0]));
    }

    Err("could not infer Rust start binary from Cargo.toml. Set `start` in psht.toml".to_string())
}

#[derive(Default)]
struct DeployHooks {
    start: Option<String>,
    preinstall: Option<String>,
    postinstall: Option<String>,
    apt_packages: Vec<String>,
    required_env: Vec<String>,
}

#[derive(Deserialize)]
struct HookConfig {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    preinstall: Option<String>,
    #[serde(default)]
    postinstall: Option<String>,
    #[serde(default, alias = "apt")]
    apt_packages: Option<Vec<String>>,
    #[serde(default)]
    required_env: Option<Vec<String>>,
}

fn normalize_hook(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_start(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("psht.toml start is empty".to_string());
    }
    Ok(Some(trimmed.to_string()))
}

fn normalize_apt_packages(values: Option<Vec<String>>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values.unwrap_or_default() {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if normalized.iter().any(|v| v == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}

fn normalize_required_env(values: Option<Vec<String>>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values.unwrap_or_default() {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if normalized.iter().any(|v| v == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}

fn read_deploy_hooks(dir: &Path) -> Result<DeployHooks, String> {
    let path = dir.join("psht.toml");
    if !path.exists() {
        return Ok(DeployHooks::default());
    }

    let content =
        fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let cfg: HookConfig =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

    Ok(DeployHooks {
        start: normalize_start(cfg.start)?,
        preinstall: normalize_hook(cfg.preinstall),
        postinstall: normalize_hook(cfg.postinstall),
        apt_packages: normalize_apt_packages(cfg.apt_packages),
        required_env: normalize_required_env(cfg.required_env),
    })
}

// AppType needs Copy since we iterate over references
impl Copy for AppType {}
impl Clone for AppType {
    fn clone(&self) -> Self {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detect_bun_app() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.ts"), "console.log('hi')").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Bun);
        assert!(config.start_command.contains("bun run index.ts"));
    }

    #[test]
    fn bun_skips_install_without_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.ts"), "console.log('hi')").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert!(config.install_command.is_empty());
    }

    #[test]
    fn bun_installs_with_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.ts"), "").unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("bun.lockb"), "").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Bun);
        assert!(config.install_command.contains("bun install"));
    }

    #[test]
    fn node_typescript_project_prefers_node() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.ts"), "console.log('hi')").unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Node);
        assert_eq!(config.start_command, "npm start");
    }

    #[test]
    fn bun_marker_overrides_package_json() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.ts"), "console.log('hi')").unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("bunfig.toml"), "").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Bun);
    }

    #[test]
    fn stack_returns_correct_name() {
        assert_eq!(AppType::Binary.stack(), "binary");
        assert_eq!(AppType::Bun.stack(), "bun");
        assert_eq!(AppType::Node.stack(), "node");
        assert_eq!(AppType::Python.stack(), "python");
        assert_eq!(AppType::Rust.stack(), "rust");
        assert_eq!(AppType::Go.stack(), "go");
        assert_eq!(AppType::Static.stack(), "static");
    }

    #[test]
    fn detect_node_app() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Node);
        assert_eq!(config.start_command, "npm start");
    }

    #[test]
    fn detect_python_app_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("requirements.txt"), "flask").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Python);
    }

    #[test]
    fn detect_python_app_pipfile() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Pipfile"), "").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Python);
    }

    #[test]
    fn detect_rust_app() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"hyperlinked\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Rust);
        assert_eq!(config.start_command, "./target/release/hyperlinked");
    }

    #[test]
    fn detect_go_app() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("go.mod"), "module example").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Go);
    }

    #[test]
    fn detect_static_site() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.html"), "<html></html>").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Static);
    }

    #[test]
    fn detect_unknown_app() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn detect_binary_app_from_start_command_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "./mybin --flag").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Binary);
        assert_eq!(config.start_command, "./mybin --flag");
        assert!(config.install_command.is_empty());
    }

    #[test]
    fn detect_binary_app_rejects_empty_start_command_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "\n  \n").unwrap();
        let err = detect(tmp.path()).unwrap_err();
        assert!(err.contains(".psht-start-command is empty"));
    }

    #[test]
    fn detect_reads_hooks_from_psht_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(
            tmp.path().join("psht.toml"),
            "start = \"node server.js\"\npreinstall = \"echo pre\"\npostinstall = \"echo post\"\napt_packages = [\"curl\", \"git\"]\nrequired_env = [\"DATABASE_URL\", \"JWT_SECRET\"]\n",
        )
        .unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.start_command, "node server.js");
        assert_eq!(config.preinstall_command.as_deref(), Some("echo pre"));
        assert_eq!(config.postinstall_command.as_deref(), Some("echo post"));
        assert_eq!(config.apt_packages, vec!["curl", "git"]);
        assert_eq!(config.required_env, vec!["DATABASE_URL", "JWT_SECRET"]);
    }

    #[test]
    fn detect_treats_blank_hooks_as_unset() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(
            tmp.path().join("psht.toml"),
            "preinstall = \"   \"\npostinstall = \"\\n\\t\"\napt_packages = [\" \", \"curl\", \"curl\"]\nrequired_env = [\"  \", \"A\", \"A\", \"B\"]\n",
        )
        .unwrap();
        let config = detect(tmp.path()).unwrap();
        assert!(config.preinstall_command.is_none());
        assert!(config.postinstall_command.is_none());
        assert_eq!(config.apt_packages, vec!["curl"]);
        assert_eq!(config.required_env, vec!["A", "B"]);
    }

    #[test]
    fn detect_reads_apt_alias_from_psht_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("psht.toml"), "apt = [\"jq\", \"zip\"]\n").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.apt_packages, vec!["jq", "zip"]);
    }

    #[test]
    fn detect_errors_on_invalid_psht_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("psht.toml"), "preinstall = [").unwrap();
        let err = detect(tmp.path()).unwrap_err();
        assert!(err.contains("failed to parse"));
    }

    #[test]
    fn psht_toml_start_overrides_start_marker() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "./old\n").unwrap();
        fs::write(tmp.path().join("psht.toml"), "start = \"./new\"\n").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Binary);
        assert_eq!(config.start_command, "./new");
    }

    #[test]
    fn psht_toml_start_overrides_procfile() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("Procfile"), "web: node server.js").unwrap();
        fs::write(tmp.path().join("psht.toml"), "start = \"node app.js\"\n").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Node);
        assert_eq!(config.start_command, "node app.js");
    }

    #[test]
    fn psht_toml_empty_start_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("psht.toml"), "start = \"   \"\n").unwrap();
        let err = detect(tmp.path()).unwrap_err();
        assert!(err.contains("start is empty"));
    }

    #[test]
    fn rust_start_prefers_default_run() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"hyperlinked\"\nversion = \"0.1.0\"\ndefault-run = \"worker\"\n",
        )
        .unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.start_command, "./target/release/worker");
    }

    #[test]
    fn rust_start_uses_single_bin_when_package_name_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[[bin]]\nname = \"runner\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.start_command, "./target/release/runner");
    }

    #[test]
    fn rust_start_errors_for_ambiguous_bins() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nversion = \"0.1.0\"\n[[bin]]\nname = \"api\"\n[[bin]]\nname = \"worker\"\n",
        )
        .unwrap();
        let err = detect(tmp.path()).unwrap_err();
        assert!(err.contains("could not infer Rust start binary"));
    }

    #[test]
    fn procfile_overrides_start_command() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("Procfile"), "web: node server.js").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Node);
        assert_eq!(config.start_command, "node server.js");
    }

    #[test]
    fn procfile_ignores_non_web_lines() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("Procfile"), "worker: node worker.js").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.start_command, "npm start");
    }

    #[test]
    fn install_command_for_each_type() {
        assert_eq!(AppType::Binary.install_command(), "");
        assert!(AppType::Bun.install_command().contains("bun install"));
        assert_eq!(AppType::Node.install_command(), "npm install");
        assert_eq!(
            AppType::Python.install_command(),
            "pip install -r requirements.txt"
        );
        assert_eq!(AppType::Rust.install_command(), "cargo build --release");
        assert_eq!(AppType::Go.install_command(), "go build -o app .");
        assert_eq!(AppType::Static.install_command(), "");
    }
}
