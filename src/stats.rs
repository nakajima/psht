use std::env;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

const ENV_INFLUX_URL: &str = "PSHT_STATS_INFLUX_URL";
const ENV_INFLUX_ORG: &str = "PSHT_STATS_INFLUX_ORG";
const ENV_INFLUX_BUCKET: &str = "PSHT_STATS_INFLUX_BUCKET";
const ENV_INFLUX_TOKEN: &str = "PSHT_STATS_INFLUX_TOKEN";
const ENV_MEASUREMENT: &str = "PSHT_STATS_INFLUX_MEASUREMENT";
const ENV_TIMEOUT_SECS: &str = "PSHT_STATS_INFLUX_TIMEOUT_SECS";
const ENV_DEBUG: &str = "PSHT_STATS_INFLUX_DEBUG";

#[derive(Clone)]
struct InfluxConfig {
    write_url: String,
    token: String,
    measurement: String,
    timeout_secs: String,
    host_tag: Option<String>,
    debug: bool,
}

enum ReporterConfig {
    Disabled,
    Enabled(InfluxConfig),
}

pub struct DeployAttempt<'a> {
    pub app: &'a str,
    pub kind: &'a str,
    pub generation: i64,
    pub attempt: u64,
    pub force: bool,
    pub success: bool,
    pub duration: Duration,
    pub error: Option<&'a str>,
}

pub struct HealthSummary {
    pub app_count: usize,
    pub unhealthy_count: usize,
    pub duration: Duration,
}

fn read_non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_env_bool(name: &str) -> bool {
    let Some(value) = read_non_empty_env(name) else {
        return false;
    };
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on"
    )
}

fn url_query_escape(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn escape_tag_component(value: &str, escape_equals: bool) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' | ' ' => {
                out.push('\\');
                out.push(ch);
            }
            '=' if escape_equals => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn escape_measurement(value: &str) -> String {
    escape_tag_component(value, false)
}

fn escape_field_string(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

fn saturating_millis(duration: Duration) -> i64 {
    let millis = duration.as_millis();
    let max = i64::MAX as u128;
    if millis > max {
        i64::MAX
    } else {
        millis as i64
    }
}

fn reporter_config() -> &'static ReporterConfig {
    static CONFIG: OnceLock<ReporterConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let Some(base_url) = read_non_empty_env(ENV_INFLUX_URL) else {
            return ReporterConfig::Disabled;
        };
        let Some(org) = read_non_empty_env(ENV_INFLUX_ORG) else {
            return ReporterConfig::Disabled;
        };
        let Some(bucket) = read_non_empty_env(ENV_INFLUX_BUCKET) else {
            return ReporterConfig::Disabled;
        };
        let Some(token) = read_non_empty_env(ENV_INFLUX_TOKEN) else {
            return ReporterConfig::Disabled;
        };
        let measurement =
            read_non_empty_env(ENV_MEASUREMENT).unwrap_or_else(|| "psht_stats".to_string());
        let timeout_secs = read_non_empty_env(ENV_TIMEOUT_SECS).unwrap_or_else(|| "2".to_string());
        let write_url = format!(
            "{}/api/v2/write?org={}&bucket={}&precision=ns",
            base_url.trim_end_matches('/'),
            url_query_escape(&org),
            url_query_escape(&bucket)
        );
        let host_tag = read_non_empty_env("HOSTNAME");
        ReporterConfig::Enabled(InfluxConfig {
            write_url,
            token,
            measurement,
            timeout_secs,
            host_tag,
            debug: parse_env_bool(ENV_DEBUG),
        })
    })
}

