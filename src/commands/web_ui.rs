use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::app_name;
use crate::container;
use crate::control_plane;
use crate::deploy_log;
use crate::detect;
use crate::sqlite_store;

use super::observability_commands::{self, AppHealthReport, PsRow};

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

pub fn serve(bind: &str, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind((bind, port))
        .map_err(|e| format!("failed to bind web UI on {bind}:{port}: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("failed to determine web UI address: {e}"))?;
    println!("psht web UI listening on http://{addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream) {
                        eprintln!("       Warning: web UI request failed: {err}");
                    }
                });
            }
            Err(err) => {
                eprintln!("       Warning: failed to accept web UI connection: {err}");
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
}

#[derive(Debug)]
struct HttpResponse {
    status_code: u16,
    reason: &'static str,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VersionStatus {
    running_version: String,
    installed_version: String,
    latest_version: Option<String>,
    upgrade_available: bool,
    restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DashboardApp {
    app: String,
    status: String,
    health: String,
    phase: String,
    desired_state: String,
    source_kind: Option<String>,
    stack: Option<String>,
    active_instance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppDetail {
    app: String,
    status: String,
    desired_state: String,
    phase: String,
    active_instance: Option<String>,
    candidate_instance: Option<String>,
    previous_instance: Option<String>,
    active_revision: Option<String>,
    candidate_revision: Option<String>,
    runtime_project: Option<String>,
    generation: Option<i64>,
    source_kind: Option<String>,
    stack: Option<String>,
    health: AppHealthReport,
    deploy_history: Vec<sqlite_store::DeployHistoryRow>,
    logs: String,
    tailscale: String,
    actions: AppActions,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AppMetadata {
    generation: Option<i64>,
    source_kind: Option<String>,
    stack: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppActions {
    start: ActionState,
    stop: ActionState,
    restart: ActionState,
    destroy: ActionState,
    tailscale_up: ActionState,
    tailscale_down: ActionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionState {
    enabled: bool,
    reason: Option<String>,
}

impl ActionState {
    fn enabled() -> Self {
        Self {
            enabled: true,
            reason: None,
        }
    }

    fn disabled(reason: &str) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppStatusKind {
    Running,
    Down,
    Stopped,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailscaleStatusKind {
    Healthy,
    Unhealthy,
    Unavailable,
}

fn handle_connection(mut stream: TcpStream) -> Result<(), String> {
    let response = match read_request(&stream) {
        Ok(request) => route_request(request),
        Err(err) => error_response(400, "Bad Request", "Request parse failed", &err),
    };
    write_response(&mut stream, response)
}

fn read_request(stream: &TcpStream) -> Result<HttpRequest, String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("failed to clone request stream: {e}"))?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| format!("failed to read request line: {e}"))?;
    if request_line.trim().is_empty() {
        return Err("empty request line".to_string());
    }

    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing request method".to_string())?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| "missing request target".to_string())?;
    if parts.next().is_none() {
        return Err("missing HTTP version".to_string());
    }

    loop {
        let mut header_line = String::new();
        reader
            .read_line(&mut header_line)
            .map_err(|e| format!("failed to read request headers: {e}"))?;
        if header_line.is_empty() || header_line == "\r\n" || header_line == "\n" {
            break;
        }
    }

    let (path, query) = parse_target(target)?;
    Ok(HttpRequest {
        method,
        path,
        query,
    })
}

fn parse_target(target: &str) -> Result<(String, HashMap<String, String>), String> {
    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let path = percent_decode(raw_path, false)?;
    let query = raw_query
        .map(parse_query_string)
        .transpose()?
        .unwrap_or_default();
    Ok((path, query))
}

fn parse_query_string(raw: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key, true)?, percent_decode(value, true)?);
    }
    Ok(out)
}

fn percent_decode(raw: &str, plus_as_space: bool) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' if plus_as_space => {
                out.push(' ');
                idx += 1;
            }
            b'%' => {
                if idx + 2 >= bytes.len() {
                    return Err(format!("invalid percent-encoding in '{raw}'"));
                }
                let hi = decode_hex_digit(bytes[idx + 1])
                    .ok_or_else(|| format!("invalid percent-encoding in '{raw}'"))?;
                let lo = decode_hex_digit(bytes[idx + 2])
                    .ok_or_else(|| format!("invalid percent-encoding in '{raw}'"))?;
                out.push(char::from((hi << 4) | lo));
                idx += 3;
            }
            byte => {
                out.push(char::from(byte));
                idx += 1;
            }
        }
    }
    Ok(out)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(10 + byte - b'a'),
        b'A'..=b'F' => Some(10 + byte - b'A'),
        _ => None,
    }
}

