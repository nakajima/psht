use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CliUpdateManifest {
    version: String,
    forge_url: String,
}

pub(super) fn cli_update_manifest() -> CliUpdateManifest {
    CliUpdateManifest {
        version: env!("CARGO_PKG_VERSION").to_string(),
        forge_url: configured_forge_url(),
    }
}

pub(super) fn cli_update_manifest_json(manifest: &CliUpdateManifest) -> Result<String, String> {
    serde_json::to_string(manifest).map_err(|e| format!("failed to serialize update manifest: {e}"))
}

pub(super) fn setup_script(hostname: &str, manifest: &CliUpdateManifest) -> Result<String, String> {
    let manifest_json = cli_update_manifest_json(manifest)?;
    Ok(format!(
        r#"#!/bin/sh
set -e

cat >/dev/null <<'__PSHT_UPDATE_MANIFEST__'
{manifest_json}
__PSHT_UPDATE_MANIFEST__

VERSION="{version}"
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"
SOURCE_URL="${{PSHT_SOURCE_URL:-$FORGE_URL}}"
SOURCE_URL="${{SOURCE_URL%/}}"

detect_target() {{
  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64|Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64|Darwin/amd64) echo "x86_64-apple-darwin" ;;
    Darwin/aarch64|Darwin/arm64) echo "aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $os/$arch" >&2; exit 1 ;;
  esac
}}

install_cli() {{
  install_dir="$1"
  target=$(detect_target)
  asset_url="$FORGE_URL/releases/download/v$VERSION/psht-$VERSION-$target.tar.gz"
  tmpdir=$(mktemp -d)
  if curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz" 2>/dev/null; then
    tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
    install -m 755 "$tmpdir/psht" "$install_dir/psht"
    rm -rf "$tmpdir"
    return 0
  fi

  echo "warning: no prebuilt psht release for $target at $asset_url" >&2
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust toolchain or use a forge with prebuilt assets for $target" >&2
    rm -rf "$tmpdir"
    exit 1
  fi

  source_root="$tmpdir/source-root"
  echo "-----> building psht from source (this can take a few minutes)" >&2
  cargo install --git "$SOURCE_URL" --tag "v$VERSION" --root "$source_root" --bin psht
  install -m 755 "$source_root/bin/psht" "$install_dir/psht"
  rm -rf "$tmpdir"
}}

# Find or install psht CLI
if command -v psht >/dev/null 2>&1 && psht __is-cli >/dev/null 2>&1; then
  PSHT_BIN=$(command -v psht)
else
  printf "Install psht CLI to (default: ~/.local/bin): " >&2
  read -r install_dir < /dev/tty
  install_dir="${{install_dir:-$HOME/.local/bin}}"
  mkdir -p "$install_dir"
  install_cli "$install_dir"
  PSHT_BIN="$install_dir/psht"
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "NOTE: Add $install_dir to your PATH: export PATH=\"$install_dir:\$PATH\"" >&2 ;;
  esac
  echo "Installed psht CLI to $PSHT_BIN" >&2
fi

mkdir -p "$HOME/.psht"
config="$HOME/.psht/config.toml"
if [ ! -f "$config" ]; then
  echo 'host = "{hostname}"' > "$config"
fi

"$PSHT_BIN" setup"#,
        version = manifest.version,
        forge_url = manifest.forge_url,
    ))
}

pub(super) fn update_script(
    hostname: &str,
    manifest: &CliUpdateManifest,
) -> Result<String, String> {
    let manifest_json = cli_update_manifest_json(manifest)?;
    Ok(format!(
        r#"#!/bin/sh
set -e

cat >/dev/null <<'__PSHT_UPDATE_MANIFEST__'
{manifest_json}
__PSHT_UPDATE_MANIFEST__

PSHT_BIN=$(command -v psht) || {{ echo "psht not found. Run: ssh psht@{hostname} setup | sh" >&2; exit 1; }}
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"
SOURCE_URL="${{PSHT_SOURCE_URL:-$FORGE_URL}}"
SOURCE_URL="${{SOURCE_URL%/}}"

detect_target() {{
  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64|Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64|Darwin/amd64) echo "x86_64-apple-darwin" ;;
    Darwin/aarch64|Darwin/arm64) echo "aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $os/$arch" >&2; exit 1 ;;
  esac
}}

current=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || current=""
if [ "$current" = "{version}" ]; then
  echo "psht {version} (up to date)" >&2
  exit 0
fi

target=$(detect_target)
asset_url="$FORGE_URL/releases/download/v{version}/psht-{version}-$target.tar.gz"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
candidate="$tmpdir/psht"
if curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz" 2>/dev/null; then
  tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
else
  echo "warning: no prebuilt psht release for $target at $asset_url" >&2
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust toolchain or use a forge with prebuilt assets for $target" >&2
    exit 1
  fi
  source_root="$tmpdir/source-root"
  echo "-----> building psht from source (this can take a few minutes)" >&2
  cargo install --git "$SOURCE_URL" --tag "v{version}" --root "$source_root" --bin psht
  candidate="$source_root/bin/psht"
fi
if [ ! -x "$candidate" ]; then
  echo "error: downloaded archive missing executable psht binary" >&2
  exit 1
fi
candidate_version=$("$candidate" --version 2>/dev/null | awk '{{print $2}}') || candidate_version=""
if [ "$candidate_version" != "{version}" ]; then
  echo "error: downloaded psht ${{candidate_version:-unknown}}, expected {version}" >&2
  exit 1
fi
staged="$tmpdir/psht.new"
install -m 755 "$candidate" "$staged"
mv "$staged" "$PSHT_BIN"
installed=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || installed=""
if [ "$installed" != "{version}" ]; then
  echo "error: installed psht ${{installed:-unknown}}, expected {version}" >&2
  exit 1
fi
echo "psht $installed (updated)" >&2"#,
        version = manifest.version,
        forge_url = manifest.forge_url,
    ))
}

