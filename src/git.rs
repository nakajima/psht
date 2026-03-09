use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process;

use crate::app_name;

pub(crate) const GIT_LOCAL_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_NAMESPACE",
    "GIT_SUPER_PREFIX",
    "GIT_QUARANTINE_PATH",
];

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn repos_dir() -> PathBuf {
    home_dir().join("repos")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn ensure_repo(app: &str) -> Result<PathBuf, String> {
    ensure_repo_in(app, &repos_dir())
}

fn ensure_repo_in(app: &str, base: &PathBuf) -> Result<PathBuf, String> {
    app_name::validate_app_name(app)?;
    let path = base.join(format!("{app}.git"));
    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| format!("failed to create repo dir: {e}"))?;
        let status = process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&path)
            .status()
            .map_err(|e| format!("failed to run git init: {e}"))?;
        if !status.success() {
            return Err("git init --bare failed".to_string());
        }
    }
    install_hook_at(app, &path)?;
    Ok(path)
}

fn install_hook_at(app: &str, repo: &PathBuf) -> Result<(), String> {
    let hook_dir = repo.join("hooks");
    fs::create_dir_all(&hook_dir).map_err(|e| format!("failed to create hooks dir: {e}"))?;
    let hook_path = hook_dir.join("post-receive");
    let current_exe = env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "psht".to_string());
    let unset_git_env = format!("unset {}\n", GIT_LOCAL_ENV_VARS.join(" "));
    let hook_content = format!(
        "#!/bin/sh
set -eu
line_count=0
deploy_ref=
deploy_sha=
while IFS=' ' read -r old new ref; do
  [ -n \"$old\" ] || continue
  line_count=$((line_count + 1))
  if [ \"$line_count\" -gt 1 ]; then
    echo \"error: push updates multiple refs; push one ref at a time\" >&2
    exit 1
  fi
  deploy_ref=\"$ref\"
  deploy_sha=\"$new\"
done
if [ \"$line_count\" -eq 0 ]; then
  exit 0
fi
{}
exec {} deploy {} --ref \"$deploy_ref\" --sha \"$deploy_sha\"
",
        unset_git_env,
        shell_quote(&current_exe),
        shell_quote(app)
    );

    if hook_path.exists() {
        let existing = fs::read_to_string(&hook_path).unwrap_or_default();
        if existing == hook_content {
            return Ok(());
        }
    }

    fs::write(&hook_path, &hook_content).map_err(|e| format!("failed to write hook: {e}"))?;
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("failed to set hook permissions: {e}"))?;
    Ok(())
}

pub fn handle_receive_pack(app: &str) -> Result<(), String> {
    let path = ensure_repo(app)?;
    let err = exec::execvp(
        "git-receive-pack",
        &["git-receive-pack", &path.to_string_lossy()],
    );
    Err(format!("exec git-receive-pack failed: {err}"))
}

pub fn handle_upload_pack(app: &str) -> Result<(), String> {
    let path = ensure_repo(app)?;
    let err = exec::execvp(
        "git-upload-pack",
        &["git-upload-pack", &path.to_string_lossy()],
    );
    Err(format!("exec git-upload-pack failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ensure_repo_creates_bare_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        fs::create_dir_all(&repos).unwrap();
        let path = ensure_repo_in("testapp", &repos).unwrap();
        assert!(path.exists());
        assert!(path.join("HEAD").exists());
    }

    #[test]
    fn ensure_repo_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        fs::create_dir_all(&repos).unwrap();
        let path1 = ensure_repo_in("testapp2", &repos).unwrap();
        let path2 = ensure_repo_in("testapp2", &repos).unwrap();
        assert_eq!(path1, path2);
    }

    #[test]
    fn install_hook_writes_executable_script() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        fs::create_dir_all(&repos).unwrap();
        let repo = ensure_repo_in("hooktest", &repos).unwrap();
        let hook_path = repo.join("hooks").join("post-receive");
        assert!(hook_path.exists());
        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("deploy 'hooktest' --ref \"$deploy_ref\" --sha \"$deploy_sha\""));
        assert!(content.contains("while IFS=' ' read -r old new ref; do"));
        assert!(content.contains("push updates multiple refs"));
        assert!(content.contains("unset GIT_DIR GIT_WORK_TREE"));
        assert!(content.find("unset ").unwrap() < content.find("exec ").unwrap());
        assert!(content.starts_with("#!/bin/sh"));
        let perms = fs::metadata(&hook_path).unwrap().permissions();
        assert_ne!(perms.mode() & 0o111, 0, "hook should be executable");
    }

    #[test]
    fn ensure_repo_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("repos");
        fs::create_dir_all(&repos).unwrap();
        let err = ensure_repo_in("bad/name", &repos).unwrap_err();
        assert!(err.contains("invalid app name"));
    }
}