fn influx_write(config: &InfluxConfig, line: &str) -> Result<(), String> {
    let output = Command::new("curl")
        .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST"])
        .args(["--max-time", &config.timeout_secs])
        .args(["-H", &format!("Authorization: Token {}", config.token)])
        .args(["-H", "Content-Type: text/plain; charset=utf-8"])
        .args(["--data-binary", line])
        .arg(&config.write_url)
        .output()
        .map_err(|e| format!("failed to run curl for influx write: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "influx curl write failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !code.starts_with('2') {
        return Err(format!("influx write returned HTTP {code}"));
    }
    Ok(())
}

fn build_line(
    measurement: &str,
    tags: &[(&str, String)],
    fields: &[(&str, String)],
    timestamp_ns: i64,
) -> String {
    let mut line = escape_measurement(measurement);
    for (key, value) in tags {
        line.push(',');
        line.push_str(&escape_tag_component(key, true));
        line.push('=');
        line.push_str(&escape_tag_component(value, true));
    }
    line.push(' ');
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        line.push_str(&escape_tag_component(key, true));
        line.push('=');
        line.push_str(value);
    }
    line.push(' ');
    line.push_str(&timestamp_ns.to_string());
    line
}

fn now_unix_ns() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let ns = now.as_nanos();
    let max = i64::MAX as u128;
    if ns > max { i64::MAX } else { ns as i64 }
}

fn send_line(line: &str) {
    let ReporterConfig::Enabled(config) = reporter_config() else {
        return;
    };
    if let Err(err) = influx_write(config, line)
        && config.debug
    {
        std::eprintln!("Warning: failed to report stats to influxdb: {err}");
    }
}

pub fn report_deploy_attempt(event: DeployAttempt<'_>) {
    let ReporterConfig::Enabled(config) = reporter_config() else {
        return;
    };
    let mut tags = vec![
        ("app", event.app.to_string()),
        ("kind", event.kind.to_string()),
        (
            "outcome",
            if event.success {
                "success".to_string()
            } else {
                "failed".to_string()
            },
        ),
    ];
    if let Some(host) = config.host_tag.as_ref() {
        tags.push(("host", host.clone()));
    }
    let mut fields = vec![
        ("generation", format!("{}i", event.generation)),
        ("attempt", format!("{}i", event.attempt)),
        ("force", event.force.to_string()),
        (
            "duration_ms",
            format!("{}i", saturating_millis(event.duration)),
        ),
    ];
    if let Some(err) = event.error {
        fields.push(("error", format!("\"{}\"", escape_field_string(err))));
    }
    let line = build_line(&config.measurement, &tags, &fields, now_unix_ns());
    send_line(&line);
}

pub fn report_health_summary(event: HealthSummary) {
    let ReporterConfig::Enabled(config) = reporter_config() else {
        return;
    };
    let mut tags = vec![(
        "outcome",
        if event.unhealthy_count == 0 {
            "healthy".to_string()
        } else {
            "degraded".to_string()
        },
    )];
    if let Some(host) = config.host_tag.as_ref() {
        tags.push(("host", host.clone()));
    }
    let fields = vec![
        ("app_count", format!("{}i", event.app_count)),
        ("unhealthy_count", format!("{}i", event.unhealthy_count)),
        (
            "duration_ms",
            format!("{}i", saturating_millis(event.duration)),
        ),
    ];
    let line = build_line(
        &format!("{}_health", config.measurement),
        &tags,
        &fields,
        now_unix_ns(),
    );
    send_line(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_query_escape_percent_encodes_reserved_chars() {
        assert_eq!(
            url_query_escape("org one/two@acme"),
            "org%20one%2Ftwo%40acme"
        );
    }

    #[test]
    fn build_line_escapes_tags_and_strings() {
        let line = build_line(
            "psht stats",
            &[
                ("app", "hello world".to_string()),
                ("kind", "push=deploy".to_string()),
            ],
            &[
                ("duration_ms", "123i".to_string()),
                (
                    "error",
                    format!("\"{}\"", escape_field_string("boom \"line\"\nnext")),
                ),
            ],
            42,
        );
        assert_eq!(
            line,
            "psht\\ stats,app=hello\\ world,kind=push\\=deploy duration_ms=123i,error=\"boom \\\"line\\\" next\" 42"
        );
    }

    #[test]
    fn saturating_millis_caps_large_values() {
        assert_eq!(saturating_millis(Duration::from_millis(10)), 10);
        assert_eq!(
            saturating_millis(Duration::from_millis(i64::MAX as u64 + 1)),
            i64::MAX
        );
    }
}
