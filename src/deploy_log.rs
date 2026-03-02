use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEPLOY_LOG_RETENTION_SECS: u64 = 24 * 60 * 60;
const DEPLOY_LOG_MAX_BYTES: usize = 512 * 1024;

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn deploy_logs_root() -> PathBuf {
    home_dir().join(".psht").join("deploy-logs")
}

fn app_deploy_logs_dir_in(root: &Path, app: &str) -> PathBuf {
    root.join(app)
}

fn deploy_log_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    format!(
        "{}-{:03}-{}",
        now.as_secs(),
        now.subsec_millis(),
        std::process::id()
    )
}

pub fn timestamp_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

fn format_line(source: &str, line: &str) -> String {
    format!("{} [{}] {}", timestamp_now(), source, line)
}

fn trim_file_to_limit(path: &Path, max_bytes: usize) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|e| format!("failed to stat deploy log {}: {e}", path.display()))?;
    if metadata.len() as usize <= max_bytes {
        return Ok(());
    }

    let bytes =
        fs::read(path).map_err(|e| format!("failed to read deploy log {}: {e}", path.display()))?;
    if bytes.len() <= max_bytes {
        return Ok(());
    }

    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && bytes[start] != b'\n' {
        start += 1;
    }
    if start < bytes.len() {
        start += 1;
    }

    fs::write(path, &bytes[start..])
        .map_err(|e| format!("failed to trim deploy log {}: {e}", path.display()))
}

fn append_to_path(path: &Path, source: &str, message: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open deploy log {}: {e}", path.display()))?;

    let mut wrote = false;
    for raw in message.split('\n') {
        if raw.trim().is_empty() {
            continue;
        }
        let cleaned = raw.replace('\r', "");
        let line = format_line(source, cleaned.trim_end());
        writeln!(file, "{line}")
            .map_err(|e| format!("failed to write deploy log {}: {e}", path.display()))?;
        wrote = true;
    }
    if !wrote {
        let line = format_line(source, message.trim_end());
        writeln!(file, "{line}")
            .map_err(|e| format!("failed to write deploy log {}: {e}", path.display()))?;
    }

    trim_file_to_limit(path, DEPLOY_LOG_MAX_BYTES)?;
    Ok(())
}

fn prune_old_logs_in(root: &Path, now: SystemTime) -> Result<(), String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to read {}: {e}", root.display())),
    };

    for app_entry in entries {
        let app_entry = app_entry.map_err(|e| format!("failed to scan {}: {e}", root.display()))?;
        let app_path = app_entry.path();
        if !app_path.is_dir() {
            continue;
        }

        let log_entries = fs::read_dir(&app_path)
            .map_err(|e| format!("failed to read {}: {e}", app_path.display()))?;
        for log_entry in log_entries {
            let log_entry =
                log_entry.map_err(|e| format!("failed to scan {}: {e}", app_path.display()))?;
            let log_path = log_entry.path();
            if !log_path.is_file() {
                continue;
            }
            let metadata = log_entry
                .metadata()
                .map_err(|e| format!("failed to stat {}: {e}", log_path.display()))?;
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            let age_secs = now
                .duration_since(modified)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_secs();
            if age_secs > DEPLOY_LOG_RETENTION_SECS {
                let _ = fs::remove_file(&log_path);
            }
        }

        let mut remaining = fs::read_dir(&app_path)
            .map_err(|e| format!("failed to read {}: {e}", app_path.display()))?;
        if remaining.next().is_none() {
            let _ = fs::remove_dir(&app_path);
        }
    }
    Ok(())
}

