use std::fs;
use std::path::Path;

#[derive(Debug, PartialEq)]
pub enum AppType {
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
}

impl AppType {
    fn default_start_command(&self) -> &str {
        match self {
            AppType::Node => "npm start",
            AppType::Python => "python app.py",
            AppType::Rust => "./target/release/app",
            AppType::Go => "./app",
            AppType::Static => "python3 -m http.server $PORT",
        }
    }

    fn install_command(&self) -> &str {
        match self {
            AppType::Node => "npm install",
            AppType::Python => "pip install -r requirements.txt",
            AppType::Rust => "cargo build --release",
            AppType::Go => "go build -o app .",
            AppType::Static => "",
        }
    }
}

impl AppConfig {
    pub fn install_command(&self) -> &str {
        self.app_type.install_command()
    }
}

pub fn detect(dir: &Path) -> Result<AppConfig, String> {
    let start_command = read_procfile(dir);

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
            let start = start_command
                .unwrap_or_else(|| app_type.default_start_command().to_string());
            return Ok(AppConfig {
                app_type: *app_type,
                start_command: start,
            });
        }
    }

    Err("could not detect app type".to_string())
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
        fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let config = detect(tmp.path()).unwrap();
        assert_eq!(config.app_type, AppType::Rust);
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
        assert_eq!(AppType::Node.install_command(), "npm install");
        assert_eq!(AppType::Python.install_command(), "pip install -r requirements.txt");
        assert_eq!(AppType::Rust.install_command(), "cargo build --release");
        assert_eq!(AppType::Go.install_command(), "go build -o app .");
        assert_eq!(AppType::Static.install_command(), "");
    }
}