fn route_request(request: HttpRequest) -> HttpResponse {
    let segments = path_segments(&request.path);
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", []) => dashboard_response(&request),
        ("GET", ["host"]) => host_response(&request),
        ("GET", ["apps", app]) => app_response(&request, app),
        ("POST", ["apps", app, "start"]) => {
            app_action_response(&request, app, "start", super::start)
        }
        ("POST", ["apps", app, "stop"]) => app_action_response(&request, app, "stop", super::stop),
        ("POST", ["apps", app, "restart"]) => {
            app_action_response(&request, app, "restart", super::restart)
        }
        ("POST", ["apps", app, "destroy"]) => {
            app_action_response(&request, app, "destroy", super::destroy)
        }
        ("POST", ["apps", app, "tailscale", "up"]) => {
            app_action_response(&request, app, "tailscale up", super::tailscale_up)
        }
        ("POST", ["apps", app, "tailscale", "down"]) => {
            app_action_response(&request, app, "tailscale down", super::tailscale_down)
        }
        ("POST", ["host", "upgrade"]) => host_upgrade_response(&request),
        ("GET", ["favicon.ico"]) => no_content_response(),
        _ => error_response(404, "Not Found", "Route not found", &request.path),
    }
}

fn dashboard_response(request: &HttpRequest) -> HttpResponse {
    let version = match version_status() {
        Ok(status) => status,
        Err(err) => {
            return error_response(500, "Internal Server Error", "Version lookup failed", &err);
        }
    };
    let apps = match load_dashboard_apps() {
        Ok(apps) => apps,
        Err(err) => {
            return error_response(500, "Internal Server Error", "Dashboard load failed", &err);
        }
    };

    let mut body = String::new();
    body.push_str("<h1>psht</h1>");
    body.push_str(&nav_html());
    body.push_str(&message_banner_html(request));
    body.push_str(&version_section_html(&version, false));
    body.push_str("<section><h2>Apps</h2>");
    if apps.is_empty() {
        body.push_str("<p>No deployed apps found.</p>");
    } else {
        body.push_str("<table><thead><tr><th>App</th><th>Status</th><th>Health</th><th>Phase</th><th>Desired</th><th>Source</th><th>Stack</th><th>Active</th></tr></thead><tbody>");
        for app in apps {
            let app_path = app_path(&app.app);
            body.push_str("<tr>");
            body.push_str(&format!(
                "<td><a href=\"{app_path}\">{}</a></td>",
                html_escape(&app.app)
            ));
            body.push_str(&format!("<td>{}</td>", html_escape(&app.status)));
            body.push_str(&format!("<td>{}</td>", html_escape(&app.health)));
            body.push_str(&format!("<td>{}</td>", html_escape(&app.phase)));
            body.push_str(&format!("<td>{}</td>", html_escape(&app.desired_state)));
            body.push_str(&format!(
                "<td>{}</td>",
                html_escape(app.source_kind.as_deref().unwrap_or("-"))
            ));
            body.push_str(&format!(
                "<td>{}</td>",
                html_escape(app.stack.as_deref().unwrap_or("-"))
            ));
            body.push_str(&format!(
                "<td>{}</td>",
                html_escape(app.active_instance.as_deref().unwrap_or("-"))
            ));
            body.push_str("</tr>");
        }
        body.push_str("</tbody></table>");
    }
    body.push_str("</section>");

    html_response(page_html("psht", &body))
}

fn host_response(request: &HttpRequest) -> HttpResponse {
    let version = match version_status() {
        Ok(status) => status,
        Err(err) => {
            return error_response(500, "Internal Server Error", "Version lookup failed", &err);
        }
    };
    let doctor = capture_self_command(&["doctor"]);
    let health = capture_self_command(&["health"]);

    let mut body = String::new();
    body.push_str("<h1>Host</h1>");
    body.push_str(&nav_html());
    body.push_str(&message_banner_html(request));
    body.push_str(&version_section_html(&version, true));
    body.push_str(&pre_section_html("Doctor", &command_output_text(&doctor)));
    body.push_str(&pre_section_html("Health", &command_output_text(&health)));

    html_response(page_html("psht host", &body))
}

