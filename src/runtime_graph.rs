use std::process::Command;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::control_plane;

static CURRENT_PROJECT_NAME: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProject {
    pub name: String,
}

impl RuntimeProject {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into().trim().to_string();
        if name.is_empty() {
            return Err("runtime project name cannot be empty".to_string());
        }
        Ok(Self { name })
    }

    pub fn current() -> Result<Self, String> {
        if let Some(name) = CURRENT_PROJECT_NAME.get() {
            return Ok(Self { name: name.clone() });
        }

        let output = Command::new("id")
            .arg("-u")
            .output()
            .map_err(|e| format!("failed to resolve current uid for runtime project: {e}"))?;
        if !output.status.success() {
            return Err("failed to resolve current uid for runtime project".to_string());
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if uid.is_empty() {
            return Err("current uid was empty while resolving runtime project".to_string());
        }
        let project_name = format!("user-{uid}");
        let _ = CURRENT_PROJECT_NAME.set(project_name.clone());
        Ok(Self { name: project_name })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceRole {
    Active,
    Candidate,
    Previous,
    Standalone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRef {
    pub app_id: String,
    pub app_ref: String,
    pub instance_name: String,
    pub project: RuntimeProject,
    pub role: InstanceRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRef {
    pub pool: String,
    pub volume: String,
    pub purpose: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetIdentity {
    pub requested_hostname: Option<String>,
    pub observed_dns_name: Option<String>,
    pub device_id: Option<String>,
    pub join_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRuntime {
    pub app_id: String,
    pub project: RuntimeProject,
    pub active_instance: Option<InstanceRef>,
    pub candidate_instance: Option<InstanceRef>,
    pub previous_instance: Option<InstanceRef>,
    pub storage: Option<StorageRef>,
    pub tailscale_state: Option<StorageRef>,
    pub tailnet: Option<TailnetIdentity>,
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

fn project_from_persisted(raw: Option<&str>) -> Result<RuntimeProject, String> {
    if let Some(raw) = raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return RuntimeProject::new(trimmed);
        }
    }
    RuntimeProject::current()
}

fn instance_ref(
    app_id: &str,
    instance_name: &str,
    project: &RuntimeProject,
    role: InstanceRole,
) -> InstanceRef {
    InstanceRef {
        app_id: app_id.to_string(),
        app_ref: app_ref_from_instance_name(instance_name).unwrap_or_else(|| app_id.to_string()),
        instance_name: instance_name.to_string(),
        project: project.clone(),
        role,
    }
}

pub fn app_runtime(app_id: &str) -> Result<Option<AppRuntime>, String> {
    let Some(state) = control_plane::read_app_runtime_state(app_id)? else {
        return Ok(None);
    };
    let project = project_from_persisted(state.runtime_project.as_deref())?;
    Ok(Some(AppRuntime {
        app_id: app_id.to_string(),
        project: project.clone(),
        active_instance: Some(instance_ref(
            app_id,
            &state.active_instance,
            &project,
            InstanceRole::Active,
        )),
        candidate_instance: None,
        previous_instance: state
            .previous_instance
            .as_deref()
            .map(|instance| instance_ref(app_id, instance, &project, InstanceRole::Previous)),
        storage: None,
        tailscale_state: None,
        tailnet: None,
    }))
}

pub fn resolve_instance(app_ref: &str) -> Result<InstanceRef, String> {
    if let Some(runtime) = app_runtime(app_ref)? {
        if let Some(active) = runtime.active_instance.as_ref()
            && active.app_ref == app_ref
        {
            return Ok(active.clone());
        }
        if let Some(previous) = runtime.previous_instance.as_ref()
            && previous.app_ref == app_ref
        {
            return Ok(previous.clone());
        }
    }

    let instance_name = instance_name_from_app_ref(app_ref);
    for (app_id, state) in control_plane::read_all_app_runtime_states()? {
        let project = project_from_persisted(state.runtime_project.as_deref())?;
        if state.active_instance == instance_name {
            return Ok(instance_ref(
                &app_id,
                &state.active_instance,
                &project,
                InstanceRole::Active,
            ));
        }
        if let Some(previous_instance) = state.previous_instance.as_deref()
            && previous_instance == instance_name
        {
            return Ok(instance_ref(
                &app_id,
                previous_instance,
                &project,
                InstanceRole::Previous,
            ));
        }
    }

    let project = RuntimeProject::current()?;
    Ok(InstanceRef {
        app_id: app_ref.to_string(),
        app_ref: app_ref.to_string(),
        instance_name,
        project,
        role: InstanceRole::Standalone,
    })
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
