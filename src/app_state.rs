use std::time::{SystemTime, UNIX_EPOCH};

use crate::container;
use crate::control_plane::{self, AppRuntimeState, DesiredState, RuntimeSnapshot};

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

pub fn app_ref_from_instance_name(instance: &str) -> Option<String> {
    let trimmed = instance.trim();
    if trimmed.is_empty() {
        return None;
    }
    let app_ref = trimmed.strip_prefix("psht-").unwrap_or(trimmed).trim();
    if app_ref.is_empty() {
        return None;
    }
    Some(app_ref.to_string())
}

pub fn instance_name_from_app_ref(app_ref: &str) -> String {
    let app_ref = app_ref.trim();
    if app_ref.starts_with("psht-") {
        app_ref.to_string()
    } else {
        format!("psht-{app_ref}")
    }
}

pub fn read_app_runtime_state(app: &str) -> Result<Option<AppRuntimeState>, String> {
    control_plane::read_app_runtime_state(app)
}

pub fn write_app_runtime_state(
    app: &str,
    active_app_ref: &str,
    previous_app_ref: Option<&str>,
) -> Result<(), String> {
    control_plane::write_app_runtime_state(
        app,
        &AppRuntimeState {
            active_instance: instance_name_from_app_ref(active_app_ref),
            previous_instance: previous_app_ref.map(instance_name_from_app_ref),
            updated_at: now_unix_secs(),
        },
    )
}

pub fn clear_app_runtime_state(app: &str) -> Result<(), String> {
    control_plane::clear_app_runtime_state(app)
}

pub fn read_all_app_runtime_states() -> Result<Vec<(String, AppRuntimeState)>, String> {
    control_plane::read_all_app_runtime_states()
}

pub fn resolve_active_app_ref(app: &str) -> Result<Option<String>, String> {
    if let Some(state) = read_app_runtime_state(app)? {
        if let Some(active_app_ref) = app_ref_from_instance_name(&state.active_instance)
            && container::exists(&active_app_ref)
        {
            return Ok(Some(active_app_ref));
        }
        if let Some(previous_instance) = state.previous_instance.as_deref()
            && let Some(previous_app_ref) = app_ref_from_instance_name(previous_instance)
            && container::exists(&previous_app_ref)
        {
            write_app_runtime_state(app, &previous_app_ref, None)?;
            return Ok(Some(previous_app_ref));
        }
        if container::exists(app) {
            write_app_runtime_state(app, app, None)?;
            return Ok(Some(app.to_string()));
        }
        return Ok(None);
    }

    if container::exists(app) {
        write_app_runtime_state(app, app, None)?;
        return Ok(Some(app.to_string()));
    }

    Ok(None)
}

pub fn resolve_existing_active_app_ref(app: &str) -> Result<String, String> {
    let Some(active_app_ref) = resolve_active_app_ref(app)? else {
        return Err(format!("app '{app}' not found"));
    };
    Ok(active_app_ref)
}

pub fn runtime_state_snapshot(app: &str) -> (Option<String>, Option<String>) {
    if let Ok(Some(state)) = read_app_runtime_state(app) {
        return (Some(state.active_instance), state.previous_instance);
    }
    let active = resolve_active_app_ref(app)
        .ok()
        .flatten()
        .map(|app_ref| instance_name_from_app_ref(&app_ref));
    (active, None)
}

pub fn control_plane_snapshot(app: &str, active_revision: Option<String>) -> RuntimeSnapshot {
    let (active_instance, previous_instance) = runtime_state_snapshot(app);
    RuntimeSnapshot {
        active_instance,
        previous_instance,
        active_revision,
        ..RuntimeSnapshot::default()
    }
}

pub fn desired_state(app: &str) -> Result<DesiredState, String> {
    control_plane::desired_state(app)
}

pub fn set_desired_state(app: &str, desired_state: DesiredState) -> Result<(), String> {
    control_plane::set_desired_state(app, desired_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ref_from_instance_name_handles_prefixed_and_unprefixed_values() {
        assert_eq!(app_ref_from_instance_name("psht-hyperlinked").as_deref(), Some("hyperlinked"));
        assert_eq!(
            app_ref_from_instance_name("hyperlinked-build-123").as_deref(),
            Some("hyperlinked-build-123")
        );
        assert!(app_ref_from_instance_name("").is_none());
    }

    #[test]
    fn instance_name_from_app_ref_prefixes_plain_names() {
        assert_eq!(instance_name_from_app_ref("demo"), "psht-demo");
        assert_eq!(instance_name_from_app_ref("psht-demo"), "psht-demo");
    }
}
