use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

fn write_home_config(home: &Path) {
    let dir = home.join(".psht");
    fs::create_dir_all(&dir).expect("create ~/.psht");
    fs::write(dir.join("config.toml"), "host = \"example.com\"\n").expect("write config");
}

fn run_psht(project_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_psht"))
        .current_dir(project_dir)
        .env("HOME", home_dir)
        .args(args)
        .output()
        .expect("run psht")
}

#[test]
fn deploy_release_without_psht_toml_in_non_tty_errors_with_guidance() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    let output = run_psht(
        &project_dir,
        &home_dir,
        &[
            "deploy",
            "--url",
            "https://example.com/app.tar.gz",
            "--start",
            "./app",
        ],
    );
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("psht.toml is missing. Run `psht deploy` once interactively to generate it."),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn deploy_release_with_positional_https_url_without_psht_toml_in_non_tty_errors_with_guidance() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    let output = run_psht(
        &project_dir,
        &home_dir,
        &["deploy", "https://example.com/app.tar.gz"],
    );
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("psht.toml is missing. Run `psht deploy` once interactively to generate it."),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn deploy_release_same_project_url_override_persists_before_deploy() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    let old_url = "https://example.invalid/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
    let new_url = "https://example.invalid/org/repo/releases/download/v0.0.4/hyperlinked-0.0.4-x86_64-unknown-linux-gnu.tar.gz";

    fs::write(
        project_dir.join("psht.toml"),
        format!("url = \"{old_url}\"\nstart = \"./app\"\napp = \"demo\"\n"),
    )
    .expect("write psht.toml");

    let output = run_psht(
        &project_dir,
        &home_dir,
        &["deploy", "--url", new_url, "--start", "./app"],
    );
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("conflicting settings between psht.toml and CLI for keys: url"),
        "unexpected stderr:\n{stderr}"
    );
    let saved = fs::read_to_string(project_dir.join("psht.toml")).expect("read psht.toml");
    assert!(saved.contains(new_url), "unexpected config:\n{saved}");
}

#[test]
fn deploy_release_positional_same_project_url_override_persists() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    let old_url = "https://example.invalid/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
    let new_url = "https://example.invalid/org/repo/releases/download/v0.0.4/hyperlinked-0.0.4-x86_64-unknown-linux-gnu.tar.gz";

    fs::write(
        project_dir.join("psht.toml"),
        format!("url = \"{old_url}\"\nstart = \"./app\"\napp = \"demo\"\n"),
    )
    .expect("write psht.toml");

    let output = run_psht(&project_dir, &home_dir, &["deploy", new_url]);
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("conflicting settings between psht.toml and CLI for keys: url"),
        "unexpected stderr:\n{stderr}"
    );
    let saved = fs::read_to_string(project_dir.join("psht.toml")).expect("read psht.toml");
    assert!(saved.contains(new_url), "unexpected config:\n{saved}");
}

#[test]
fn deploy_release_url_override_rejected_when_project_changes() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    let old_url = "https://example.invalid/org/repo/releases/download/v0.0.3/hyperlinked-0.0.3-x86_64-unknown-linux-gnu.tar.gz";
    let new_url = "https://example.invalid/org/other/releases/download/v0.0.4/hyperlinked-0.0.4-x86_64-unknown-linux-gnu.tar.gz";

    fs::write(
        project_dir.join("psht.toml"),
        format!("url = \"{old_url}\"\nstart = \"./app\"\napp = \"demo\"\n"),
    )
    .expect("write psht.toml");

    let output = run_psht(
        &project_dir,
        &home_dir,
        &["deploy", "--url", new_url, "--start", "./app"],
    );
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("same-project version bumps"),
        "unexpected stderr:\n{stderr}"
    );
    let saved = fs::read_to_string(project_dir.join("psht.toml")).expect("read psht.toml");
    assert!(saved.contains(old_url), "unexpected config:\n{saved}");
}

#[test]
fn deploy_with_positional_target_rejected_when_release_url_exists_in_psht_toml() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    fs::write(
        project_dir.join("psht.toml"),
        concat!(
            "url = \"https://example.com/a.tar.gz\"\n",
            "start = \"./app\"\n",
            "app = \"demo\"\n",
        ),
    )
    .expect("write psht.toml");

    let output = run_psht(&project_dir, &home_dir, &["deploy", "my-app"]);
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("positional deploy target cannot be used when psht.toml has `url`"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn deploy_release_from_file_requires_start_key() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    fs::write(
        project_dir.join("psht.toml"),
        "url = \"https://example.com/a.tar.gz\"\n",
    )
    .expect("write psht.toml");

    let output = run_psht(&project_dir, &home_dir, &["deploy"]);
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("psht.toml: missing `start`"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn deploy_release_flags_rejected_when_psht_toml_has_no_release_settings() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("project");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&project_dir).expect("create project");
    fs::create_dir_all(&home_dir).expect("create home");
    write_home_config(&home_dir);

    fs::write(project_dir.join("psht.toml"), "app = \"demo\"\n").expect("write psht.toml");

    let output = run_psht(
        &project_dir,
        &home_dir,
        &[
            "deploy",
            "--url",
            "https://example.com/a.tar.gz",
            "--start",
            "./app",
        ],
    );
    assert!(!output.status.success(), "expected deploy to fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("psht.toml exists without release settings"),
        "unexpected stderr:\n{stderr}"
    );
}
