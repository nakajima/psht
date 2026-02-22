use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::container;

fn credentials_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string());
    PathBuf::from(home).join(".config/tailscale-oauth")
}

fn read_credentials_from(path: &std::path::Path) -> Result<(String, String), String> {
    let contents = fs::read_to_string(path)
        .map_err(|_| "tailscale OAuth not configured — run bootstrap.sh".to_string())?;

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

fn parse_auth_key(json: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("failed to parse auth key response: {e}"))?;
    value["key"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "missing key in auth key response".to_string())
}

pub fn oauth_token(client_id: &str, client_secret: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-s", "-X", "POST"])
        .args([
            "-d",
            &format!("client_id={client_id}&client_secret={client_secret}"),
        ])
        .arg("https://api.tailscale.com/api/v2/oauth/token")
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

pub fn install_in_container(app: &str) -> Result<(), String> {
    container::exec_cmd_rolling(
        app,
        "command -v tailscale || (apt-get update -qq && apt-get install -y -qq curl && curl -fsSL https://tailscale.com/install.sh | sh)",
        5,
    )?;
    container::exec_cmd(app, "systemctl enable tailscaled")
}

pub fn join_in_container(app: &str) -> Result<(), String> {
    let key = auth_key()?;
    container::exec_cmd(app, "systemctl start tailscaled")?;
    container::exec_cmd(
        app,
        &format!("tailscale up --auth-key {key} --hostname {app} --ssh"),
    )?;

    let ts_hostname = container::exec_output(app, "tailscale status --json")
        .ok()
        .and_then(|json| parse_self_dns_name(&json));
    match ts_hostname {
        Some(name) => eprintln!("       Joined tailnet as {name}"),
        None => eprintln!("       Joined tailnet"),
    }

    Ok(())
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
}