pub fn setup() -> Result<(), String> {
    let host = hostname();
    let manifest = cli_update_manifest();
    println!("{}", setup_script(&host, &manifest)?);
    Ok(())
}

pub fn update() -> Result<(), String> {
    let host = hostname();
    let manifest = cli_update_manifest();
    println!("{}", update_script(&host, &manifest)?);
    Ok(())
}

pub fn print_cli() -> Result<(), String> {
    let cli = ensure_cli_binary()?;
    let mut file =
        fs::File::open(&cli).map_err(|e| format!("failed to open {}: {e}", cli.display()))?;
    let mut stdout = std::io::stdout().lock();
    std::io::copy(&mut file, &mut stdout)
        .map_err(|e| format!("failed to stream {}: {e}", cli.display()))?;
    stdout
        .flush()
        .map_err(|e| format!("failed to flush stdout: {e}"))?;
    Ok(())
}

pub(super) fn init_stacks_in(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create stacks dir: {e}"))?;
    for (name, content) in STACKS {
        fs::write(dir.join(format!("{name}.sh")), content)
            .map_err(|e| format!("failed to write {name}.sh: {e}"))?;
    }
    Ok(())
}

pub fn init_stacks() -> Result<(), String> {
    init_stacks_in(&stacks_dir())
}

pub(super) fn write_oauth_config(
    path: &Path,
    client_id: &str,
    client_secret: &str,
) -> Result<(), String> {
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("OAuth client ID and secret are required".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content =
        format!("TS_OAUTH_CLIENT_ID={client_id}\nTS_OAUTH_CLIENT_SECRET={client_secret}\n");
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

pub(super) fn supervise_service_unit_content(psht_bin: &str, psht_home: &Path) -> String {
    let home = psht_home.to_string_lossy();
    format!(
        "[Unit]\nDescription=psht supervision daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nUser=psht\nGroup=psht\nWorkingDirectory={home}\nEnvironment=HOME={home}\nExecStart={psht_bin} daemon\nRestart=always\nRestartSec=2\nKillMode=process\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn install_supervision_units(psht_bin: &str, psht_home: &Path) -> Result<(), String> {
    let service = supervise_service_unit_content(psht_bin, psht_home);
    fs::write(SUPERVISE_SERVICE_PATH, service)
        .map_err(|e| format!("failed to write {SUPERVISE_SERVICE_PATH}: {e}"))?;
    run_cmd("chmod", &["644", SUPERVISE_SERVICE_PATH])?;
    let _ = fs::remove_file(LEGACY_SUPERVISE_TIMER_PATH);
    run_cmd("systemctl", &["daemon-reload"])?;
    let _ = run_cmd("systemctl", &["disable", "--now", "psht-supervise.timer"]);
    run_cmd("systemctl", &["enable", "--now", "psht-supervise.service"])?;
    Ok(())
}

pub(super) fn web_service_unit_content(
    psht_bin: &str,
    psht_home: &Path,
    bind: &str,
    port: u16,
) -> String {
    let home = psht_home.to_string_lossy();
    format!(
        "[Unit]\nDescription=psht web UI\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nUser=psht\nGroup=psht\nWorkingDirectory={home}\nEnvironment=HOME={home}\nExecStart={psht_bin} web serve --bind {bind} --port {port}\nRestart=always\nRestartSec=2\nKillMode=process\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn install_web_unit(psht_bin: &str, psht_home: &Path, bind: &str, port: u16) -> Result<(), String> {
    let service = web_service_unit_content(psht_bin, psht_home, bind, port);
    fs::write(WEB_SERVICE_PATH, service)
        .map_err(|e| format!("failed to write {WEB_SERVICE_PATH}: {e}"))?;
    run_cmd("chmod", &["644", WEB_SERVICE_PATH])?;
    run_cmd("systemctl", &["daemon-reload"])?;
    run_cmd("systemctl", &["enable", WEB_SERVICE_NAME])?;
    run_cmd("systemctl", &["restart", WEB_SERVICE_NAME])?;
    Ok(())
}

pub fn web_start(bind: &str, port: u16) -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht-server web start".to_string());
    }
    if !command_succeeds("id", &["psht"]) {
        return Err("psht user is missing. Run: sudo psht-server bootstrap".to_string());
    }

    let current_bin = current_psht_binary()?;
    let psht_bin = prepare_server_binary(&current_bin)?;
    let psht_bin_str = psht_bin.to_string_lossy().to_string();
    let psht_home = psht_user_home_dir();

    install_web_unit(&psht_bin_str, &psht_home, bind, port)?;
    println!("=====> psht web UI running via systemd on http://{bind}:{port}");
    Ok(())
}

pub fn web_stop() -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht-server web stop".to_string());
    }
    if !Path::new(WEB_SERVICE_PATH).exists() {
        println!("psht web UI service is not installed.");
        return Ok(());
    }

    run_cmd("systemctl", &["disable", "--now", WEB_SERVICE_NAME])?;
    println!("=====> psht web UI stopped");
    Ok(())
}

