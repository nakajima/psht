use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::container;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetDevice {
    pub id: String,
    pub hostname_label: Option<String>,
    pub dns_name: Option<String>,
    pub tags: Vec<String>,
}

fn credentials_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string());
    PathBuf::from(home).join(".config/tailscale-oauth")
}

fn read_credentials_from(path: &std::path::Path) -> Result<(String, String), String> {
    let contents = fs::read_to_string(path)
        .map_err(|_| "tailscale OAuth not configured — run `psht bootstrap`".to_string())?;

    let mut client_id = None;
    let mut client_secret = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "TS_OAUTH_CLIENT_ID" => client_id = Some(value.trim().to_string()),
                "TS_OAUTH_CLIENT_SECRET" => client_secret = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }

    let client_id = client_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing TS_OAUTH_CLIENT_ID in tailscale OAuth config".to_string())?;
    let client_secret = client_secret
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing TS_OAUTH_CLIENT_SECRET in tailscale OAuth config".to_string())?;

    Ok((client_id, client_secret))
}

pub fn read_credentials() -> Result<(String, String), String> {
    read_credentials_from(&credentials_path())
}

fn parse_oauth_token(json: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse OAuth response: {e}"))?;
    value["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "missing access_token in OAuth response".to_string())
}

fn parse_self_dns_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = value["Self"]["DNSName"].as_str()?;
    Some(name.trim_end_matches('.').to_string())
}

fn parse_backend_state(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value["BackendState"].as_str().map(|s| s.to_string())
}

fn parse_auth_key(json: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("failed to parse auth key response: {e}"))?;
    value["key"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "missing key in auth key response".to_string())
}

fn value_as_nonempty_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_tailnet_device(value: &serde_json::Value) -> Option<TailnetDevice> {
    let serde_json::Value::Object(obj) = value else {
        return None;
    };
    let id = obj
        .get("id")
        .and_then(value_as_nonempty_string)
        .or_else(|| obj.get("nodeId").and_then(value_as_nonempty_string))
        .or_else(|| obj.get("deviceId").and_then(value_as_nonempty_string))?;

    let hostname_label = obj
        .get("hostname")
        .and_then(value_as_nonempty_string)
        .map(|hostname| hostname.trim_end_matches('.').to_string())
        .and_then(|hostname| {
            let trimmed = hostname.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

    let dns_name = obj
        .get("name")
        .and_then(value_as_nonempty_string)
        .or_else(|| obj.get("dnsName").and_then(value_as_nonempty_string))
        .map(|name| name.trim_end_matches('.').to_string())
        .and_then(|name| {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

    let tags = obj
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(value_as_nonempty_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(TailnetDevice {
        id,
        hostname_label,
        dns_name,
        tags,
    })
}

fn parse_tailnet_devices(json: &str) -> Result<Vec<TailnetDevice>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("failed to parse tailscale device list response: {e}"))?;
    let candidates = match &value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(obj) => obj
            .get("devices")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "missing devices array in tailscale device list response".to_string())?,
        _ => {
            return Err("invalid tailscale device list response".to_string());
        }
    };

    let mut out = Vec::new();
    for candidate in candidates {
        if let Some(device) = parse_tailnet_device(candidate) {
            out.push(device);
        }
    }
    Ok(out)
}

fn oauth_token_request_args(client_id: &str, client_secret: &str) -> Vec<String> {
    vec![
        "-s".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "--data-urlencode".to_string(),
        format!("client_id={client_id}"),
        "--data-urlencode".to_string(),
        format!("client_secret={client_secret}"),
        "https://api.tailscale.com/api/v2/oauth/token".to_string(),
    ]
}

pub fn oauth_token(client_id: &str, client_secret: &str) -> Result<String, String> {
    let args = oauth_token_request_args(client_id, client_secret);
    let output = Command::new("curl")
        .args(&args)
        .output()
        .map_err(|e| format!("failed to request OAuth token: {e}"))?;

    if !output.status.success() {
        return Err("curl failed requesting OAuth token".to_string());
    }

    parse_oauth_token(&String::from_utf8_lossy(&output.stdout))
}

pub fn create_auth_key(token: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "capabilities": {
            "devices": {
                "create": {
                    "reusable": false,
                    "ephemeral": true,
                    "preauthorized": true,
                    "tags": ["tag:psht"]
                }
            }
        }
    });

    let output = Command::new("curl")
        .args(["-s", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &body.to_string()])
        .arg("https://api.tailscale.com/api/v2/tailnet/-/keys")
        .output()
        .map_err(|e| format!("failed to create auth key: {e}"))?;

    let response = String::from_utf8_lossy(&output.stdout).to_string();

    if !output.status.success() {
        return Err(format!(
            "failed to create tailscale auth key: {response}\n       \
             Ensure tag:psht exists: https://login.tailscale.com/admin/acls/visual/tags/add"
        ));
    }

    parse_auth_key(&response)
}

