#![forbid(unsafe_code)]

//! Permission 是策略层；Sandbox 是技术执行层，两者不可混用。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};

use harness_types::{
    ActorId, InformationFlowLabel, MissionId, PermissionGrantId, PermissionRequestId, ProjectId,
    RunId,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    UntrustedOnly,
    OnRequest,
    Always,
    NeverWithinSandbox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionRuleEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionRuleAction {
    Read,
    Write,
    Execute,
    Network,
    Browser,
    Mcp,
    Plugin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRule {
    pub id: String,
    pub effect: PermissionRuleEffect,
    pub action: PermissionRuleAction,
    pub pattern: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PermissionRuleFile {
    pub schema_version: u32,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

pub fn load_permission_rules(path: &Path) -> Result<PermissionRuleFile, PermissionError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PermissionError::new("permission-rules-io", error.to_string()))?;
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        return Err(PermissionError::new(
            "permission-rules-target-invalid",
            path.display().to_string(),
        ));
    }
    let file: PermissionRuleFile = toml::from_str(
        &fs::read_to_string(path)
            .map_err(|error| PermissionError::new("permission-rules-io", error.to_string()))?,
    )
    .map_err(|error| PermissionError::new("permission-rules-toml", error.to_string()))?;
    if file.schema_version != 1 {
        return Err(PermissionError::new(
            "permission-rules-schema-unsupported",
            file.schema_version.to_string(),
        ));
    }
    validate_rules(&file.rules)?;
    Ok(file)
}

pub fn save_permission_rules_atomic(
    path: &Path,
    rules: &[PermissionRule],
) -> Result<(), PermissionError> {
    validate_rules(rules)?;
    let parent = path
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| {
            PermissionError::new(
                "permission-rules-parent-invalid",
                path.display().to_string(),
            )
        })?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.file_type().is_file())
    {
        return Err(PermissionError::new(
            "permission-rules-target-invalid",
            path.display().to_string(),
        ));
    }
    let mut rules = rules.to_vec();
    rules.sort_by(|left, right| left.id.cmp(&right.id));
    let text = toml::to_string_pretty(&PermissionRuleFile {
        schema_version: 1,
        rules,
    })
    .map_err(|error| PermissionError::new("permission-rules-serialize", error.to_string()))?;
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    );
    let temporary = parent.join(format!(".kernary-permissions-new-{suffix}.toml"));
    let backup = parent.join(format!(".kernary-permissions-backup-{suffix}.toml"));
    let write_result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(PermissionError::new(
            "permission-rules-write",
            error.to_string(),
        ));
    }
    if !path.exists() {
        return fs::rename(&temporary, path)
            .map_err(|error| PermissionError::new("permission-rules-swap", error.to_string()));
    }
    fs::rename(path, &backup)
        .map_err(|error| PermissionError::new("permission-rules-backup", error.to_string()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::rename(&backup, path);
        let _ = fs::remove_file(&temporary);
        return Err(PermissionError::new(
            "permission-rules-swap",
            error.to_string(),
        ));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionProfile {
    pub id: String,
    pub name: String,
    pub filesystem: FilesystemPolicy,
    pub subprocess: SubprocessPolicy,
    pub network: NetworkPolicy,
    pub browser: BrowserPolicy,
    pub mcp: McpPolicy,
    pub plugin: PluginPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemPolicy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubprocessPolicy {
    pub enabled: bool,
    pub allowed_executables: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkPolicy {
    pub enabled: bool,
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserPolicy {
    pub enabled: bool,
    pub allow_uploads: bool,
    pub allow_downloads: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpPolicy {
    pub allowed_server_ids: Vec<String>,
    pub allowed_tool_patterns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginPolicy {
    pub allowed_plugin_ids: Vec<String>,
    pub allowed_capability_patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionEnvelope {
    pub project_id: ProjectId,
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub actor_id: ActorId,
    pub origin: InvocationOrigin,
    pub information_flow: InformationFlowLabel,
}

/// 谁直接触发了这次能力调用。权限策略不能只依赖可伪造的 actor 名称。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationOrigin {
    User,
    Agent,
    System,
    Plugin,
    Mcp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PermissionAction {
    InternalCompute {
        capability: String,
    },
    FilesystemRead {
        path: PathBuf,
    },
    FilesystemWrite {
        path: PathBuf,
    },
    WorkspacePatchApply {
        operation: String,
        preview_id: String,
        preview_fingerprint: String,
        paths: Vec<PathBuf>,
    },
    ProcessSpawn {
        executable: PathBuf,
        arguments: Vec<String>,
        cwd: PathBuf,
    },
    NetworkConnect {
        host: String,
    },
    BrowserOpen {
        origin: String,
    },
    BrowserSnapshot {
        origin: String,
    },
    BrowserAct {
        origin: String,
        action: BrowserAction,
    },
    BrowserUpload {
        origin: String,
        path: PathBuf,
    },
    BrowserDownload {
        origin: String,
    },
    McpCall {
        server_id: String,
        tool_name: String,
        side_effect: bool,
        arguments_sha256: String,
    },
    PluginCall {
        plugin_id: String,
        capability: String,
        side_effect: bool,
        arguments_sha256: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserAction {
    Click,
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantScope {
    Once,
    Run,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: PermissionRequestId,
    pub envelope: ExecutionEnvelope,
    pub action: PermissionAction,
    pub action_key: String,
    pub reason: String,
    pub risk: RiskLevel,
    pub sandbox_allowed: bool,
    pub available_scopes: Vec<GrantScope>,
    pub status: ApprovalStatus,
    pub created_at_millis: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionGrant {
    pub id: PermissionGrantId,
    pub request_id: PermissionRequestId,
    pub action_key: String,
    pub project_id: ProjectId,
    pub run_id: Option<RunId>,
    pub scope: GrantScope,
    pub remaining_uses: Option<u32>,
    pub created_at_millis: i64,
    pub revoked_at_millis: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionAllowSource {
    Sandbox,
    Grant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow {
        source: PermissionAllowSource,
        grant_id: Option<PermissionGrantId>,
    },
    Deny {
        reason: String,
        hard: bool,
    },
    RequestApproval(Box<ApprovalRequest>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionError {
    pub code: &'static str,
    pub message: String,
}

impl PermissionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for PermissionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for PermissionError {}

pub struct PermissionEngine {
    profile: PermissionProfile,
    approval_policy: ApprovalPolicy,
    rules: Vec<PermissionRule>,
    requests: BTreeMap<PermissionRequestId, ApprovalRequest>,
    grants: BTreeMap<PermissionGrantId, PermissionGrant>,
}

impl PermissionEngine {
    #[must_use]
    pub fn new(profile: PermissionProfile, approval_policy: ApprovalPolicy) -> Self {
        Self {
            profile,
            approval_policy,
            rules: Vec::new(),
            requests: BTreeMap::new(),
            grants: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn profile(&self) -> &PermissionProfile {
        &self.profile
    }

    #[must_use]
    pub const fn approval_policy(&self) -> ApprovalPolicy {
        self.approval_policy
    }

    pub fn set_approval_policy(&mut self, approval_policy: ApprovalPolicy) {
        self.approval_policy = approval_policy;
    }

    pub fn allow_mcp_server(&mut self, server_id: &str) -> Result<(), PermissionError> {
        if server_id.trim().is_empty() {
            return Err(PermissionError::new("mcp-server-id-invalid", server_id));
        }
        if !self
            .profile
            .mcp
            .allowed_server_ids
            .iter()
            .any(|configured| configured == server_id)
        {
            self.profile
                .mcp
                .allowed_server_ids
                .push(server_id.to_owned());
            self.profile.mcp.allowed_server_ids.sort();
        }
        Ok(())
    }

    pub fn remove_mcp_server(&mut self, server_id: &str) {
        self.profile
            .mcp
            .allowed_server_ids
            .retain(|configured| configured != server_id);
    }

    pub fn replace_rules(&mut self, rules: Vec<PermissionRule>) -> Result<(), PermissionError> {
        validate_rules(&rules)?;
        self.rules = rules;
        Ok(())
    }

    #[must_use]
    pub fn rules(&self) -> Vec<PermissionRule> {
        self.rules.clone()
    }

    pub fn add_rule(&mut self, rule: PermissionRule) -> Result<(), PermissionError> {
        validate_rule(&rule)?;
        if self.rules.iter().any(|existing| existing.id == rule.id) {
            return Err(PermissionError::new("permission-rule-exists", rule.id));
        }
        self.rules.push(rule);
        Ok(())
    }

    pub fn remove_rule(&mut self, rule_id: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|rule| rule.id != rule_id);
        self.rules.len() != before
    }

    pub fn evaluate(
        &mut self,
        action: PermissionAction,
        envelope: &ExecutionEnvelope,
        request_id: PermissionRequestId,
        now_millis: i64,
    ) -> Result<PermissionDecision, PermissionError> {
        if let Some(reason) = hard_deny_reason(&self.profile, &action)? {
            return Ok(PermissionDecision::Deny { reason, hard: true });
        }
        let matching_rule = best_matching_rule(&self.rules, &self.profile, &action);
        if matches!(
            matching_rule.map(|rule| rule.effect),
            Some(PermissionRuleEffect::Deny)
        ) {
            return Ok(PermissionDecision::Deny {
                reason: format!(
                    "命中 Permission deny rule {}",
                    matching_rule.expect("deny rule exists").id
                ),
                hard: true,
            });
        }
        let key = action_key(&action)?;
        if let Some(grant_id) = self.find_grant(&key, envelope) {
            let grant = self
                .grants
                .get_mut(&grant_id)
                .expect("grant ID 来自 registry");
            if grant.scope == GrantScope::Once {
                grant.remaining_uses = Some(grant.remaining_uses.unwrap_or(1).saturating_sub(1));
            }
            return Ok(PermissionDecision::Allow {
                source: PermissionAllowSource::Grant,
                grant_id: Some(grant_id),
            });
        }
        let sandbox_allowed = allowed_by_profile(&self.profile, &action)?;
        if sandbox_allowed
            && matches!(
                matching_rule.map(|rule| rule.effect),
                Some(PermissionRuleEffect::Allow)
            )
        {
            return Ok(PermissionDecision::Allow {
                source: PermissionAllowSource::Sandbox,
                grant_id: None,
            });
        }
        let agent_high_risk = envelope.origin != InvocationOrigin::User
            && matches!(risk_of(&action), RiskLevel::High | RiskLevel::Critical);
        let mandatory_workspace_patch =
            matches!(action, PermissionAction::WorkspacePatchApply { .. });
        let policy_requests = mandatory_workspace_patch
            || matches!(
                matching_rule.map(|rule| rule.effect),
                Some(PermissionRuleEffect::Ask)
            )
            || self.approval_policy == ApprovalPolicy::Always
            || (self.approval_policy == ApprovalPolicy::UntrustedOnly
                && envelope.information_flow.integrity == harness_types::IntegrityLabel::Untrusted)
            || (self.approval_policy == ApprovalPolicy::OnRequest
                && (!sandbox_allowed || agent_high_risk));
        if sandbox_allowed && !policy_requests {
            return Ok(PermissionDecision::Allow {
                source: PermissionAllowSource::Sandbox,
                grant_id: None,
            });
        }
        if self.requests.contains_key(&request_id) {
            return Err(PermissionError::new(
                "approval-request-exists",
                request_id.to_string(),
            ));
        }
        let request = ApprovalRequest {
            id: request_id.clone(),
            envelope: envelope.clone(),
            action: action.clone(),
            action_key: key,
            reason: if sandbox_allowed {
                format!("{} 命中当前 approval policy", action_kind(&action))
            } else {
                format!(
                    "{} 不在当前 sandbox profile 允许范围内",
                    action_kind(&action)
                )
            },
            risk: risk_of(&action),
            sandbox_allowed,
            available_scopes: if envelope.run_id.is_some() {
                vec![GrantScope::Once, GrantScope::Run, GrantScope::Project]
            } else {
                vec![GrantScope::Once, GrantScope::Project]
            },
            status: ApprovalStatus::Pending,
            created_at_millis: now_millis,
        };
        self.requests.insert(request_id, request.clone());
        Ok(PermissionDecision::RequestApproval(Box::new(request)))
    }

    pub fn respond(
        &mut self,
        request_id: &PermissionRequestId,
        allow: bool,
        scope: GrantScope,
        grant_id: PermissionGrantId,
        now_millis: i64,
    ) -> Result<Option<PermissionGrant>, PermissionError> {
        let request = self.requests.get_mut(request_id).ok_or_else(|| {
            PermissionError::new("approval-request-not-found", request_id.to_string())
        })?;
        if request.status != ApprovalStatus::Pending {
            return Err(PermissionError::new(
                "approval-request-not-pending",
                request_id.to_string(),
            ));
        }
        if !allow {
            request.status = ApprovalStatus::Denied;
            return Ok(None);
        }
        if !request.available_scopes.contains(&scope) {
            return Err(PermissionError::new(
                "approval-scope-not-available",
                format!("{scope:?}"),
            ));
        }
        if self.grants.contains_key(&grant_id) {
            return Err(PermissionError::new(
                "permission-grant-exists",
                grant_id.to_string(),
            ));
        }
        request.status = ApprovalStatus::Allowed;
        let grant = PermissionGrant {
            id: grant_id.clone(),
            request_id: request_id.clone(),
            action_key: request.action_key.clone(),
            project_id: request.envelope.project_id.clone(),
            run_id: (scope == GrantScope::Run)
                .then(|| request.envelope.run_id.clone())
                .flatten(),
            scope,
            remaining_uses: (scope == GrantScope::Once).then_some(1),
            created_at_millis: now_millis,
            revoked_at_millis: None,
        };
        self.grants.insert(grant_id, grant.clone());
        Ok(Some(grant))
    }

    pub fn revoke(&mut self, grant_id: &PermissionGrantId, now_millis: i64) {
        if let Some(grant) = self.grants.get_mut(grant_id)
            && grant.revoked_at_millis.is_none()
        {
            grant.revoked_at_millis = Some(now_millis);
        }
    }

    pub fn restore_pending_request(
        &mut self,
        id: PermissionRequestId,
        action: PermissionAction,
        envelope: ExecutionEnvelope,
        created_at_millis: i64,
    ) -> Result<ApprovalRequest, PermissionError> {
        if let Some(existing) = self.requests.get(&id) {
            if existing.status == ApprovalStatus::Pending
                && existing.action == action
                && existing.envelope == envelope
            {
                return Ok(existing.clone());
            }
            return Err(PermissionError::new(
                "approval-restore-conflict",
                id.to_string(),
            ));
        }
        if let Some(reason) = hard_deny_reason(&self.profile, &action)? {
            return Err(PermissionError::new("approval-now-hard-denied", reason));
        }
        let sandbox_allowed = allowed_by_profile(&self.profile, &action)?;
        let request = ApprovalRequest {
            id: id.clone(),
            action_key: action_key(&action)?,
            reason: if sandbox_allowed {
                format!("{} 命中恢复的 approval", action_kind(&action))
            } else {
                format!("{} 仍超出 sandbox profile", action_kind(&action))
            },
            risk: risk_of(&action),
            available_scopes: if envelope.run_id.is_some() {
                vec![GrantScope::Once, GrantScope::Run, GrantScope::Project]
            } else {
                vec![GrantScope::Once, GrantScope::Project]
            },
            envelope,
            action,
            sandbox_allowed,
            status: ApprovalStatus::Pending,
            created_at_millis,
        };
        self.requests.insert(id, request.clone());
        Ok(request)
    }

    #[must_use]
    pub fn pending_requests(&self) -> Vec<ApprovalRequest> {
        self.requests
            .values()
            .filter(|request| request.status == ApprovalStatus::Pending)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn active_grants(&self) -> Vec<PermissionGrant> {
        self.grants
            .values()
            .filter(|grant| {
                grant.revoked_at_millis.is_none()
                    && (grant.scope != GrantScope::Once || grant.remaining_uses.unwrap_or(0) > 0)
            })
            .cloned()
            .collect()
    }

    fn find_grant(&self, key: &str, envelope: &ExecutionEnvelope) -> Option<PermissionGrantId> {
        self.active_grants()
            .into_iter()
            .find(|grant| {
                grant.action_key == key
                    && grant.project_id == envelope.project_id
                    && (grant.scope == GrantScope::Project
                        || grant.scope == GrantScope::Once
                        || grant.run_id == envelope.run_id)
            })
            .map(|grant| grant.id)
    }
}

fn validate_rules(rules: &[PermissionRule]) -> Result<(), PermissionError> {
    let mut ids = std::collections::BTreeSet::new();
    for rule in rules {
        validate_rule(rule)?;
        if !ids.insert(rule.id.clone()) {
            return Err(PermissionError::new("permission-rule-duplicate", &rule.id));
        }
    }
    Ok(())
}

fn validate_rule(rule: &PermissionRule) -> Result<(), PermissionError> {
    let valid_id = !rule.id.is_empty()
        && rule.id.len() <= 128
        && rule.id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_' | b'.' | b':'))
        });
    let valid_pattern = !rule.pattern.trim().is_empty()
        && rule.pattern.len() <= 512
        && !rule.pattern.chars().any(char::is_control);
    if !valid_id || !valid_pattern {
        return Err(PermissionError::new("permission-rule-invalid", &rule.id));
    }
    Ok(())
}

fn best_matching_rule<'a>(
    rules: &'a [PermissionRule],
    profile: &PermissionProfile,
    action: &PermissionAction,
) -> Option<&'a PermissionRule> {
    rules
        .iter()
        .filter(|rule| rule_matches(rule, profile, action))
        .max_by_key(|rule| {
            let effect = match rule.effect {
                PermissionRuleEffect::Allow => 1,
                PermissionRuleEffect::Ask => 2,
                PermissionRuleEffect::Deny => 3,
            };
            (
                effect,
                rule.pattern.len(),
                std::cmp::Reverse(rule.id.as_str()),
            )
        })
}

fn rule_matches(
    rule: &PermissionRule,
    profile: &PermissionProfile,
    action: &PermissionAction,
) -> bool {
    match (rule.action, action) {
        (PermissionRuleAction::Read, PermissionAction::FilesystemRead { path }) => {
            path_rule_matches(&rule.pattern, path, &profile.filesystem.read_roots)
        }
        (PermissionRuleAction::Write, PermissionAction::FilesystemWrite { path }) => {
            path_rule_matches(&rule.pattern, path, &profile.filesystem.write_roots)
        }
        (PermissionRuleAction::Write, PermissionAction::WorkspacePatchApply { paths, .. }) => {
            let matches = |path: &PathBuf| {
                path_rule_matches(&rule.pattern, path, &profile.filesystem.write_roots)
            };
            if rule.effect == PermissionRuleEffect::Allow {
                !paths.is_empty() && paths.iter().all(matches)
            } else {
                paths.iter().any(matches)
            }
        }
        (
            PermissionRuleAction::Execute,
            PermissionAction::ProcessSpawn {
                executable,
                arguments,
                ..
            },
        ) => {
            let executable = normalize_path(executable);
            let basename = Path::new(&executable)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&executable)
                .to_owned();
            let full_argv = std::iter::once(executable.clone())
                .chain(arguments.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            let basename_argv = std::iter::once(basename)
                .chain(arguments.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            rule_wildcard_match(&rule.pattern, &executable)
                || rule_wildcard_match(&rule.pattern, &full_argv)
                || rule_wildcard_match(&rule.pattern, &basename_argv)
        }
        (PermissionRuleAction::Network, PermissionAction::NetworkConnect { host }) => {
            rule_wildcard_match(&rule.pattern, host)
        }
        (PermissionRuleAction::Browser, PermissionAction::BrowserOpen { origin })
        | (PermissionRuleAction::Browser, PermissionAction::BrowserSnapshot { origin })
        | (PermissionRuleAction::Browser, PermissionAction::BrowserAct { origin, .. })
        | (PermissionRuleAction::Browser, PermissionAction::BrowserUpload { origin, .. })
        | (PermissionRuleAction::Browser, PermissionAction::BrowserDownload { origin }) => {
            rule_wildcard_match(&rule.pattern, origin)
        }
        (
            PermissionRuleAction::Mcp,
            PermissionAction::McpCall {
                server_id,
                tool_name,
                ..
            },
        ) => rule_wildcard_match(&rule.pattern, &format!("{server_id}/{tool_name}")),
        (
            PermissionRuleAction::Plugin,
            PermissionAction::PluginCall {
                plugin_id,
                capability,
                ..
            },
        ) => rule_wildcard_match(&rule.pattern, &format!("{plugin_id}/{capability}")),
        _ => false,
    }
}

fn path_rule_matches(pattern: &str, path: &Path, roots: &[PathBuf]) -> bool {
    let normalized = normalize_path(path);
    let pattern = pattern.replace('\\', "/");
    if rule_wildcard_match(&pattern, &normalized) {
        return true;
    }
    if let Some(home_relative) = pattern.strip_prefix("~/") {
        if rule_wildcard_match(home_relative, &normalized) {
            return true;
        }
        for (index, _) in normalized.match_indices('/') {
            if rule_wildcard_match(home_relative, &normalized[index + 1..]) {
                return true;
            }
        }
    }
    let relative_pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
    roots.iter().any(|root| {
        path.strip_prefix(root).ok().is_some_and(|relative| {
            rule_wildcard_match(relative_pattern, &normalize_path(relative))
        })
    })
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn rule_wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut matches = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    matches[0][0] = true;
    let mut pattern_index = 0;
    while pattern_index < pattern.len() {
        let double_star =
            pattern[pattern_index] == b'*' && pattern.get(pattern_index + 1).copied() == Some(b'*');
        let step = usize::from(double_star) + 1;
        for value_index in 0..=value.len() {
            if !matches[pattern_index][value_index] {
                continue;
            }
            match pattern[pattern_index] {
                b'*' if double_star => {
                    matches[pattern_index + step][value_index] = true;
                    if value_index < value.len() {
                        matches[pattern_index][value_index + 1] = true;
                    }
                }
                b'*' => {
                    matches[pattern_index + 1][value_index] = true;
                    if value_index < value.len() && value[value_index] != b'/' {
                        matches[pattern_index][value_index + 1] = true;
                    }
                }
                b'?' if value_index < value.len() && value[value_index] != b'/' => {
                    matches[pattern_index + 1][value_index + 1] = true;
                }
                byte if value.get(value_index).copied() == Some(byte) => {
                    matches[pattern_index + 1][value_index + 1] = true;
                }
                _ => {}
            }
        }
        pattern_index += step;
    }
    matches[pattern.len()][value.len()]
}

fn action_kind(action: &PermissionAction) -> &'static str {
    match action {
        PermissionAction::InternalCompute { .. } => "internal.compute",
        PermissionAction::FilesystemRead { .. } => "filesystem.read",
        PermissionAction::FilesystemWrite { .. } => "filesystem.write",
        PermissionAction::WorkspacePatchApply { .. } => "workspace.patch.apply",
        PermissionAction::ProcessSpawn { .. } => "process.spawn",
        PermissionAction::NetworkConnect { .. } => "network.connect",
        PermissionAction::BrowserOpen { .. } => "browser.open",
        PermissionAction::BrowserSnapshot { .. } => "browser.snapshot",
        PermissionAction::BrowserAct { .. } => "browser.act",
        PermissionAction::BrowserUpload { .. } => "browser.upload",
        PermissionAction::BrowserDownload { .. } => "browser.download",
        PermissionAction::McpCall { .. } => "mcp.call",
        PermissionAction::PluginCall { .. } => "plugin.call",
    }
}

fn action_key(action: &PermissionAction) -> Result<String, PermissionError> {
    let key = match action {
        PermissionAction::InternalCompute { capability } => {
            format!("internal.compute:{capability}")
        }
        PermissionAction::FilesystemRead { path } => {
            format!("filesystem.read:{}", normalized_path(path)?)
        }
        PermissionAction::FilesystemWrite { path } => {
            format!("filesystem.write:{}", normalized_path(path)?)
        }
        PermissionAction::WorkspacePatchApply {
            operation,
            preview_id,
            preview_fingerprint,
            paths,
        } => {
            let mut paths = paths
                .iter()
                .map(|path| normalized_path(path))
                .collect::<Result<Vec<_>, _>>()?;
            paths.sort();
            paths.dedup();
            format!(
                "workspace.patch.apply:{operation}:{preview_id}:{preview_fingerprint}:{}",
                paths.join("|")
            )
        }
        PermissionAction::ProcessSpawn {
            executable,
            arguments,
            cwd,
        } => {
            let arguments = serde_json::to_string(arguments).map_err(|error| {
                PermissionError::new("process-arguments-invalid", error.to_string())
            })?;
            format!(
                "process.spawn:{}:{}:{arguments}",
                normalized_path(executable)?,
                normalized_path(cwd)?
            )
        }
        PermissionAction::NetworkConnect { host } => {
            format!("network.connect:{}", host.to_ascii_lowercase())
        }
        PermissionAction::BrowserOpen { origin } => {
            format!("browser.open:{}", origin.to_ascii_lowercase())
        }
        PermissionAction::BrowserSnapshot { origin } => {
            format!("browser.snapshot:{}", origin.to_ascii_lowercase())
        }
        PermissionAction::BrowserAct { origin, action } => {
            format!("browser.act:{}:{action:?}", origin.to_ascii_lowercase())
        }
        PermissionAction::BrowserUpload { origin, path } => format!(
            "browser.upload:{}:{}",
            origin.to_ascii_lowercase(),
            normalized_path(path)?
        ),
        PermissionAction::BrowserDownload { origin } => {
            format!("browser.download:{}", origin.to_ascii_lowercase())
        }
        PermissionAction::McpCall {
            server_id,
            tool_name,
            arguments_sha256,
            ..
        } => format!("mcp.call:{server_id}:{tool_name}:{arguments_sha256}"),
        PermissionAction::PluginCall {
            plugin_id,
            capability,
            arguments_sha256,
            ..
        } => format!("plugin.call:{plugin_id}:{capability}:{arguments_sha256}"),
    };
    Ok(key)
}

fn risk_of(action: &PermissionAction) -> RiskLevel {
    match action {
        PermissionAction::InternalCompute { .. }
        | PermissionAction::FilesystemRead { .. }
        | PermissionAction::BrowserOpen { .. }
        | PermissionAction::BrowserSnapshot { .. } => RiskLevel::Low,
        PermissionAction::FilesystemWrite { .. }
        | PermissionAction::BrowserDownload { .. }
        | PermissionAction::BrowserAct { .. } => RiskLevel::Medium,
        PermissionAction::BrowserUpload { .. } => RiskLevel::Critical,
        PermissionAction::PluginCall {
            side_effect: false, ..
        } => RiskLevel::Low,
        PermissionAction::ProcessSpawn { .. }
        | PermissionAction::WorkspacePatchApply { .. }
        | PermissionAction::NetworkConnect { .. }
        | PermissionAction::McpCall { .. }
        | PermissionAction::PluginCall { .. } => RiskLevel::High,
    }
}

fn hard_deny_reason(
    profile: &PermissionProfile,
    action: &PermissionAction,
) -> Result<Option<String>, PermissionError> {
    let path = match action {
        PermissionAction::FilesystemRead { path }
        | PermissionAction::FilesystemWrite { path }
        | PermissionAction::BrowserUpload { path, .. } => Some(path),
        PermissionAction::WorkspacePatchApply { paths, .. } => {
            for path in paths {
                for root in &profile.filesystem.denied_roots {
                    if is_inside(root, path)? {
                        return Ok(Some(format!("目标位于 denied root：{}", root.display())));
                    }
                }
            }
            None
        }
        _ => None,
    };
    if let Some(path) = path {
        for root in &profile.filesystem.denied_roots {
            if is_inside(root, path)? {
                return Ok(Some(format!("目标位于 denied root：{}", root.display())));
            }
        }
    }
    Ok(None)
}

fn allowed_by_profile(
    profile: &PermissionProfile,
    action: &PermissionAction,
) -> Result<bool, PermissionError> {
    Ok(match action {
        PermissionAction::InternalCompute { .. } => true,
        PermissionAction::FilesystemRead { path } => profile
            .filesystem
            .read_roots
            .iter()
            .any(|root| is_inside(root, path).unwrap_or(false)),
        PermissionAction::FilesystemWrite { path } => profile
            .filesystem
            .write_roots
            .iter()
            .any(|root| is_inside(root, path).unwrap_or(false)),
        PermissionAction::WorkspacePatchApply { paths, .. } => {
            !paths.is_empty()
                && paths.iter().all(|path| {
                    profile
                        .filesystem
                        .write_roots
                        .iter()
                        .any(|root| is_inside(root, path).unwrap_or(false))
                })
        }
        PermissionAction::ProcessSpawn { executable, .. } => {
            profile.subprocess.enabled
                && (profile.subprocess.allowed_executables.is_empty()
                    || profile
                        .subprocess
                        .allowed_executables
                        .iter()
                        .any(|allowed| paths_equal(allowed, executable).unwrap_or(false)))
        }
        PermissionAction::NetworkConnect { host } => {
            profile.network.enabled
                && profile
                    .network
                    .allowed_hosts
                    .iter()
                    .any(|pattern| wildcard_match(host, pattern))
        }
        PermissionAction::BrowserOpen { .. }
        | PermissionAction::BrowserSnapshot { .. }
        | PermissionAction::BrowserAct { .. } => profile.browser.enabled,
        PermissionAction::BrowserUpload { .. } => {
            profile.browser.enabled && profile.browser.allow_uploads
        }
        PermissionAction::BrowserDownload { .. } => {
            profile.browser.enabled && profile.browser.allow_downloads
        }
        PermissionAction::McpCall {
            server_id,
            tool_name,
            ..
        } => {
            profile
                .mcp
                .allowed_server_ids
                .iter()
                .any(|allowed| allowed == server_id)
                && profile
                    .mcp
                    .allowed_tool_patterns
                    .iter()
                    .any(|pattern| wildcard_match(tool_name, pattern))
        }
        PermissionAction::PluginCall {
            plugin_id,
            capability,
            ..
        } => {
            profile
                .plugin
                .allowed_plugin_ids
                .iter()
                .any(|allowed| allowed == plugin_id)
                && profile
                    .plugin
                    .allowed_capability_patterns
                    .iter()
                    .any(|pattern| wildcard_match(capability, pattern))
        }
    })
}

fn normalized_path(path: &Path) -> Result<String, PermissionError> {
    if !path.is_absolute() {
        return Err(PermissionError::new(
            "permission-path-not-absolute",
            path.display().to_string(),
        ));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                output.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(PermissionError::new(
                        "permission-path-escapes-root",
                        path.display().to_string(),
                    ));
                }
            }
        }
    }
    let value = output.to_string_lossy().into_owned();
    Ok(if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    })
}

fn is_inside(root: &Path, target: &Path) -> Result<bool, PermissionError> {
    let root = normalized_path(root)?;
    let target = normalized_path(target)?;
    let separator = std::path::MAIN_SEPARATOR;
    Ok(target == root || target.starts_with(&format!("{root}{separator}")))
}

fn paths_equal(left: &Path, right: &Path) -> Result<bool, PermissionError> {
    Ok(normalized_path(left)? == normalized_path(right)?)
}

fn wildcard_match(value: &str, pattern: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    let (mut value_index, mut pattern_index, mut star, mut checkpoint) = (0, 0, None, 0);
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            value_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            checkpoint = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            checkpoint += 1;
            value_index = checkpoint;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

pub fn workspace_write_profile(project_root: PathBuf) -> PermissionProfile {
    PermissionProfile {
        id: "workspace-write".to_owned(),
        name: "Workspace write".to_owned(),
        filesystem: FilesystemPolicy {
            read_roots: vec![project_root.clone()],
            write_roots: vec![project_root],
            denied_roots: vec![],
        },
        subprocess: SubprocessPolicy {
            enabled: true,
            allowed_executables: vec![],
        },
        network: NetworkPolicy::default(),
        browser: BrowserPolicy {
            enabled: true,
            allow_uploads: false,
            allow_downloads: true,
        },
        mcp: McpPolicy::default(),
        plugin: PluginPolicy::default(),
    }
}

#[cfg(test)]
mod tests {
    use harness_types::{ConfidentialityLabel, IntegrityLabel};
    use tempfile::tempdir;

    use super::*;

    fn envelope(run: &str) -> ExecutionEnvelope {
        ExecutionEnvelope {
            project_id: ProjectId::from("project:test"),
            mission_id: MissionId::from("mission:test"),
            run_id: Some(RunId::from(run)),
            actor_id: ActorId::from("agent:test"),
            origin: InvocationOrigin::Agent,
            information_flow: InformationFlowLabel {
                integrity: IntegrityLabel::Trusted,
                confidentiality: ConfidentialityLabel::ProjectPrivate,
            },
        }
    }

    #[test]
    fn workspace_prefix_collision_is_not_inside() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().join("project");
        let mut engine = PermissionEngine::new(
            workspace_write_profile(root.clone()),
            ApprovalPolicy::NeverWithinSandbox,
        );
        let inside = engine
            .evaluate(
                PermissionAction::FilesystemWrite {
                    path: root.join("src/main.rs"),
                },
                &envelope("run:1"),
                PermissionRequestId::from("approval:1"),
                1,
            )
            .expect("inside");
        assert!(matches!(inside, PermissionDecision::Allow { .. }));
        let outside = engine
            .evaluate(
                PermissionAction::FilesystemWrite {
                    path: temporary.path().join("project-other/main.rs"),
                },
                &envelope("run:1"),
                PermissionRequestId::from("approval:2"),
                2,
            )
            .expect("outside");
        assert!(matches!(outside, PermissionDecision::RequestApproval(_)));
    }

    #[test]
    fn denied_root_is_hard_and_never_creates_request() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let mut profile = workspace_write_profile(root.clone());
        profile.filesystem.denied_roots = vec![root.join(".secrets")];
        let mut engine = PermissionEngine::new(profile, ApprovalPolicy::Always);
        let decision = engine
            .evaluate(
                PermissionAction::FilesystemRead {
                    path: root.join(".secrets/token"),
                },
                &envelope("run:1"),
                PermissionRequestId::from("approval:1"),
                1,
            )
            .expect("decision");
        assert!(matches!(
            decision,
            PermissionDecision::Deny { hard: true, .. }
        ));
        engine.set_approval_policy(ApprovalPolicy::NeverWithinSandbox);
        let full_mode_decision = engine
            .evaluate(
                PermissionAction::FilesystemRead {
                    path: root.join(".secrets/token"),
                },
                &envelope("run:1"),
                PermissionRequestId::from("approval:full-mode"),
                2,
            )
            .expect("full mode decision");
        assert!(matches!(
            full_mode_decision,
            PermissionDecision::Deny { hard: true, .. }
        ));
        assert!(engine.pending_requests().is_empty());
    }

    #[test]
    fn custom_rules_are_specific_deny_first_and_never_bypass_sandbox() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let mut profile = workspace_write_profile(root.clone());
        profile.filesystem.denied_roots = vec![root.join(".secrets")];
        let mut engine = PermissionEngine::new(profile, ApprovalPolicy::Always);
        engine
            .replace_rules(vec![
                PermissionRule {
                    id: "rule:allow-src".to_owned(),
                    effect: PermissionRuleEffect::Allow,
                    action: PermissionRuleAction::Read,
                    pattern: "./src/**".to_owned(),
                },
                PermissionRule {
                    id: "rule:deny-private".to_owned(),
                    effect: PermissionRuleEffect::Deny,
                    action: PermissionRuleAction::Read,
                    pattern: "./src/private/**".to_owned(),
                },
            ])
            .expect("rules");
        assert!(matches!(
            engine
                .evaluate(
                    PermissionAction::FilesystemRead {
                        path: root.join("src/public.rs")
                    },
                    &envelope("run:rule"),
                    PermissionRequestId::from("approval:rule-allow"),
                    1
                )
                .expect("allow"),
            PermissionDecision::Allow { .. }
        ));
        assert!(matches!(
            engine
                .evaluate(
                    PermissionAction::FilesystemRead {
                        path: root.join("src/private/key.rs")
                    },
                    &envelope("run:rule"),
                    PermissionRequestId::from("approval:rule-deny"),
                    2
                )
                .expect("deny"),
            PermissionDecision::Deny { hard: true, .. }
        ));
        engine
            .add_rule(PermissionRule {
                id: "rule:allow-everything".to_owned(),
                effect: PermissionRuleEffect::Allow,
                action: PermissionRuleAction::Read,
                pattern: "**".to_owned(),
            })
            .expect("broad allow");
        engine
            .add_rule(PermissionRule {
                id: "rule:deny-rm".to_owned(),
                effect: PermissionRuleEffect::Deny,
                action: PermissionRuleAction::Execute,
                pattern: "rm -rf **".to_owned(),
            })
            .expect("execute deny");
        assert!(matches!(
            engine
                .evaluate(
                    PermissionAction::ProcessSpawn {
                        executable: root.join("bin/rm"),
                        arguments: vec!["-rf".to_owned(), "project".to_owned()],
                        cwd: root.clone()
                    },
                    &envelope("run:rule"),
                    PermissionRequestId::from("approval:deny-rm"),
                    3
                )
                .expect("deny rm"),
            PermissionDecision::Deny { hard: true, .. }
        ));
        assert!(matches!(
            engine
                .evaluate(
                    PermissionAction::FilesystemRead {
                        path: root.join(".secrets/token")
                    },
                    &envelope("run:rule"),
                    PermissionRequestId::from("approval:hard-deny"),
                    4
                )
                .expect("hard deny"),
            PermissionDecision::Deny { hard: true, .. }
        ));
    }

    #[test]
    fn permission_rule_file_round_trips_atomically() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("kernary.permissions.toml");
        let rules = vec![PermissionRule {
            id: "rule:config-write".to_owned(),
            effect: PermissionRuleEffect::Ask,
            action: PermissionRuleAction::Write,
            pattern: "./config/**".to_owned(),
        }];
        save_permission_rules_atomic(&path, &rules).expect("save");
        assert_eq!(load_permission_rules(&path).expect("load").rules, rules);
        let duplicate = vec![rules[0].clone(), rules[0].clone()];
        assert_eq!(
            save_permission_rules_atomic(&path, &duplicate)
                .expect_err("duplicate")
                .code,
            "permission-rule-duplicate"
        );
        assert_eq!(
            load_permission_rules(&path).expect("preserved").rules,
            rules
        );
    }

    #[test]
    fn once_run_and_project_grants_have_exact_scope() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let action = PermissionAction::FilesystemWrite {
            path: root.join("file.rs"),
        };
        let mut engine =
            PermissionEngine::new(workspace_write_profile(root), ApprovalPolicy::Always);
        let request = engine
            .evaluate(
                action.clone(),
                &envelope("run:1"),
                PermissionRequestId::from("approval:once"),
                1,
            )
            .expect("request");
        let PermissionDecision::RequestApproval(request) = request else {
            panic!("approval expected");
        };
        engine
            .respond(
                &request.id,
                true,
                GrantScope::Once,
                PermissionGrantId::from("grant:once"),
                2,
            )
            .expect("grant");
        assert!(matches!(
            engine
                .evaluate(
                    action.clone(),
                    &envelope("run:1"),
                    PermissionRequestId::from("approval:unused"),
                    3
                )
                .expect("allowed"),
            PermissionDecision::Allow {
                source: PermissionAllowSource::Grant,
                ..
            }
        ));
        assert!(matches!(
            engine
                .evaluate(
                    action,
                    &envelope("run:1"),
                    PermissionRequestId::from("approval:again"),
                    4
                )
                .expect("again"),
            PermissionDecision::RequestApproval(_)
        ));
    }

    #[test]
    fn mcp_requires_server_and_tool_pattern() {
        let temporary = tempdir().expect("tempdir");
        let mut profile = workspace_write_profile(temporary.path().to_path_buf());
        profile.mcp.allowed_server_ids = vec!["github".to_owned()];
        profile.mcp.allowed_tool_patterns = vec!["pulls.*".to_owned()];
        let mut engine = PermissionEngine::new(profile, ApprovalPolicy::NeverWithinSandbox);
        let allowed = engine
            .evaluate(
                PermissionAction::McpCall {
                    server_id: "github".to_owned(),
                    tool_name: "pulls.list".to_owned(),
                    side_effect: false,
                    arguments_sha256: "args-hash".to_owned(),
                },
                &envelope("run:1"),
                PermissionRequestId::from("approval:1"),
                1,
            )
            .expect("allowed");
        assert!(matches!(allowed, PermissionDecision::Allow { .. }));
    }

    #[test]
    fn run_grant_does_not_leak_and_project_grant_reuses() {
        let temporary = tempdir().expect("tempdir");
        let mut engine = PermissionEngine::new(
            workspace_write_profile(temporary.path().to_path_buf()),
            ApprovalPolicy::OnRequest,
        );
        let action = PermissionAction::NetworkConnect {
            host: "api.example.com".to_owned(),
        };
        let first = engine
            .evaluate(
                action.clone(),
                &envelope("run:1"),
                PermissionRequestId::from("approval:run"),
                1,
            )
            .expect("request");
        let PermissionDecision::RequestApproval(first) = first else {
            panic!("approval expected");
        };
        engine
            .respond(
                &first.id,
                true,
                GrantScope::Run,
                PermissionGrantId::from("grant:run"),
                2,
            )
            .expect("run grant");
        assert!(matches!(
            engine
                .evaluate(
                    action.clone(),
                    &envelope("run:1"),
                    PermissionRequestId::from("approval:unused"),
                    3
                )
                .expect("same run"),
            PermissionDecision::Allow { .. }
        ));
        let other = engine
            .evaluate(
                action.clone(),
                &envelope("run:2"),
                PermissionRequestId::from("approval:project"),
                4,
            )
            .expect("other run");
        let PermissionDecision::RequestApproval(other) = other else {
            panic!("other run must ask");
        };
        engine
            .respond(
                &other.id,
                true,
                GrantScope::Project,
                PermissionGrantId::from("grant:project"),
                5,
            )
            .expect("project grant");
        assert!(matches!(
            engine
                .evaluate(
                    action,
                    &envelope("run:3"),
                    PermissionRequestId::from("approval:unused-2"),
                    6
                )
                .expect("project reuse"),
            PermissionDecision::Allow { .. }
        ));
    }

    #[test]
    fn pending_request_rehydrates_with_same_id_even_if_policy_changes() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let action = PermissionAction::FilesystemRead {
            path: root.join("file.rs"),
        };
        let request_id = PermissionRequestId::from("approval:durable");
        let mut first = PermissionEngine::new(
            workspace_write_profile(root.clone()),
            ApprovalPolicy::Always,
        );
        let decision = first
            .evaluate(action.clone(), &envelope("run:1"), request_id.clone(), 1)
            .expect("request");
        assert!(matches!(decision, PermissionDecision::RequestApproval(_)));

        let mut recovered = PermissionEngine::new(
            workspace_write_profile(root),
            ApprovalPolicy::NeverWithinSandbox,
        );
        let restored = recovered
            .restore_pending_request(request_id.clone(), action, envelope("run:1"), 1)
            .expect("restore");
        assert_eq!(restored.id, request_id);
        assert_eq!(recovered.pending_requests().len(), 1);
    }

    #[test]
    fn agent_process_needs_approval_and_grant_is_exact_to_argv_and_cwd() {
        let temporary = tempdir().expect("tempdir");
        let executable = std::env::current_exe().expect("current exe");
        let mut profile = workspace_write_profile(temporary.path().to_path_buf());
        profile.subprocess.allowed_executables = vec![executable.clone()];
        let mut engine = PermissionEngine::new(profile, ApprovalPolicy::OnRequest);
        let action = PermissionAction::ProcessSpawn {
            executable: executable.clone(),
            arguments: vec!["--list".to_owned()],
            cwd: temporary.path().to_path_buf(),
        };
        let request = engine
            .evaluate(
                action.clone(),
                &envelope("run:1"),
                PermissionRequestId::from("approval:process"),
                1,
            )
            .expect("request");
        let PermissionDecision::RequestApproval(request) = request else {
            panic!("agent high-risk process must ask");
        };
        engine
            .respond(
                &request.id,
                true,
                GrantScope::Project,
                PermissionGrantId::from("grant:process"),
                2,
            )
            .expect("grant");
        assert!(matches!(
            engine
                .evaluate(
                    action,
                    &envelope("run:2"),
                    PermissionRequestId::from("approval:unused"),
                    3,
                )
                .expect("same argv"),
            PermissionDecision::Allow {
                source: PermissionAllowSource::Grant,
                ..
            }
        ));
        let changed = engine
            .evaluate(
                PermissionAction::ProcessSpawn {
                    executable,
                    arguments: vec!["--ignored".to_owned()],
                    cwd: temporary.path().to_path_buf(),
                },
                &envelope("run:2"),
                PermissionRequestId::from("approval:changed"),
                4,
            )
            .expect("changed argv");
        assert!(matches!(changed, PermissionDecision::RequestApproval(_)));
    }

    #[test]
    fn workspace_patch_requires_second_approval_even_for_user_and_grant_is_fingerprint_exact() {
        let temporary = tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let mut user = envelope("run:user");
        user.origin = InvocationOrigin::User;
        let mut engine = PermissionEngine::new(
            workspace_write_profile(root.clone()),
            ApprovalPolicy::OnRequest,
        );
        let action = PermissionAction::WorkspacePatchApply {
            operation: "apply".to_owned(),
            preview_id: "preview:1".to_owned(),
            preview_fingerprint: "a".repeat(64),
            paths: vec![root.join("a.rs"), root.join("b.rs")],
        };
        let decision = engine
            .evaluate(
                action.clone(),
                &user,
                PermissionRequestId::from("approval:workspace-patch"),
                1,
            )
            .expect("evaluate");
        let PermissionDecision::RequestApproval(request) = decision else {
            panic!("workspace patch must always ask")
        };
        engine
            .respond(
                &request.id,
                true,
                GrantScope::Project,
                PermissionGrantId::from("grant:workspace-patch"),
                2,
            )
            .expect("grant");
        assert!(matches!(
            engine
                .evaluate(
                    action,
                    &user,
                    PermissionRequestId::from("approval:unused"),
                    3,
                )
                .expect("reuse"),
            PermissionDecision::Allow {
                source: PermissionAllowSource::Grant,
                ..
            }
        ));
        let changed = engine
            .evaluate(
                PermissionAction::WorkspacePatchApply {
                    operation: "apply".to_owned(),
                    preview_id: "preview:1".to_owned(),
                    preview_fingerprint: "b".repeat(64),
                    paths: vec![root.join("a.rs"), root.join("b.rs")],
                },
                &user,
                PermissionRequestId::from("approval:changed"),
                4,
            )
            .expect("changed");
        assert!(matches!(changed, PermissionDecision::RequestApproval(_)));
    }

    #[test]
    fn extension_grant_is_exact_to_argument_hash() {
        let temporary = tempdir().expect("tempdir");
        let mut profile = workspace_write_profile(temporary.path().to_path_buf());
        profile.plugin.allowed_plugin_ids = vec!["demo".to_owned()];
        profile.plugin.allowed_capability_patterns = vec!["lookup".to_owned()];
        let mut engine = PermissionEngine::new(profile, ApprovalPolicy::Always);
        let action = PermissionAction::PluginCall {
            plugin_id: "demo".to_owned(),
            capability: "lookup".to_owned(),
            side_effect: false,
            arguments_sha256: "hash-a".to_owned(),
        };
        let request = engine
            .evaluate(
                action.clone(),
                &envelope("run:1"),
                PermissionRequestId::from("approval:extension"),
                1,
            )
            .expect("request");
        let PermissionDecision::RequestApproval(request) = request else {
            panic!("approval expected");
        };
        engine
            .respond(
                &request.id,
                true,
                GrantScope::Project,
                PermissionGrantId::from("grant:extension"),
                2,
            )
            .expect("grant");
        assert!(matches!(
            engine
                .evaluate(
                    action,
                    &envelope("run:2"),
                    PermissionRequestId::from("approval:unused"),
                    3,
                )
                .expect("same args"),
            PermissionDecision::Allow { .. }
        ));
        let changed = engine
            .evaluate(
                PermissionAction::PluginCall {
                    plugin_id: "demo".to_owned(),
                    capability: "lookup".to_owned(),
                    side_effect: false,
                    arguments_sha256: "hash-b".to_owned(),
                },
                &envelope("run:2"),
                PermissionRequestId::from("approval:changed-extension"),
                4,
            )
            .expect("changed");
        assert!(matches!(changed, PermissionDecision::RequestApproval(_)));
    }
}