fn app_response(request: &HttpRequest, app: &str) -> HttpResponse {
    if let Err(err) = app_name::validate_app_name(app) {
        return error_response(400, "Bad Request", "Invalid app name", &err);
    }

    let detail = match load_app_detail(app) {
        Ok(Some(detail)) => detail,
        Ok(None) => return error_response(404, "Not Found", "App not found", app),
        Err(err) => return error_response(500, "Internal Server Error", "App load failed", &err),
    };

    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>", html_escape(&detail.app)));
    body.push_str(&nav_html());
    body.push_str(&message_banner_html(request));
    body.push_str("<section><h2>Overview</h2><table><tbody>");
    body.push_str(&table_row_html("Status", &detail.status));
    body.push_str(&table_row_html("Desired", &detail.desired_state));
    body.push_str(&table_row_html("Phase", &detail.phase));
    body.push_str(&table_row_html(
        "Generation",
        &detail
            .generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
    ));
    body.push_str(&table_row_html(
        "Source",
        detail.source_kind.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Stack",
        detail.stack.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Active instance",
        detail.active_instance.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Candidate instance",
        detail.candidate_instance.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Previous instance",
        detail.previous_instance.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Active revision",
        detail.active_revision.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Candidate revision",
        detail.candidate_revision.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Runtime project",
        detail.runtime_project.as_deref().unwrap_or("-"),
    ));
    body.push_str(&table_row_html(
        "Port",
        &super::allocate_port(&detail.app).to_string(),
    ));
    body.push_str("</tbody></table></section>");

    let next = app_path(&detail.app);
    body.push_str("<section><h2>Actions</h2>");
    body.push_str(&action_button_html(
        &format!("{next}/start"),
        &next,
        "Start",
        &detail.actions.start,
    ));
    body.push_str(&action_button_html(
        &format!("{next}/stop"),
        &next,
        "Stop",
        &detail.actions.stop,
    ));
    body.push_str(&action_button_html(
        &format!("{next}/restart"),
        &next,
        "Restart",
        &detail.actions.restart,
    ));
    body.push_str(&action_button_html(
        &format!("{next}/destroy"),
        &next,
        "Destroy",
        &detail.actions.destroy,
    ));
    body.push_str(&action_button_html(
        &format!("{next}/tailscale/up"),
        &next,
        "Tailscale Up",
        &detail.actions.tailscale_up,
    ));
    body.push_str(&action_button_html(
        &format!("{next}/tailscale/down"),
        &next,
        "Tailscale Down",
        &detail.actions.tailscale_down,
    ));
    body.push_str("</section>");

    body.push_str("<section><h2>Health</h2>");
    body.push_str(&format!(
        "<p>{}</p>",
        html_escape(if detail.health.healthy {
            "ok"
        } else {
            "unhealthy"
        })
    ));
    body.push_str("<ul>");
    for line in &detail.health.details {
        body.push_str(&format!("<li>{}</li>", html_escape(line)));
    }
    body.push_str("</ul></section>");

    body.push_str("<section><h2>Deploy History</h2>");
    if detail.deploy_history.is_empty() {
        body.push_str("<p>No deploy history recorded.</p>");
    } else {
        body.push_str("<table><thead><tr><th>When</th><th>Outcome</th><th>Revision</th><th>Summary</th></tr></thead><tbody>");
        for row in &detail.deploy_history {
            body.push_str("<tr>");
            body.push_str(&format!(
                "<td>{}</td>",
                html_escape(&format_age_ms(row.created_at_ms))
            ));
            body.push_str(&format!("<td>{}</td>", html_escape(&row.outcome)));
            body.push_str(&format!(
                "<td>{}</td>",
                html_escape(row.revision.as_deref().unwrap_or("-"))
            ));
            body.push_str(&format!("<td>{}</td>", html_escape(&row.summary)));
            body.push_str("</tr>");
        }
        body.push_str("</tbody></table>");
    }
    body.push_str("</section>");

    body.push_str(&pre_section_html("Tailscale", &detail.tailscale));
    body.push_str(&pre_section_html("Recent Logs", &detail.logs));

    html_response(page_html(&format!("psht {}", detail.app), &body))
}

fn app_action_response<F>(
    request: &HttpRequest,
    app: &str,
    action_name: &str,
    action: F,
) -> HttpResponse
where
    F: FnOnce(&str) -> Result<(), String>,
{
    if let Err(err) = app_name::validate_app_name(app) {
        return error_response(400, "Bad Request", "Invalid app name", &err);
    }
    let default_target = app_path(app);
    let target = redirect_target(request, &default_target);
    match action(app) {
        Ok(()) => redirect_response(&target, Some(("ok", &format!("{}: {}", action_name, app)))),
        Err(err) => redirect_response(&target, Some(("error", &err))),
    }
}

fn host_upgrade_response(request: &HttpRequest) -> HttpResponse {
    let target = redirect_target(request, "/host");
    match super::upgrade_server() {
        Ok(()) => redirect_response(
            &target,
            Some((
                "ok",
                "upgrade complete; restart the web UI if the running version changed",
            )),
        ),
        Err(err) => redirect_response(&target, Some(("error", &err))),
    }
}

