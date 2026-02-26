use std::env;
use std::process::Command;

fn route_id(app: &str) -> String {
    format!("psht-{app}")
}

fn route_hostname(app: &str, domain: &str) -> String {
    format!("{app}.{domain}")
}

fn validate_route_app_label(app: &str) -> Result<(), String> {
    if app.is_empty() {
        return Err("invalid app name for Caddy route host: empty".to_string());
    }
    if app.len() > 63 {
        return Err(format!(
            "invalid app name for Caddy route host: '{app}' exceeds 63 characters"
        ));
    }
    if !app.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!(
            "invalid app name for Caddy route host '{app}': only [A-Za-z0-9-] are allowed"
        ));
    }
    let first = app.as_bytes()[0];
    let last = app.as_bytes()[app.len() - 1];
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return Err(format!(
            "invalid app name for Caddy route host '{app}': must start and end with [A-Za-z0-9]"
        ));
    }
    Ok(())
}

fn route_json(app: &str, domain: &str, port: u16) -> String {
    let id = route_id(app);
    let host = route_hostname(app, domain);
    format!(
        r#"{{"@id":"{}","match":[{{"host":["{}"]}}],"handle":[{{"handler":"reverse_proxy","upstreams":[{{"dial":"{}:{}"}}]}}]}}"#,
        id, host, app, port
    )
}

fn routes_url(api_url: &str) -> String {
    format!("{api_url}/config/apps/http/servers/srv0/routes")
}

fn config_from_values(
    api_url: Option<&str>,
    domain: Option<&str>,
) -> Result<Option<(String, String)>, String> {
    match api_url {
        None => Ok(None),
        Some(url) => match domain {
            Some(d) => Ok(Some((url.to_string(), d.to_string()))),
            None => Err("CADDY_API_URL is set but CADDY_DOMAIN is missing".to_string()),
        },
    }
}

fn config_from_env() -> Result<Option<(String, String)>, String> {
    let api_url = env::var("CADDY_API_URL").ok();
    let domain = env::var("CADDY_DOMAIN").ok();
    config_from_values(api_url.as_deref(), domain.as_deref())
}

fn remove_config_from_values(api_url: Option<&str>) -> Option<String> {
    api_url.map(|url| url.to_string())
}

fn remove_config_from_env() -> Option<String> {
    let api_url = env::var("CADDY_API_URL").ok();
    remove_config_from_values(api_url.as_deref())
}

fn fetch_routes(api_url: &str) -> Result<Vec<serde_json::Value>, String> {
    let output = Command::new("curl")
        .args(["-sf", &routes_url(api_url)])
        .output()
        .map_err(|e| format!("failed to fetch caddy routes: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("404") {
            return Ok(Vec::new());
        }
        return Err(format!("failed to fetch caddy routes: {}", stderr.trim()));
    }

    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse caddy routes response: {e}"))?;
    match value {
        serde_json::Value::Array(routes) => Ok(routes),
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Object(map) if map.is_empty() => Ok(Vec::new()),
        serde_json::Value::Object(map) => {
            let mut indexed = Vec::new();
            for (key, value) in map {
                let idx = key.parse::<usize>().map_err(|_| {
                    format!("unexpected caddy routes object key: {key} (expected array index)")
                })?;
                indexed.push((idx, value));
            }
            indexed.sort_by_key(|(idx, _)| *idx);
            Ok(indexed.into_iter().map(|(_, value)| value).collect())
        }
        _ => Err("unexpected caddy routes response: expected JSON array-like value".to_string()),
    }
}

fn upsert_route_list(
    mut routes: Vec<serde_json::Value>,
    route: serde_json::Value,
    id: &str,
) -> Vec<serde_json::Value> {
    routes.retain(|existing| existing.get("@id").and_then(|v| v.as_str()) != Some(id));
    routes.insert(0, route);
    routes
}

fn delete_route(api_url: &str, app: &str) -> Result<(), String> {
    let url = format!("{}/id/{}", api_url, route_id(app));
    let output = Command::new("curl")
        .args(["-sf", "-X", "DELETE", &url])
        .output()
        .map_err(|e| format!("failed to delete caddy route for {app}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("404") {
            return Ok(());
        }
        return Err(format!(
            "failed to delete caddy route for {app}: {}",
            stderr.trim()
        ));
    }
    Ok(())
}

