use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GitCheckoutTarget {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitDeployStatus {
    Pending,
    Success,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GitDeployState {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub status: GitDeployStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PendingGitDeployRequest {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_requested_at: Option<u64>,
}

impl PendingGitDeployRequest {
    pub fn from_target(
        target: &GitCheckoutTarget,
        force: bool,
        request_id: Option<String>,
        requested_at: Option<u64>,
    ) -> Self {
        Self {
            ref_name: target.ref_name.clone(),
            sha: target.sha.clone(),
            force,
            interrupt_requested_at: requested_at,
            request_id,
        }
    }

    pub fn target(&self) -> GitCheckoutTarget {
        GitCheckoutTarget {
            ref_name: self.ref_name.clone(),
            sha: self.sha.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeployInterruptState {
    pub request_id: String,
    pub requested_at: u64,
    pub target_sha: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeployLockMetadata {
    pub pid: Option<u32>,
    pub created: Option<u64>,
    pub updated: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CleanupJobState {
    pub app: String,
    pub active_instance_at_schedule: String,
    pub scheduled_previous_instance: String,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub scheduled_at: u64,
    pub updated_at: u64,
}