fn no_content_response() -> HttpResponse {
    HttpResponse {
        status_code: 204,
        reason: "No Content",
        content_type: "text/plain; charset=utf-8",
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn html_response(body: String) -> HttpResponse {
    HttpResponse {
        status_code: 200,
        reason: "OK",
        content_type: HTML_CONTENT_TYPE,
        headers: Vec::new(),
        body: body.into_bytes(),
    }
}

fn redirect_response(target: &str, message: Option<(&str, &str)>) -> HttpResponse {
    let mut location = sanitize_redirect_target(target, "/");
    if let Some((key, value)) = message {
        let separator = if location.contains('?') { "&" } else { "?" };
        location.push_str(separator);
        location.push_str(&url_encode(key));
        location.push('=');
        location.push_str(&url_encode(value));
    }
    HttpResponse {
        status_code: 303,
        reason: "See Other",
        content_type: "text/plain; charset=utf-8",
        headers: vec![("Location".to_string(), location)],
        body: b"See Other".to_vec(),
    }
}

fn error_response(
    status_code: u16,
    reason: &'static str,
    title: &str,
    detail: &str,
) -> HttpResponse {
    html_response_with_status(
        status_code,
        reason,
        page_html(
            title,
            &format!(
                "<h1>{}</h1>{}<pre>{}</pre>",
                html_escape(title),
                nav_html(),
                html_escape(detail)
            ),
        ),
    )
}

fn html_response_with_status(status_code: u16, reason: &'static str, body: String) -> HttpResponse {
    HttpResponse {
        status_code,
        reason,
        content_type: HTML_CONTENT_TYPE,
        headers: Vec::new(),
        body: body.into_bytes(),
    }
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> Result<(), String> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status_code,
        response.reason,
        response.content_type,
        response.body.len()
    );
    for (key, value) in response.headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("failed to write response head: {e}"))?;
    stream
        .write_all(&response.body)
        .map_err(|e| format!("failed to write response body: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("failed to flush response: {e}"))
}

fn path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn nav_html() -> String {
    "<nav><a href=\"/\">Dashboard</a> | <a href=\"/host\">Host</a></nav>".to_string()
}

fn page_html(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title><style>body{{font-family:sans-serif;max-width:1100px;margin:2rem auto;padding:0 1rem;line-height:1.4;}}nav{{margin:0 0 1rem 0;}}table{{border-collapse:collapse;width:100%;margin:0.5rem 0 1.5rem 0;}}th,td{{border:1px solid #d0d0d0;padding:0.5rem;text-align:left;vertical-align:top;}}pre{{background:#f5f5f5;border:1px solid #d0d0d0;padding:0.75rem;overflow-x:auto;white-space:pre-wrap;}}form{{display:inline-block;margin:0;}}button{{padding:0.35rem 0.6rem;}}button:disabled{{opacity:0.6;cursor:not-allowed;}}section{{margin:1.5rem 0;}}.banner{{padding:0.75rem;border:1px solid #d0d0d0;margin:1rem 0;}}.banner.ok{{background:#eef8ee;}}.banner.error{{background:#fff0f0;}}.banner.warn{{background:#fff8e8;}}.action{{display:inline-flex;align-items:center;gap:0.35rem;margin:0 0.5rem 0.5rem 0;}}.action-note{{color:#555;font-size:0.9rem;}}</style></head><body>{}</body></html>",
        html_escape(title),
        body
    )
}

fn message_banner_html(request: &HttpRequest) -> String {
    if let Some(ok) = request.query.get("ok") {
        return format!("<div class=\"banner ok\">{}</div>", html_escape(ok));
    }
    if let Some(error) = request.query.get("error") {
        return format!("<div class=\"banner error\">{}</div>", html_escape(error));
    }
    String::new()
}

fn version_section_html(version: &VersionStatus, include_upgrade_action: bool) -> String {
    let mut section = String::new();
    section.push_str("<section><h2>Version</h2><table><tbody>");
    section.push_str(&table_row_html("Running", &version.running_version));
    section.push_str(&table_row_html("Installed", &version.installed_version));
    section.push_str(&table_row_html(
        "Latest",
        version.latest_version.as_deref().unwrap_or("unavailable"),
    ));
    section.push_str(&table_row_html(
        "Out of date",
        if version.upgrade_available {
            "yes"
        } else {
            "no"
        },
    ));
    section.push_str("</tbody></table>");
    if version.upgrade_available {
        section
            .push_str("<div class=\"banner warn\">A newer psht-server release is available.</div>");
        if include_upgrade_action {
            section.push_str(&enabled_action_form_html(
                "/host/upgrade",
                "/host",
                "Upgrade Server",
            ));
        }
    }
    if version.restart_required {
        section.push_str(&format!(
            "<div class=\"banner warn\">Running binary is {} but the installed binary is {}. Restart the web UI to load the updated binary.</div>",
            html_escape(&version.running_version),
            html_escape(&version.installed_version),
        ));
    }
    section.push_str("</section>");
    section
}

fn table_row_html(label: &str, value: &str) -> String {
    format!(
        "<tr><th>{}</th><td>{}</td></tr>",
        html_escape(label),
        html_escape(value)
    )
}

fn pre_section_html(title: &str, text: &str) -> String {
    format!(
        "<section><h2>{}</h2><pre>{}</pre></section>",
        html_escape(title),
        html_escape(text)
    )
}

fn enabled_action_form_html(action_path: &str, next: &str, label: &str) -> String {
    let next = sanitize_redirect_target(next, "/");
    let action = format!("{action_path}?next={}", url_encode(&next));
    format!(
        "<span class=\"action\"><form method=\"post\" action=\"{}\"><button type=\"submit\">{}</button></form></span>",
        html_escape(&action),
        html_escape(label)
    )
}

fn action_button_html(action_path: &str, next: &str, label: &str, state: &ActionState) -> String {
    if state.enabled {
        return enabled_action_form_html(action_path, next, label);
    }

    let reason = state.reason.as_deref().unwrap_or("unavailable");
    format!(
        "<span class=\"action\"><button type=\"button\" disabled title=\"{}\">{}</button><span class=\"action-note\">{}</span></span>",
        html_escape(reason),
        html_escape(label),
        html_escape(reason),
    )
}

fn redirect_target(request: &HttpRequest, default: &str) -> String {
    request
        .query
        .get("next")
        .map(|value| sanitize_redirect_target(value, default))
        .unwrap_or_else(|| default.to_string())
}

fn sanitize_redirect_target(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        trimmed.to_string()
    } else {
        fallback.to_string()
    }
}

fn app_path(app: &str) -> String {
    format!("/apps/{}", url_encode(app))
}

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        let is_unreserved =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/');
        if is_unreserved {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn version_status() -> Result<VersionStatus, String> {
    let running = env!("CARGO_PKG_VERSION").to_string();
    let installed = super::current_psht_binary()
        .ok()
        .and_then(|path| super::binary_version(&path))
        .unwrap_or_else(|| running.clone());
    let latest = super::latest_release_version_for_warning();
    Ok(build_version_status(
        &running,
        &installed,
        latest.as_deref(),
    ))
}

fn build_version_status(running: &str, installed: &str, latest: Option<&str>) -> VersionStatus {
    let latest_version = latest.map(|value| value.to_string());
    let upgrade_available = latest
        .map(|value| super::version_is_newer(value, installed))
        .unwrap_or(false);
    VersionStatus {
        running_version: running.to_string(),
        installed_version: installed.to_string(),
        latest_version,
        upgrade_available,
        restart_required: running != installed,
    }
}

fn load_dashboard_apps() -> Result<Vec<DashboardApp>, String> {
    let rows = observability_commands::ps_rows()?;
    let mut apps = Vec::with_capacity(rows.len());
    for row in rows {
        let metadata = app_metadata(&row.app)?;
        let status = sqlite_store::get_app_status(&row.app)?;
        let runtime = sqlite_store::get_app_runtime_state(&row.app)?;
        let health = app_health_for_row(&row);
        let desired = control_plane::desired_state(&row.app)
            .unwrap_or(control_plane::DesiredState::Running)
            .as_str()
            .to_string();
        apps.push(DashboardApp {
            app: row.app,
            status: row.status,
            health: if health.healthy {
                "ok".to_string()
            } else {
                "unhealthy".to_string()
            },
            source_kind: metadata.source_kind,
            stack: metadata.stack,
            phase: status
                .as_ref()
                .map(|value| value.phase.clone())
                .unwrap_or_else(|| "-".to_string()),
            desired_state: desired,
            active_instance: status
                .and_then(|value| value.active_instance)
                .or_else(|| runtime.map(|value| value.active_instance)),
        });
    }
    Ok(apps)
}

fn load_app_detail(app: &str) -> Result<Option<AppDetail>, String> {
    let ps_row = observability_commands::ps_rows()?
        .into_iter()
        .find(|row| row.app == app);
    let spec_row = sqlite_store::get_app_spec(app)?;
    let status_row = sqlite_store::get_app_status(app)?;
    let runtime_state = sqlite_store::get_app_runtime_state(app)?;
    let deploy_history = sqlite_store::list_deploy_history(app, 10)?;
    let logs =
        read_recent_logs(app).unwrap_or_else(|err| format!("recent logs unavailable: {err}"));
    let tailscale = read_tailscale_summary(app)
        .unwrap_or_else(|err| format!("tailscale status unavailable: {err}"));

    if ps_row.is_none()
        && spec_row.is_none()
        && status_row.is_none()
        && runtime_state.is_none()
        && deploy_history.is_empty()
        && logs.trim() == "No recent logs."
    {
        return Ok(None);
    }

    let (status, health) = match ps_row.as_ref() {
        Some(row) => (row.status.clone(), app_health_for_row(row)),
        None => (
            "Missing".to_string(),
            AppHealthReport {
                app: app.to_string(),
                healthy: false,
                details: vec!["active container missing".to_string()],
            },
        ),
    };
    let metadata = app_metadata_from_spec(app, spec_row.as_ref());
    let desired_state = spec_row
        .as_ref()
        .map(|row| row.desired_state.clone())
        .unwrap_or_else(|| control_plane::DesiredState::Running.as_str().to_string());
    let active_instance = status_row
        .as_ref()
        .and_then(|value| value.active_instance.clone())
        .or_else(|| {
            runtime_state
                .as_ref()
                .map(|value| value.active_instance.clone())
        });
    let actions = app_actions(
        app_status_kind(&status),
        active_instance.is_some(),
        tailscale_status_kind(&tailscale),
    );

    Ok(Some(AppDetail {
        app: app.to_string(),
        status,
        desired_state,
        phase: status_row
            .as_ref()
            .map(|value| value.phase.clone())
            .unwrap_or_else(|| "-".to_string()),
        active_instance,
        candidate_instance: status_row
            .as_ref()
            .and_then(|value| value.candidate_instance.clone()),
        previous_instance: status_row
            .as_ref()
            .and_then(|value| value.previous_instance.clone())
            .or_else(|| {
                runtime_state
                    .as_ref()
                    .and_then(|value| value.previous_instance.clone())
            }),
        active_revision: status_row
            .as_ref()
            .and_then(|value| value.active_revision.clone()),
        candidate_revision: status_row
            .as_ref()
            .and_then(|value| value.candidate_revision.clone()),
        runtime_project: runtime_state.and_then(|value| value.runtime_project),
        generation: metadata.generation,
        source_kind: metadata.source_kind,
        stack: metadata.stack,
        health,
        deploy_history,
        logs,
        tailscale,
        actions,
    }))
}

fn app_metadata(app: &str) -> Result<AppMetadata, String> {
    let spec_row = sqlite_store::get_app_spec(app)?;
    Ok(app_metadata_from_spec(app, spec_row.as_ref()))
}

fn app_metadata_from_spec(app: &str, spec_row: Option<&sqlite_store::AppSpecRow>) -> AppMetadata {
    AppMetadata {
        generation: spec_row.map(|row| row.generation),
        source_kind: spec_row.map(|row| source_kind_label(&row.source_kind).to_string()),
        stack: detect_app_stack(app, spec_row.map(|row| row.source_kind.as_str())),
    }
}

fn source_kind_label(kind: &str) -> &str {
    match kind {
        "tar-stdin" => "push",
        _ => kind,
    }
}

fn detect_app_stack(app: &str, source_kind: Option<&str>) -> Option<String> {
    for dir in app_source_dirs(app, source_kind) {
        if !dir.is_dir() {
            continue;
        }
        if let Some(stack) = detect_stack_in_dir(&dir) {
            return Some(stack);
        }
    }
    None
}

fn app_source_dirs(app: &str, source_kind: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let push_dir = super::home_dir().join(app);
    let git_dir = super::builds_dir().join(app);

    match source_kind {
        Some("tar-stdin") => {
            dirs.push(push_dir);
            dirs.push(git_dir);
        }
        Some("git") => {
            dirs.push(git_dir);
            dirs.push(push_dir);
        }
        _ => {
            dirs.push(push_dir);
            dirs.push(git_dir);
        }
    }

    dirs
}

fn detect_stack_in_dir(dir: &Path) -> Option<String> {
    let uses_custom_stack = dir.join("psht-stack.sh").exists();
    match detect::detect(dir) {
        Ok(config) if uses_custom_stack => Some(format!("custom ({})", config.stack())),
        Ok(config) => Some(config.stack().to_string()),
        Err(_) if uses_custom_stack => Some("custom".to_string()),
        Err(_) => None,
    }
}

fn app_status_kind(status: &str) -> AppStatusKind {
    match status.trim().to_ascii_lowercase().as_str() {
        "running" => AppStatusKind::Running,
        "down" => AppStatusKind::Down,
        "stopped" => AppStatusKind::Stopped,
        _ => AppStatusKind::Missing,
    }
}

fn tailscale_status_kind(summary: &str) -> TailscaleStatusKind {
    if summary.starts_with("tailscale status unavailable:") {
        return TailscaleStatusKind::Unavailable;
    }
    if summary.contains("State: Running")
        && summary.contains("Online: yes")
        && !summary.contains("\nHealth:")
        && !summary.contains("\nRepair:")
    {
        return TailscaleStatusKind::Healthy;
    }
    TailscaleStatusKind::Unhealthy
}

fn app_actions(
    status: AppStatusKind,
    has_active_instance: bool,
    tailscale: TailscaleStatusKind,
) -> AppActions {
    let missing = ActionState::disabled("app missing");
    let stopped = ActionState::disabled("already stopped");

    let start = match status {
        AppStatusKind::Running => ActionState::disabled("already running"),
        AppStatusKind::Missing => missing.clone(),
        AppStatusKind::Down | AppStatusKind::Stopped => ActionState::enabled(),
    };
    let stop = match status {
        AppStatusKind::Running | AppStatusKind::Down => ActionState::enabled(),
        AppStatusKind::Stopped => stopped.clone(),
        AppStatusKind::Missing => missing.clone(),
    };
    let restart = match status {
        AppStatusKind::Running | AppStatusKind::Down => ActionState::enabled(),
        AppStatusKind::Stopped => stopped,
        AppStatusKind::Missing => missing.clone(),
    };
    let tailscale_up = if matches!(status, AppStatusKind::Missing) {
        ActionState::disabled("app missing")
    } else if !has_active_instance {
        ActionState::disabled("start the app first")
    } else if matches!(tailscale, TailscaleStatusKind::Healthy) {
        ActionState::disabled("tailscale already healthy")
    } else {
        ActionState::enabled()
    };
    let tailscale_down = if matches!(status, AppStatusKind::Missing) {
        ActionState::disabled("app missing")
    } else if !has_active_instance {
        ActionState::disabled("start the app first")
    } else if matches!(tailscale, TailscaleStatusKind::Healthy) {
        ActionState::enabled()
    } else {
        ActionState::disabled("use tailscale up")
    };

    AppActions {
        start,
        stop,
        restart,
        destroy: ActionState::enabled(),
        tailscale_up,
        tailscale_down,
    }
}

fn app_health_for_row(row: &PsRow) -> AppHealthReport {
    if let Some(active_app) = row.active_app.as_deref() {
        return observability_commands::check_app_health(
            &row.app,
            active_app,
            &row.container_status,
        );
    }
    AppHealthReport {
        app: row.app.clone(),
        healthy: false,
        details: vec!["active container missing".to_string()],
    }
}

fn read_recent_logs(app: &str) -> Result<String, String> {
    let deploy_lines = deploy_log::recent_entries(
        app,
        super::LOGS_DEPLOY_HISTORY_FILES,
        super::LOGS_DEPLOY_HISTORY_LINES_PER_FILE,
    )?;
    let mut lines = Vec::new();
    for line in deploy_lines {
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }

    if let Some(active_app) = super::resolve_active_app_ref(app)?
        && container::is_running(&active_app)?
    {
        let app_log = container::exec_output(
            &active_app,
            &format!(
                "if [ -f {path} ]; then tail -n {lines} {path}; fi",
                path = super::APP_PROCESS_LOG_PATH,
                lines = super::APP_LOG_TAIL_LINES
            ),
        )
        .map_err(|e| format!("failed to read app log from '{active_app}': {e}"))?;
        for line in app_log.lines() {
            if !line.trim().is_empty() {
                lines.push(format!("app> {line}"));
            }
        }
    }

    if lines.is_empty() {
        return Ok("No recent logs.".to_string());
    }
    Ok(lines.join("\n"))
}

fn read_tailscale_summary(app: &str) -> Result<String, String> {
    let active_app = super::resolve_existing_active_app_ref(app)?;
    if !container::is_running(&active_app)? {
        return Err(format!("app '{app}' is not running"));
    }
    let status = container::exec_output(&active_app, "tailscale status --json")?;
    super::tailscale_self_status_summary_from_json(app, &status)
}

fn capture_self_command(args: &[&str]) -> Result<String, String> {
    let exe = super::current_psht_binary()?;
    let output = Command::new(&exe)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {} {:?}: {e}", exe.display(), args))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
        (true, true) => String::new(),
    };
    if output.status.success() {
        Ok(if combined.is_empty() {
            "(no output)".to_string()
        } else {
            combined
        })
    } else if combined.is_empty() {
        Err(format!("command {:?} failed", args))
    } else {
        Err(combined)
    }
}

fn command_output_text(result: &Result<String, String>) -> String {
    match result {
        Ok(output) => output.clone(),
        Err(err) => err.clone(),
    }
}

fn format_age_ms(created_at_ms: i64) -> String {
    let now_ms = now_unix_ms();
    let age_ms = if created_at_ms >= now_ms {
        0
    } else {
        now_ms - created_at_ms
    };
    let age = Duration::from_millis(age_ms as u64);
    if age.as_secs() < 60 {
        format!("{}s ago", age.as_secs())
    } else if age.as_secs() < 60 * 60 {
        format!("{}m ago", age.as_secs() / 60)
    } else if age.as_secs() < 60 * 60 * 24 {
        format!("{}h ago", age.as_secs() / (60 * 60))
    } else {
        format!("{}d ago", age.as_secs() / (60 * 60 * 24))
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_query_string_decodes_values() {
        let query = parse_query_string("ok=hello+world&next=%2Fapps%2Fdemo").unwrap();
        assert_eq!(query.get("ok").map(String::as_str), Some("hello world"));
        assert_eq!(query.get("next").map(String::as_str), Some("/apps/demo"));
    }

    #[test]
    fn sanitize_redirect_target_rejects_external_values() {
        assert_eq!(sanitize_redirect_target("//evil.example", "/"), "/");
        assert_eq!(sanitize_redirect_target("http://evil.example", "/"), "/");
        assert_eq!(sanitize_redirect_target("/apps/demo", "/"), "/apps/demo");
    }

    #[test]
    fn build_version_status_marks_upgrade_available() {
        let status = build_version_status("0.2.70", "0.2.70", Some("0.2.71"));
        assert!(status.upgrade_available);
        assert!(!status.restart_required);
    }

    #[test]
    fn build_version_status_marks_restart_required() {
        let status = build_version_status("0.2.70", "0.2.71", Some("0.2.71"));
        assert!(!status.upgrade_available);
        assert!(status.restart_required);
    }

    #[test]
    fn path_segments_ignore_empty_pieces() {
        assert_eq!(path_segments("/apps/demo/"), vec!["apps", "demo"]);
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn app_actions_disable_start_when_running() {
        let actions = app_actions(AppStatusKind::Running, true, TailscaleStatusKind::Healthy);

        assert_eq!(actions.start, ActionState::disabled("already running"));
        assert_eq!(actions.stop, ActionState::enabled());
        assert_eq!(actions.restart, ActionState::enabled());
        assert_eq!(
            actions.tailscale_up,
            ActionState::disabled("tailscale already healthy")
        );
        assert_eq!(actions.tailscale_down, ActionState::enabled());
    }

    #[test]
    fn app_actions_require_start_before_tailscale_when_stopped() {
        let actions = app_actions(
            AppStatusKind::Stopped,
            false,
            TailscaleStatusKind::Unavailable,
        );

        assert_eq!(actions.start, ActionState::enabled());
        assert_eq!(actions.stop, ActionState::disabled("already stopped"));
        assert_eq!(actions.restart, ActionState::disabled("already stopped"));
        assert_eq!(
            actions.tailscale_up,
            ActionState::disabled("start the app first")
        );
        assert_eq!(
            actions.tailscale_down,
            ActionState::disabled("start the app first")
        );
    }

    #[test]
    fn app_actions_keep_destroy_enabled_when_missing() {
        let actions = app_actions(
            AppStatusKind::Missing,
            false,
            TailscaleStatusKind::Unavailable,
        );

        assert_eq!(actions.start, ActionState::disabled("app missing"));
        assert_eq!(actions.stop, ActionState::disabled("app missing"));
        assert_eq!(actions.restart, ActionState::disabled("app missing"));
        assert_eq!(actions.destroy, ActionState::enabled());
        assert_eq!(actions.tailscale_up, ActionState::disabled("app missing"));
        assert_eq!(actions.tailscale_down, ActionState::disabled("app missing"));
    }

    #[test]
    fn app_actions_enable_tailscale_up_when_running_but_unhealthy() {
        let actions = app_actions(AppStatusKind::Down, true, TailscaleStatusKind::Unhealthy);

        assert_eq!(actions.start, ActionState::enabled());
        assert_eq!(actions.stop, ActionState::enabled());
        assert_eq!(actions.restart, ActionState::enabled());
        assert_eq!(actions.tailscale_up, ActionState::enabled());
        assert_eq!(
            actions.tailscale_down,
            ActionState::disabled("use tailscale up")
        );
    }

    #[test]
    fn action_button_html_renders_disabled_reason() {
        let html = action_button_html(
            "/apps/demo/start",
            "/apps/demo",
            "Start",
            &ActionState::disabled("already running"),
        );

        assert!(html.contains("disabled"));
        assert!(html.contains("already running"));
        assert!(!html.contains("method=\"post\""));
    }

    #[test]
    fn detect_stack_in_dir_reports_detected_stack() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        assert_eq!(detect_stack_in_dir(tmp.path()).as_deref(), Some("rust"));
    }

    #[test]
    fn detect_stack_in_dir_marks_custom_stack() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("index.html"), "<!doctype html>\n").unwrap();
        fs::write(tmp.path().join("psht-stack.sh"), "#!/bin/sh\n").unwrap();

        assert_eq!(
            detect_stack_in_dir(tmp.path()).as_deref(),
            Some("custom (static)")
        );
    }
}