fn add_route(api_url: &str, app: &str, domain: &str, port: u16) -> Result<(), String> {
    // Fetch + update the full routes list because Caddy 2.6 expects a RouteList payload.
    let url = routes_url(api_url);
    let id = route_id(app);
    let route: serde_json::Value = serde_json::from_str(&route_json(app, domain, port))
        .map_err(|e| format!("failed to encode caddy route payload: {e}"))?;
    let routes = upsert_route_list(fetch_routes(api_url)?, route, &id);
    let body = serde_json::to_string(&routes)
        .map_err(|e| format!("failed to serialize caddy routes payload: {e}"))?;

    let output = Command::new("curl")
        .args([
            "-sf",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            &url,
        ])
        .output()
        .map_err(|e| format!("failed to add caddy route: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "failed to add caddy route for {app}: {}",
            stderr.trim()
        ));
    }
    Ok(())
}

pub fn add(app: &str, port: u16) -> Result<(), String> {
    let (api_url, domain) = match config_from_env()? {
        Some(cfg) => cfg,
        None => return Ok(()),
    };
    validate_route_app_label(app)?;
    add_route(&api_url, app, &domain, port)?;
    eprintln!("       Route: {}", route_hostname(app, &domain));
    Ok(())
}

pub fn remove(app: &str) -> Result<(), String> {
    let api_url = match remove_config_from_env() {
        Some(url) => url,
        None => return Ok(()),
    };
    delete_route(&api_url, app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_id_format() {
        assert_eq!(route_id("myapp"), "psht-myapp");
        assert_eq!(route_id("web-api"), "psht-web-api");
    }

    #[test]
    fn route_hostname_format() {
        assert_eq!(route_hostname("myapp", "example.com"), "myapp.example.com");
    }

    #[test]
    fn validate_route_app_label_accepts_dns_label() {
        assert!(validate_route_app_label("my-app-1").is_ok());
    }

    #[test]
    fn validate_route_app_label_rejects_underscore_and_dot() {
        let err = validate_route_app_label("my_app.v1").unwrap_err();
        assert!(err.contains("[A-Za-z0-9-]"));
    }

    #[test]
    fn route_json_has_correct_id() {
        let json = route_json("myapp", "example.com", 3042);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["@id"], "psht-myapp");
    }

    #[test]
    fn route_json_matches_host() {
        let json = route_json("myapp", "example.com", 3042);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["match"][0]["host"][0], "myapp.example.com");
    }

    #[test]
    fn route_json_has_reverse_proxy_handler() {
        let json = route_json("myapp", "example.com", 3042);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["handle"][0]["handler"], "reverse_proxy");
    }

    #[test]
    fn route_json_dials_correct_upstream() {
        let json = route_json("myapp", "example.com", 3042);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["handle"][0]["upstreams"][0]["dial"], "myapp:3042");
    }

    #[test]
    fn route_json_uses_app_not_container_name() {
        let json = route_json("myapp", "example.com", 3042);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dial = v["handle"][0]["upstreams"][0]["dial"].as_str().unwrap();
        assert!(
            !dial.starts_with("psht-"),
            "upstream dial should use app name, not container name"
        );
    }

    #[test]
    fn config_skips_when_no_api_url() {
        let result = config_from_values(None, Some("example.com")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn config_returns_both_when_set() {
        let result =
            config_from_values(Some("http://localhost:2019"), Some("example.com")).unwrap();
        assert_eq!(
            result,
            Some((
                "http://localhost:2019".to_string(),
                "example.com".to_string()
            ))
        );
    }

    #[test]
    fn config_errors_when_api_set_but_domain_missing() {
        let result = config_from_values(Some("http://localhost:2019"), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("CADDY_DOMAIN"));
    }

    #[test]
    fn routes_url_targets_server_routes_collection() {
        let url = routes_url("http://localhost:2019");
        assert_eq!(
            url,
            "http://localhost:2019/config/apps/http/servers/srv0/routes"
        );
    }

    #[test]
    fn upsert_route_list_prepends_and_replaces_id() {
        let existing_keep = serde_json::json!({"@id":"other", "match":[]});
        let existing_replace = serde_json::json!({"@id":"psht-app", "match":[{"host":["old"]}]});
        let new_route = serde_json::json!({"@id":"psht-app", "match":[{"host":["new"]}]});

        let routes = upsert_route_list(
            vec![existing_keep.clone(), existing_replace],
            new_route.clone(),
            "psht-app",
        );

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0], new_route);
        assert_eq!(routes[1], existing_keep);
    }

    #[test]
    fn remove_config_uses_api_without_domain() {
        let result = remove_config_from_values(Some("http://localhost:2019"));
        assert_eq!(result.as_deref(), Some("http://localhost:2019"));
    }

    #[test]
    fn remove_config_none_when_api_missing() {
        let result = remove_config_from_values(None);
        assert!(result.is_none());
    }
}