fn recent_entries_in(
    root: &Path,
    app: &str,
    max_files: usize,
    max_lines_per_file: usize,
) -> Result<Vec<String>, String> {
    let app_dir = app_deploy_logs_dir_in(root, app);
    let entries = match fs::read_dir(&app_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("failed to read {}: {e}", app_dir.display())),
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to scan {}: {e}", app_dir.display()))?;
        let path = entry.path();
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    if max_files > 0 && files.len() > max_files {
        files = files[files.len() - max_files..].to_vec();
    }

    let mut result = Vec::new();
    for path in files {
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read deploy log {}: {e}", path.display()))?;
        let mut lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
        if max_lines_per_file > 0 && lines.len() > max_lines_per_file {
            lines = lines[lines.len() - max_lines_per_file..].to_vec();
        }
        result.extend(lines);
    }
    Ok(result)
}

fn active_log_slot() -> &'static Mutex<Option<PathBuf>> {
    static SLOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

struct ActiveDeployLogGuard {
    previous: Option<PathBuf>,
}

impl ActiveDeployLogGuard {
    fn activate(path: PathBuf) -> Self {
        let mut guard = active_log_slot().lock().unwrap_or_else(|e| e.into_inner());
        let previous = guard.replace(path);
        Self { previous }
    }
}

impl Drop for ActiveDeployLogGuard {
    fn drop(&mut self) {
        let mut guard = active_log_slot().lock().unwrap_or_else(|e| e.into_inner());
        *guard = self.previous.take();
    }
}

pub struct DeployLogSession {
    _active: ActiveDeployLogGuard,
}

pub fn start_for_app(app: &str) -> Result<DeployLogSession, String> {
    let root = deploy_logs_root();
    let _ = prune_old_logs_in(&root, SystemTime::now());
    let app_dir = app_deploy_logs_dir_in(&root, app);
    fs::create_dir_all(&app_dir)
        .map_err(|e| format!("failed to create {}: {e}", app_dir.display()))?;
    let path = app_dir.join(format!("{}.log", deploy_log_id()));
    append_to_path(
        &path,
        "deploy",
        &format!("deploy log initialized for {app}"),
    )?;
    let active = ActiveDeployLogGuard::activate(path);
    Ok(DeployLogSession { _active: active })
}

pub fn append(source: &str, message: &str) {
    let path = {
        let guard = active_log_slot().lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    if let Some(path) = path {
        let _ = append_to_path(&path, source, message);
    }
}

pub fn recent_entries(
    app: &str,
    max_files: usize,
    max_lines_per_file: usize,
) -> Result<Vec<String>, String> {
    let root = deploy_logs_root();
    let _ = prune_old_logs_in(&root, SystemTime::now());
    recent_entries_in(&root, app, max_files, max_lines_per_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn trim_file_to_limit_keeps_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deploy.log");
        fs::write(&path, "line-1\nline-2\nline-3\nline-4\n").unwrap();
        trim_file_to_limit(&path, 12).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("line-4"),
            "trimmed content should keep newest tail: {content}"
        );
        assert!(
            content.len() <= 12,
            "trimmed content should not exceed requested size"
        );
    }

    #[test]
    fn recent_entries_reads_newest_files_and_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("deploy-logs");
        let app_dir = app_deploy_logs_dir_in(&root, "myapp");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("001.log"), "a1\na2\na3\n").unwrap();
        fs::write(app_dir.join("002.log"), "b1\nb2\n").unwrap();
        fs::write(app_dir.join("003.log"), "c1\nc2\nc3\n").unwrap();

        let lines = recent_entries_in(&root, "myapp", 2, 2).unwrap();
        assert_eq!(
            lines,
            vec![
                "b1".to_string(),
                "b2".to_string(),
                "c2".to_string(),
                "c3".to_string()
            ]
        );
    }

    #[test]
    fn prune_old_logs_removes_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("deploy-logs");
        let app_dir = app_deploy_logs_dir_in(&root, "myapp");
        fs::create_dir_all(&app_dir).unwrap();
        let stale = app_dir.join("stale.log");
        fs::write(&stale, "old\n").unwrap();
        thread::sleep(Duration::from_millis(10));

        let now = SystemTime::now() + Duration::from_secs(DEPLOY_LOG_RETENTION_SECS + 5);
        prune_old_logs_in(&root, now).unwrap();
        assert!(!stale.exists(), "stale log should be removed");
    }
}