pub fn bootstrap() -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht-server bootstrap".to_string());
    }

    let psht_user = "psht";
    let psht_home = PathBuf::from(format!("/home/{psht_user}"));
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();

    let current_bin = current_psht_binary()?;
    let psht_bin = prepare_server_binary(&current_bin)?;
    let psht_bin_str = psht_bin.to_string_lossy().to_string();
    let psht_dir = current_bin
        .parent()
        .ok_or_else(|| "failed to determine psht binary directory".to_string())?;

    if !command_exists("incus") {
        eprintln!("-----> Installing Incus");
        if !command_exists("curl") {
            run_cmd("apt-get", &["update"])?;
            run_cmd("apt-get", &["install", "-y", "curl"])?;
        }

        fs::create_dir_all("/etc/apt/keyrings")
            .map_err(|e| format!("failed to create /etc/apt/keyrings: {e}"))?;
        run_cmd(
            "curl",
            &[
                "-fsSL",
                "https://pkgs.zabbly.com/key.asc",
                "-o",
                "/etc/apt/keyrings/zabbly.asc",
            ],
        )?;

        let codename = os_release_codename()?;
        let arch = run_cmd_capture("dpkg", &["--print-architecture"])?;
        let source = format!(
            "Enabled: yes\nTypes: deb\nURIs: https://pkgs.zabbly.com/incus/stable\nSuites: {codename}\nComponents: main\nArchitectures: {arch}\nSigned-By: /etc/apt/keyrings/zabbly.asc\n"
        );
        fs::write(
            "/etc/apt/sources.list.d/zabbly-incus-stable.sources",
            source,
        )
        .map_err(|e| {
            format!("failed to write /etc/apt/sources.list.d/zabbly-incus-stable.sources: {e}")
        })?;

        run_cmd("apt-get", &["update"])?;
        run_cmd("apt-get", &["install", "-y", "incus"])?;
    }

    let _ = Command::new("systemctl")
        .args(["start", "incus.socket", "incus-user.socket"])
        .status();

    if !command_succeeds("incus", &["profile", "show", "default"]) {
        eprintln!("-----> Initializing Incus");
        run_cmd("incus", &["admin", "init", "--minimal"])?;
    }

    if !skip_tailscale {
        if !command_exists("tailscale") {
            return Err(
                "Tailscale is not installed. Install it first: https://tailscale.com/download/linux"
                    .to_string(),
            );
        }
        if !command_succeeds("tailscale", &["status"]) {
            return Err("Tailscale is not connected. Run: sudo tailscale up --ssh".to_string());
        }
        if !tailscale_ssh_enabled()? {
            eprintln!("-----> Enabling Tailscale SSH");
            run_cmd("tailscale", &["up", "--ssh"])?;
        }
        eprintln!("-----> Tailscale SSH is active");
    }

    let oauth_config = psht_home.join(".config/tailscale-oauth");
    if !skip_tailscale {
        if oauth_config.exists() {
            eprintln!("-----> Tailscale OAuth already configured");
        } else if let (Some(client_id), Some(client_secret)) = (
            env::var("TS_OAUTH_CLIENT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            env::var("TS_OAUTH_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        ) {
            eprintln!("-----> Setting up Tailscale OAuth from environment");
            write_oauth_config(&oauth_config, client_id.trim(), client_secret.trim())?;
        } else {
            println!();
            eprintln!("-----> Setting up Tailscale OAuth for container networking");
            println!();
            println!("       1. Ensure tag:psht exists in your ACL:");
            println!("          https://login.tailscale.com/admin/acls/visual/tags/add");
            println!();
            println!("       2. Create a credential at:");
            println!("          {TAILSCALE_OAUTH_SETTINGS_URL}");
            println!("          Under Scopes, configure:");
            println!("            - Keys: Write, tag:psht");
            println!("            - Devices: Core Read");
            println!("            - Devices: Core Write");
            println!();

            let confirm = prompt_tty("       Have you completed the steps above? (y/n) ")?;
            if confirm != "y" && confirm != "Y" {
                return Err(
                    "Complete the steps above and re-run: sudo psht-server bootstrap".to_string(),
                );
            }

            println!();
            let client_id = prompt_tty("OAuth client ID: ")?;
            let client_secret = prompt_tty("OAuth client secret: ")?;
            write_oauth_config(&oauth_config, client_id.trim(), client_secret.trim())?;
        }

        eprintln!("-----> Validating Tailscale OAuth permissions");
        let oauth_check = check_tailscale_oauth_permissions(&oauth_config);
        if !oauth_check.all_ok() {
            return Err(format_tailscale_oauth_permission_failure(&oauth_check));
        }
    }

    ensure_line_in_file(Path::new("/etc/shells"), &psht_bin_str)?;

    if !command_succeeds("id", &[psht_user]) {
        eprintln!("-----> Creating user {psht_user}");
        run_cmd("useradd", &["-m", "-s", &psht_bin_str, psht_user])?;
    } else {
        eprintln!("-----> User {psht_user} exists, updating shell");
        run_cmd("chsh", &["-s", &psht_bin_str, psht_user])?;
    }

    let owner = format!("{psht_user}:{psht_user}");
    // Ensure the service user can create runtime config (for example ~/.config/incus).
    fs::create_dir_all(&psht_home)
        .map_err(|e| format!("failed to create {}: {e}", psht_home.display()))?;
    let psht_config_dir = psht_home.join(".config");
    fs::create_dir_all(&psht_config_dir)
        .map_err(|e| format!("failed to create {}: {e}", psht_config_dir.display()))?;
    let psht_home_s = psht_home.to_string_lossy().to_string();
    let psht_config_dir_s = psht_config_dir.to_string_lossy().to_string();
    run_cmd("chown", &[&owner, &psht_home_s])?;
    run_cmd("chown", &["-R", &owner, &psht_config_dir_s])?;

    // Suppress MOTD/noise on SSH login for the psht service user.
    let hushlogin = psht_home.join(".hushlogin");
    if !hushlogin.exists() {
        fs::write(&hushlogin, "")
            .map_err(|e| format!("failed to write {}: {e}", hushlogin.display()))?;
    }
    let hushlogin_s = hushlogin.to_string_lossy().to_string();
    run_cmd("chown", &[&owner, &hushlogin_s])?;
    run_cmd("chmod", &["644", &hushlogin_s])?;

    if oauth_config.exists() {
        let oauth = oauth_config.to_string_lossy().to_string();
        run_cmd("chown", &[&owner, &oauth])?;
        run_cmd("chmod", &["600", &oauth])?;
    }

    let psht_cli_src = psht_dir.join("psht");
    let psht_bin_dir = psht_home.join("bin");
    fs::create_dir_all(&psht_bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", psht_bin_dir.display()))?;
    if psht_cli_src.exists() {
        let psht_cli_dst = psht_bin_dir.join("psht");
        fs::copy(&psht_cli_src, &psht_cli_dst).map_err(|e| {
            format!(
                "failed to copy {} to {}: {e}",
                psht_cli_src.display(),
                psht_cli_dst.display()
            )
        })?;
        let cli_path = psht_cli_dst.to_string_lossy().to_string();
        let cli_dir = psht_bin_dir.to_string_lossy().to_string();
        run_cmd("chmod", &["755", &cli_path])?;
        run_cmd("chown", &[&owner, &cli_dir, &cli_path])?;
    }

    eprintln!("-----> Adding {psht_user} to incus group");
    run_cmd("usermod", &["-aG", "incus", psht_user])?;

    let mut incus_ready = false;
    for _ in 0..30 {
        if command_succeeds("incus", &["info"]) {
            incus_ready = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !incus_ready {
        return Err("incus did not become ready after 30 seconds".to_string());
    }

    let psht_uid = run_cmd_capture("id", &["-u", psht_user])?;
    let psht_project = format!("user-{}", psht_uid.trim());
    if !command_succeeds("incus", &["project", "show", &psht_project]) {
        run_cmd("incus", &["project", "create", &psht_project])?;
    }
    run_cmd(
        "incus",
        &["project", "set", &psht_project, "restricted=true"],
    )?;
    run_cmd(
        "incus",
        &[
            "project",
            "set",
            &psht_project,
            "restricted.devices.proxy=allow",
        ],
    )?;
    ensure_project_default_profile(&psht_project)?;

    eprintln!("-----> Setting up directories");
    let repos = psht_home.join("repos");
    let builds = psht_home.join("builds");
    let stacks = psht_home.join("stacks");
    fs::create_dir_all(&repos).map_err(|e| format!("failed to create {}: {e}", repos.display()))?;
    fs::create_dir_all(&builds)
        .map_err(|e| format!("failed to create {}: {e}", builds.display()))?;
    fs::create_dir_all(&stacks)
        .map_err(|e| format!("failed to create {}: {e}", stacks.display()))?;

    let repos_s = repos.to_string_lossy().to_string();
    let builds_s = builds.to_string_lossy().to_string();
    let stacks_s = stacks.to_string_lossy().to_string();
    init_stacks_in(&stacks)?;
    run_cmd("chown", &["-R", &owner, &repos_s, &builds_s, &stacks_s])?;

    eprintln!("-----> Installing host supervision daemon");
    install_supervision_units(&psht_bin_str, &psht_home)?;

    let ts_hostname = if skip_tailscale {
        hostname()
    } else {
        run_cmd_capture("tailscale", &["status", "--json"])
            .ok()
            .and_then(|json| parse_tailscale_dns_name(&json))
            .unwrap_or_else(hostname)
    };

    println!();
    println!("=====> psht is ready!");
    println!("       Containers will join your tailnet as <app>");
    println!();
    println!("Usage:");
    println!();
    println!("  cd your-app/");
    println!("  psht deploy");
    println!();
    println!("Commands:");
    println!("  ssh {psht_user}@{ts_hostname} ps");
    println!("  ssh {psht_user}@{ts_hostname} logs <app>");
    println!("  ssh {psht_user}@{ts_hostname} stop <app>");
    println!("  ssh {psht_user}@{ts_hostname} start <app>");
    println!("  ssh {psht_user}@{ts_hostname} restart <app>");
    Ok(())
}

fn server_release_url(forge_url: &str, version: &str, target: &str) -> String {
    format!("{forge_url}/releases/download/v{version}/psht-server-{version}-{target}.tar.gz")
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        let canonical = fs::canonicalize(&path).unwrap_or(path.clone());
        if deduped.iter().any(|existing: &PathBuf| {
            fs::canonicalize(existing).unwrap_or(existing.clone()) == canonical
        }) {
            continue;
        }
        deduped.push(path);
    }
    deduped
}

pub(super) fn psht_user_shell_path() -> Option<PathBuf> {
    let passwd = run_cmd_capture("getent", &["passwd", "psht"]).ok()?;
    let shell = passwd.split(':').nth(6)?.trim();
    if shell.is_empty() {
        return None;
    }
    Some(PathBuf::from(shell))
}

fn collect_server_install_targets(current_bin: &Path) -> Vec<PathBuf> {
    let mut targets = vec![current_bin.to_path_buf()];
    if let Ok(which_bin) = run_cmd_capture("which", &["psht-server"]) {
        let path = PathBuf::from(which_bin.trim());
        if path.exists() {
            targets.push(path);
        }
    }
    if let Some(shell_path) = psht_user_shell_path()
        && shell_path.exists()
    {
        targets.push(shell_path);
    }
    dedupe_paths(targets)
}

fn ensure_binary_version(path: &Path, expected: &str, label: &str) -> Result<(), String> {
    let installed = binary_version(path).unwrap_or_else(|| "unknown".to_string());
    if installed == expected {
        return Ok(());
    }
    Err(format!(
        "{label} version mismatch at {}: expected {expected}, got {installed}",
        path.display()
    ))
}

pub(super) fn install_binary_atomically(
    source: &Path,
    destination: &Path,
    mode: u32,
    label: &str,
) -> Result<(), String> {
    if !source.is_file() {
        return Err(format!(
            "failed to install {label}: source {} does not exist",
            source.display()
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "failed to install {label}: destination has no parent: {}",
            destination.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "failed to install {label}: invalid destination file name: {}",
                destination.display()
            )
        })?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staged = parent.join(format!(
        ".{file_name}.psht-staged-{}-{unique}",
        std::process::id()
    ));

    let result = (|| -> Result<(), String> {
        fs::copy(source, &staged).map_err(|e| {
            format!(
                "failed to stage {label} binary {} -> {}: {e}",
                source.display(),
                staged.display()
            )
        })?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("failed to chmod staged {}: {e}", staged.display()))?;
        fs::rename(&staged, destination).map_err(|e| {
            format!(
                "failed to install {label} binary to {}: {e}",
                destination.display()
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

pub fn upgrade_server() -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht-server upgrade".to_string());
    }

    let current_bin = current_psht_binary()?;
    let current_version = binary_version(&current_bin).ok_or_else(|| {
        format!(
            "failed to detect current version from {}",
            current_bin.display()
        )
    })?;
    let latest = latest_release_version()?;
    let target = detect_release_target()?;
    let forge_url = configured_forge_url();
    let install_targets = collect_server_install_targets(&current_bin);

    if !version_is_newer(&latest, &current_version) {
        let all_targets_match = install_targets
            .iter()
            .all(|path| binary_matches_version(path, &latest));
        if all_targets_match {
            println!("psht {latest} (up to date)");
            return Ok(());
        }
    }

    eprintln!("-----> Upgrading psht {current_version} -> {latest}");
    let tmpdir = run_cmd_capture("mktemp", &["-d"])?;
    let tmpdir_path = PathBuf::from(tmpdir);
    let tmpdir_s = tmpdir_path.to_string_lossy().to_string();
    let server_tar = tmpdir_path.join("psht-server.tar.gz");
    let cli_tar = tmpdir_path.join("psht.tar.gz");
    let server_tar_s = server_tar.to_string_lossy().to_string();
    let cli_tar_s = cli_tar.to_string_lossy().to_string();

    let result = (|| {
        let server_url = server_release_url(&forge_url, &latest, target);
        let cli_url = cli_release_url(&forge_url, &latest, target);

        eprintln!("-----> Downloading release artifacts");
        run_cmd_quiet("curl", &["-fsSL", &server_url, "-o", &server_tar_s])
            .map_err(|e| format!("download failed: {server_url}: {e}"))?;
        run_cmd_quiet("curl", &["-fsSL", &cli_url, "-o", &cli_tar_s])
            .map_err(|e| format!("download failed: {cli_url}: {e}"))?;

        run_cmd_quiet("tar", &["xzf", &server_tar_s, "-C", &tmpdir_s])?;
        run_cmd_quiet("tar", &["xzf", &cli_tar_s, "-C", &tmpdir_s])?;

        let server_candidate = tmpdir_path.join("psht-server");
        let cli_candidate = tmpdir_path.join("psht");
        if !server_candidate.is_file() {
            return Err("release tarball missing psht-server binary".to_string());
        }
        if !cli_candidate.is_file() {
            return Err("release tarball missing psht binary".to_string());
        }
        ensure_binary_version(&server_candidate, &latest, "downloaded psht-server")?;
        ensure_binary_version(&cli_candidate, &latest, "downloaded psht")?;

        eprintln!("-----> Installing server binary");
        for target_path in &install_targets {
            install_binary_atomically(&server_candidate, target_path, 0o755, "psht-server")?;
            ensure_binary_version(target_path, &latest, "installed psht-server")?;
        }

        eprintln!("-----> Installing CLI binary");
        let psht_cli_dst = home_dir().join("bin/psht");
        install_binary_atomically(&cli_candidate, &psht_cli_dst, 0o755, "psht")?;
        let _ = run_cmd("chown", &["psht:psht", &psht_cli_dst.to_string_lossy()]);
        ensure_binary_version(&psht_cli_dst, &latest, "installed psht")?;

        eprintln!("-----> Updating incus");
        run_cmd("apt-get", &["update", "-qq"])?;
        run_cmd("apt-get", &["install", "-y", "-qq", "incus"])?;

        eprintln!("-----> Refreshing stacks");
        init_stacks_in(&stacks_dir())?;
        let _ = run_cmd(
            "chown",
            &["-R", "psht:psht", &stacks_dir().to_string_lossy()],
        );
        Ok(())
    })();

    let _ = fs::remove_dir_all(&tmpdir_path);
    result?;
    println!("=====> psht upgraded to {latest}");
    Ok(())
}

pub(super) fn cleanup_all_owned_tailscale_devices(app: &str) -> Result<(), String> {
    let tracked = sqlite_store::list_active_owned_tailscale_devices(app)?;
    if tracked.is_empty() {
        return Ok(());
    }

    let mut errors = Vec::new();
    match tailscale::tailnet_access_token() {
        Ok(token) => match tailscale::list_tailnet_devices(&token) {
            Ok(devices) => {
                let mut devices_by_id = BTreeMap::new();
                for device in devices {
                    devices_by_id.insert(device.id.clone(), device);
                }

                for row in &tracked {
                    let Some(device) = devices_by_id.get(&row.device_id) else {
                        continue;
                    };
                    if !device.tags.iter().any(|tag| tag == "tag:psht") {
                        continue;
                    }
                    if let Err(err) = tailscale::delete_tailnet_device(&token, &row.device_id) {
                        errors.push(format!("{}: {err}", row.device_id));
                    }
                }
            }
            Err(err) => errors.push(format!("failed to list tailnet devices: {err}")),
        },
        Err(err) => errors.push(format!("failed to acquire tailnet token: {err}")),
    }

    if let Err(err) = sqlite_store::retire_all_owned_tailscale_devices(app) {
        errors.push(format!("failed to retire owned device rows: {err}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join(" | "))
    }
}

fn resolve_active_app_for_tailscale(app: &str) -> Result<String, String> {
    resolve_existing_active_app_ref(app)
}

fn ensure_container_running_for_tailscale(app: &str) -> Result<String, String> {
    let active_app = resolve_active_app_for_tailscale(app)?;
    if container::is_running(&active_app)? {
        Ok(active_app)
    } else {
        Err(format!("app '{app}' is not running"))
    }
}

pub fn tailscale_status(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = ensure_container_running_for_tailscale(app)?;
    let status = container::exec_output(&active_app, "tailscale status --json")?;
    let summary = tailscale_self_status_summary_from_json(app, &status)?;
    println!("{summary}");
    Ok(())
}

pub(super) fn join_tailscale_for_repair_with_fallback<FStateJoin, FAuthJoin>(
    mut join_with_state: FStateJoin,
    mut join_with_auth_key: FAuthJoin,
) -> Result<(Option<String>, &'static str), String>
where
    FStateJoin: FnMut() -> Result<Option<String>, String>,
    FAuthJoin: FnMut() -> Result<Option<String>, String>,
{
    match join_with_state() {
        Ok(Some(name)) => Ok((Some(name), "state")),
        Ok(None) => {
            eprintln!(
                "       State-based tailscale recovery produced no DNS name; falling back to auth-key tailscale join"
            );
            Ok((join_with_auth_key()?, "auth_key"))
        }
        Err(state_err) => {
            eprintln!("       State-based tailscale join failed: {state_err}");
            eprintln!("       Falling back to auth-key tailscale join");
            Ok((join_with_auth_key()?, "auth_key"))
        }
    }
}

pub fn tailscale_up(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_active_app_for_tailscale(app)?;
    if !container::is_running(&active_app)? {
        eprintln!("-----> Starting container");
        container::start(&active_app)?;
    }

    eprintln!("-----> Repairing tailscale in container");
    let (tailscale_pool, tailscale_state_volume) = ensure_app_tailscale_volume(app)?;
    tailscale::install_in_container(&active_app)?;
    let _ = container::exec_cmd(&active_app, "tailscale down >/dev/null 2>&1 || true");
    let _ = container::exec_cmd(
        &active_app,
        "systemctl stop tailscaled >/dev/null 2>&1 || true",
    );
    seed_tailscale_state_volume_from_container(
        &active_app,
        &tailscale_pool,
        &tailscale_state_volume,
    )?;
    container::ensure_tailscale_state_mount(&active_app, &tailscale_pool, &tailscale_state_volume)?;
    let (tailnet_hostname, created_via) = join_tailscale_for_repair_with_fallback(
        || tailscale::join_with_state_in_container(&active_app, app),
        || tailscale::join_with_auth_key_in_container(&active_app, app),
    )?;
    if let Err(track_err) = track_owned_tailscale_device(app, &active_app, created_via) {
        eprintln!("       Warning: failed to track tailscale device ownership: {track_err}");
    }
    let (_, _, health) =
        wait_for_tailscale_online(&active_app, Duration::from_secs(TAILSCALE_ONLINE_WAIT_SECS))?;
    if !health.is_empty() {
        eprintln!("       Warning: {}", health.join(" | "));
    }
    let _ = container::exec_cmd(&active_app, "tailscale serve reset >/dev/null 2>&1 || true");
    let port = allocate_port(app);
    if let Err(e) = tailscale::expose_http_in_container(&active_app, port) {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    if let Some(ref name) = tailnet_hostname
        && !tailscale_hostname_is_exact(name, app)
    {
        eprintln!(
            "       Warning: tailscale hostname '{name}' does not match requested app label '{app}'"
        );
    }
    if let Some(name) = tailnet_hostname {
        eprintln!("=====> Tailscale ready: http://{name} (host port :{port} on server)");
    } else {
        eprintln!("=====> Tailscale repaired for {app}");
    }
    Ok(())
}

pub fn tailscale_down(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = ensure_container_running_for_tailscale(app)?;
    eprintln!("-----> Bringing tailscale down in container");
    container::exec_cmd(&active_app, "tailscale down")
}

fn enforce_supervised_app_running(app: &str, project: &str) -> Result<(), String> {
    let active_app = resolve_existing_active_app_ref(app)?;
    let port = allocate_port(app);
    if !container::is_running(&active_app)? {
        eprintln!("-----> Supervise: starting container for {app}");
        container::start(&active_app)?;
    }

    match probe_app_service(&active_app, port) {
        Ok(probe) if probe.is_ready() => {
            let mut keep = BTreeSet::new();
            keep.insert(instance_name_from_app_ref(&active_app));
            reconcile_family_instances_strict(app, &keep, project)?;
            return Ok(());
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("       Warning: failed to inspect app service for {app}: {err}");
        }
    }

    eprintln!("-----> Supervise: starting app service for {app}");
    let vars = read_env_vars(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, &vars)?;
    let command = read_start_command(&active_app)?;
    launch_app_process(&active_app, port, &command, &vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    let mut keep = BTreeSet::new();
    keep.insert(instance_name_from_app_ref(&active_app));
    reconcile_family_instances_strict(app, &keep, project)?;
    eprintln!("=====> Supervise recovered {app}");
    Ok(())
}

fn enforce_supervised_app_stopped(app: &str, project: &str) -> Result<(), String> {
    let Some(active_app) = resolve_active_app_ref(app)? else {
        return Ok(());
    };
    if !container::is_running(&active_app)? {
        let mut keep = BTreeSet::new();
        keep.insert(instance_name_from_app_ref(&active_app));
        reconcile_family_instances_strict(app, &keep, project)?;
        return Ok(());
    }
    eprintln!("-----> Supervise: stopping {app} (desired state is stopped)");
    let port = allocate_port(app);
    let _ = stop_app_process_on_port(&active_app, port);
    container::stop(&active_app)?;
    let mut keep = BTreeSet::new();
    keep.insert(instance_name_from_app_ref(&active_app));
    reconcile_family_instances_strict(app, &keep, project)
}

fn repair_phase_after_supervise(
    app: &str,
    converged: bool,
    repair_error: Option<&str>,
) -> Result<(), String> {
    let _ = crate::control_plane::repair_stale_phase(
        app,
        control_plane_snapshot(app),
        converged,
        repair_error,
        (reconcile_lease_ttl_secs() * 1_000) as i64,
    )?;
    Ok(())
}

pub fn daemon() -> Result<(), String> {
    let lock_path = deploy_lock_path(SUPERVISE_DAEMON_LOCK_APP);
    let Some(_guard) = try_acquire_deploy_lock_at(&lock_path)? else {
        return Err("psht supervision daemon is already running".to_string());
    };

    eprintln!("-----> Starting psht supervision daemon");
    loop {
        if let Err(err) = refresh_deploy_lock_heartbeat_at(&lock_path, std::process::id()) {
            eprintln!("       Warning: failed to refresh daemon lock heartbeat: {err}");
        }
        match supervise() {
            Ok(()) => thread::sleep(Duration::from_secs(SUPERVISE_DAEMON_INTERVAL_SECS)),
            Err(err) => {
                eprintln!("       Warning: supervision pass failed: {err}");
                thread::sleep(Duration::from_secs(SUPERVISE_DAEMON_ERROR_BACKOFF_SECS));
            }
        }
    }
}

pub fn supervise() -> Result<(), String> {
    let states = read_managed_app_runtime_states()?;
    if states.is_empty() {
        return Ok(());
    }
    let project = current_project_name()?;

    let mut failures = Vec::new();
    for (app, _) in states {
        match container::has_running_operation(&app) {
            Ok(true) => {
                eprintln!("       Supervise: skipping {app} while container operation is active");
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!(
                    "       Warning: failed to inspect container operations for {app}: {err}"
                );
            }
        }

        let desired = app_desired_state(&app)?;
        let result = if desired == DESIRED_STATE_STOPPED {
            enforce_supervised_app_stopped(&app, &project)
        } else {
            enforce_supervised_app_running(&app, &project)
        };
        if let Err(err) = repair_phase_after_supervise(
            &app,
            result.is_ok(),
            result.as_ref().err().map(|err| err.as_str()),
        ) {
            eprintln!("       Warning: failed to repair stale phase for {app}: {err}");
        }
        if let Err(err) = result {
            eprintln!("       Warning: supervise reconciliation failed for {app}: {err}");
            failures.push(format!("{app}: {err}"));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("supervise failures: {}", failures.join("; ")))
    }
}