pub fn auth_key() -> Result<String, String> {
    let (client_id, client_secret) = read_credentials()?;
    let token = oauth_token(&client_id, &client_secret)?;
    create_auth_key(&token)
}

pub fn tailnet_access_token() -> Result<String, String> {
    let (client_id, client_secret) = read_credentials()?;
    oauth_token(&client_id, &client_secret)
}

pub fn list_tailnet_devices(token: &str) -> Result<Vec<TailnetDevice>, String> {
    let output = Command::new("curl")
        .args(["-s", "-X", "GET"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .arg("https://api.tailscale.com/api/v2/tailnet/-/devices")
        .output()
        .map_err(|e| format!("failed to list tailnet devices: {e}"))?;
    let response = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        return Err(format!("failed to list tailscale devices: {response}"));
    }
    parse_tailnet_devices(&response)
}

pub fn delete_tailnet_device(token: &str, device_id: &str) -> Result<(), String> {
    let output = Command::new("curl")
        .args(["-s", "-X", "DELETE"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .arg(format!(
            "https://api.tailscale.com/api/v2/device/{device_id}"
        ))
        .output()
        .map_err(|e| format!("failed to delete tailscale device {device_id}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let response = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "failed to delete tailscale device {device_id}: {response}"
    ))
}

pub fn install_in_container(app: &str) -> Result<(), String> {
    container::exec_cmd_rolling(
        app,
        "command -v tailscale || (apt-get update -qq && apt-get install -y -qq curl && curl -fsSL https://tailscale.com/install.sh | sh)",
        5,
    )?;
    container::exec_cmd(app, "systemctl enable tailscaled")
}

pub fn dns_name_in_container(app: &str) -> Option<String> {
    container::exec_output(app, "tailscale status --json")
        .ok()
        .and_then(|json| parse_self_dns_name(&json))
}

fn tailscale_up_with_auth_key_command(auth_key: &str, machine_name: &str) -> String {
    format!("tailscale up --auth-key {auth_key} --hostname {machine_name} --ssh")
}

fn join_with_up_command(container_app: &str, up_command: &str) -> Result<Option<String>, String> {
    container::exec_cmd(container_app, "systemctl start tailscaled")?;
    container::exec_cmd(container_app, up_command)?;

    let ts_hostname = dns_name_in_container(container_app);
    match ts_hostname {
        Some(ref name) => eprintln!("       Joined tailnet as {name}"),
        None => eprintln!("       Joined tailnet"),
    }

    Ok(ts_hostname)
}

pub fn join_with_auth_key_in_container(
    container_app: &str,
    machine_name: &str,
) -> Result<Option<String>, String> {
    let key = auth_key()?;
    let up_command = tailscale_up_with_auth_key_command(&key, machine_name);
    join_with_up_command(container_app, &up_command)
}

pub fn join_with_state_in_container(
    container_app: &str,
    machine_name: &str,
) -> Result<Option<String>, String> {
    container::exec_cmd(container_app, "systemctl start tailscaled")?;

    let status_json =
        container::exec_output(container_app, "tailscale status --json").map_err(|e| {
            format!("failed to read tailscale state from container '{container_app}': {e}")
        })?;
    if let Some(state) = parse_backend_state(&status_json)
        && (state == "NeedsLogin" || state == "NoState")
    {
        return Err(format!("tailscale state requires login (state: {state})"));
    }

    // Prefer non-interactive settings update to avoid blocking on login prompts.
    container::exec_cmd(
        container_app,
        &format!("tailscale set --hostname {machine_name} --ssh >/dev/null 2>&1 || true"),
    )?;

    let ts_hostname = dns_name_in_container(container_app);
    match ts_hostname {
        Some(ref name) => eprintln!("       Joined tailnet as {name}"),
        None => eprintln!("       Joined tailnet"),
    }

    Ok(ts_hostname)
}

fn serve_http_command(port: u16) -> String {
    format!(
        "tailscale serve --bg --http=80 http://127.0.0.1:{port} >/dev/null 2>&1 || \
tailscale serve --bg --http=80 / http://127.0.0.1:{port} >/dev/null 2>&1 || \
tailscale serve --bg http://127.0.0.1:{port} >/dev/null 2>&1 || \
tailscale serve --bg {port}"
    )
}

pub fn expose_http_in_container(app: &str, port: u16) -> Result<(), String> {
    container::exec_cmd(app, &serve_http_command(port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn read_credentials_parses_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tailscale-oauth");
        fs::write(
            &path,
            "TS_OAUTH_CLIENT_ID=my-client-id\nTS_OAUTH_CLIENT_SECRET=my-secret\n",
        )
        .unwrap();

        let (id, secret) = read_credentials_from(&path).unwrap();
        assert_eq!(id, "my-client-id");
        assert_eq!(secret, "my-secret");
    }

    #[test]
    fn read_credentials_missing_file_errors() {
        let result = read_credentials_from(std::path::Path::new("/nonexistent/tailscale-oauth"));
        let err = result.unwrap_err();
        assert!(
            err.contains("not configured"),
            "expected 'not configured' error, got: {err}"
        );
    }

    #[test]
    fn read_credentials_missing_var_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tailscale-oauth");
        fs::write(&path, "TS_OAUTH_CLIENT_ID=my-client-id\n").unwrap();

        let err = read_credentials_from(&path).unwrap_err();
        assert!(
            err.contains("TS_OAUTH_CLIENT_SECRET"),
            "expected missing secret error, got: {err}"
        );
    }

    #[test]
    fn read_credentials_empty_var_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tailscale-oauth");
        fs::write(
            &path,
            "TS_OAUTH_CLIENT_ID=\nTS_OAUTH_CLIENT_SECRET=my-secret\n",
        )
        .unwrap();

        let err = read_credentials_from(&path).unwrap_err();
        assert!(
            err.contains("TS_OAUTH_CLIENT_ID"),
            "expected missing client ID error, got: {err}"
        );
    }

    #[test]
    fn read_credentials_ignores_comments_and_blanks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tailscale-oauth");
        fs::write(
            &path,
            "# Tailscale OAuth credentials\n\nTS_OAUTH_CLIENT_ID=cid\n\nTS_OAUTH_CLIENT_SECRET=csec\n",
        )
        .unwrap();

        let (id, secret) = read_credentials_from(&path).unwrap();
        assert_eq!(id, "cid");
        assert_eq!(secret, "csec");
    }

    #[test]
    fn parse_oauth_token_response() {
        let json = r#"{"access_token":"tok123","token_type":"Bearer","expires_in":3600}"#;
        let token = parse_oauth_token(json).unwrap();
        assert_eq!(token, "tok123");
    }

    #[test]
    fn oauth_token_request_uses_urlencoding() {
        let args = oauth_token_request_args("id@x", "sec&ret");
        assert!(args.contains(&"--data-urlencode".to_string()));
        assert!(args.contains(&"client_id=id@x".to_string()));
        assert!(args.contains(&"client_secret=sec&ret".to_string()));
    }

    #[test]
    fn parse_auth_key_response() {
        let json = r#"{"key":"tskey-auth-xxx","created":"2024-01-01T00:00:00Z"}"#;
        let key = parse_auth_key(json).unwrap();
        assert_eq!(key, "tskey-auth-xxx");
    }

    #[test]
    fn parse_oauth_token_error_response() {
        let json = r#"{"error":"invalid_client"}"#;
        let err = parse_oauth_token(json).unwrap_err();
        assert!(
            err.contains("missing access_token"),
            "expected missing token error, got: {err}"
        );
    }

    #[test]
    fn parse_oauth_token_invalid_json() {
        let err = parse_oauth_token("not json").unwrap_err();
        assert!(
            err.contains("failed to parse"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn parse_auth_key_missing_field() {
        let json = r#"{"id":"something"}"#;
        let err = parse_auth_key(json).unwrap_err();
        assert!(
            err.contains("missing key"),
            "expected missing key error, got: {err}"
        );
    }

    #[test]
    fn parse_self_dns_name_from_status() {
        let json =
            r#"{"Self":{"DNSName":"psht-test.tail1234.ts.net.","TailscaleIPs":["100.64.0.1"]}}"#;
        let name = parse_self_dns_name(json).unwrap();
        assert_eq!(name, "psht-test.tail1234.ts.net");
    }

    #[test]
    fn parse_self_dns_name_with_suffix() {
        let json = r#"{"Self":{"DNSName":"psht-test-1.tail1234.ts.net."}}"#;
        let name = parse_self_dns_name(json).unwrap();
        assert_eq!(name, "psht-test-1.tail1234.ts.net");
    }

    #[test]
    fn parse_self_dns_name_missing_self() {
        let json = r#"{"Version":"1.0"}"#;
        assert!(parse_self_dns_name(json).is_none());
    }

    #[test]
    fn parse_backend_state_from_status() {
        let json = r#"{"BackendState":"Running","Self":{"DNSName":"psht-test.tail1234.ts.net."}}"#;
        assert_eq!(parse_backend_state(json).as_deref(), Some("Running"));
    }

    #[test]
    fn serve_http_command_includes_expected_port_and_fallbacks() {
        let cmd = serve_http_command(3233);
        assert!(
            cmd.contains("--http=80"),
            "should attempt explicit port 80 mapping"
        );
        assert!(
            cmd.contains("http://127.0.0.1:3233"),
            "should map to localhost app port"
        );
        assert!(cmd.contains("tailscale serve --bg 3233"), "should fallback");
    }

    #[test]
    fn tailscale_up_command_uses_requested_machine_name() {
        let cmd = tailscale_up_with_auth_key_command("tskey-auth-abc", "hyperlinked");
        assert_eq!(
            cmd,
            "tailscale up --auth-key tskey-auth-abc --hostname hyperlinked --ssh"
        );
    }

    #[test]
    fn parse_backend_state_missing_returns_none() {
        let json = r#"{"Self":{"DNSName":"psht-test.tail1234.ts.net."}}"#;
        assert!(parse_backend_state(json).is_none());
    }

    #[test]
    fn parse_tailnet_devices_object_response() {
        let json = r#"{
          "devices": [
            {
              "id": "dev1",
              "hostname": "hyperlinked-1",
              "name": "hyperlinked-1.tail.ts.net.",
              "tags": ["tag:psht"]
            }
          ]
        }"#;
        let devices = parse_tailnet_devices(json).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "dev1");
        assert_eq!(devices[0].hostname_label.as_deref(), Some("hyperlinked-1"));
        assert_eq!(
            devices[0].dns_name.as_deref(),
            Some("hyperlinked-1.tail.ts.net")
        );
        assert_eq!(devices[0].tags, vec!["tag:psht".to_string()]);
    }

    #[test]
    fn parse_tailnet_devices_array_response() {
        let json = r#"[
          {
            "nodeId": "123",
            "hostname": "hyperlinked",
            "dnsName": "hyperlinked.tail.ts.net",
            "tags": []
          }
        ]"#;
        let devices = parse_tailnet_devices(json).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "123");
        assert_eq!(devices[0].hostname_label.as_deref(), Some("hyperlinked"));
        assert_eq!(
            devices[0].dns_name.as_deref(),
            Some("hyperlinked.tail.ts.net")
        );
    }
}
