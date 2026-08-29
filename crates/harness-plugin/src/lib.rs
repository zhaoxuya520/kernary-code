#![forbid(unsafe_code)]

//! Manifest-first Plugin Runtime。插件代码只在隔离子进程中运行，metadata discovery 不执行代码。

mod process;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use harness_permission::PermissionAction;
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process::{resolve_inside, run_plugin_process};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

impl PluginError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for PluginError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for PluginError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginLifecycleStatus {
    Installed,
    Disabled,
    Activating,
    Active,
    Draining,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginDependency {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginToolManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub effect_class: ToolEffectClass,
    #[serde(default = "side_effect_by_default")]
    pub side_effect: bool,
    pub input_schema: PathBuf,
    pub output_schema: Option<PathBuf>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

const fn side_effect_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginContributions {
    #[serde(default)]
    pub tools: Vec<PluginToolManifest>,
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    #[serde(default)]
    pub mcp_servers: Vec<PathBuf>,
    #[serde(default)]
    pub context_providers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub engine_range: String,
    pub entry: PathBuf,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default)]
    pub contributions: PluginContributions,
    pub activation_timeout_millis: Option<u64>,
    pub tool_timeout_millis: Option<u64>,
    pub max_output_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginPermissionReview {
    pub plugin_id: String,
    pub manifest_hash: String,
    pub entry_sha256: String,
    pub contribution_sha256: BTreeMap<PathBuf, String>,
    pub permissions: Vec<String>,
    pub tool_names: Vec<String>,
    pub skill_paths: Vec<PathBuf>,
    pub mcp_server_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub status: PluginLifecycleStatus,
    pub scope: Option<String>,
    pub permissions: Vec<String>,
    pub contribution_count: usize,
    pub review_hash: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDiscoveryError {
    pub manifest_path: PathBuf,
    pub error: PluginError,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginDiscoveryReport {
    pub plugins: Vec<PluginView>,
    pub errors: Vec<PluginDiscoveryError>,
}

struct InstalledPlugin {
    root: PathBuf,
    manifest_path: PathBuf,
    manifest: PluginManifest,
    review: Option<PluginPermissionReview>,
    status: PluginLifecycleStatus,
    scope: Option<String>,
    registered_tools: Vec<String>,
    last_error: Option<String>,
}

pub struct PluginManager {
    engine_version: Version,
    tools: ToolRegistry,
    plugins: Mutex<BTreeMap<String, Arc<Mutex<InstalledPlugin>>>>,
}

impl PluginManager {
    pub fn new(engine_version: &str, tools: ToolRegistry) -> Result<Arc<Self>, PluginError> {
        let engine_version = Version::parse(engine_version).map_err(|error| {
            PluginError::new("plugin-engine-version-invalid", error.to_string())
        })?;
        Ok(Arc::new(Self {
            engine_version,
            tools,
            plugins: Mutex::new(BTreeMap::new()),
        }))
    }

    /// 扫描 `plugin.toml` 并验证 metadata/path/hash；绝不执行 entry。
    pub fn discover(&self, roots: &[PathBuf]) -> Result<Vec<PluginView>, PluginError> {
        let report = self.discover_isolated(roots)?;
        if let Some(error) = report.errors.into_iter().next() {
            return Err(error.error);
        }
        Ok(report.plugins)
    }

    pub fn discover_isolated(
        &self,
        roots: &[PathBuf],
    ) -> Result<PluginDiscoveryReport, PluginError> {
        let mut manifests = Vec::new();
        for root in roots {
            if !root.exists() {
                continue;
            }
            let root = fs::canonicalize(root)
                .map_err(|error| PluginError::new("plugin-discovery-root", error.to_string()))?;
            for entry in fs::read_dir(&root)
                .map_err(|error| PluginError::new("plugin-discovery-read", error.to_string()))?
            {
                let entry = entry.map_err(|error| {
                    PluginError::new("plugin-discovery-read", error.to_string())
                })?;
                if !entry
                    .file_type()
                    .map_err(|error| PluginError::new("plugin-discovery-type", error.to_string()))?
                    .is_dir()
                {
                    continue;
                }
                let manifest = entry.path().join("plugin.toml");
                if manifest.is_file() {
                    manifests.push(manifest);
                }
            }
        }
        manifests.sort();
        let mut report = PluginDiscoveryReport::default();
        for manifest in manifests {
            match self.install_manifest(&manifest) {
                Ok(plugin) => report.plugins.push(plugin),
                Err(error) => report.errors.push(PluginDiscoveryError {
                    manifest_path: manifest,
                    error,
                }),
            }
        }
        Ok(report)
    }

    pub fn install_manifest(&self, manifest_path: &Path) -> Result<PluginView, PluginError> {
        let manifest_path = fs::canonicalize(manifest_path)
            .map_err(|error| PluginError::new("plugin-manifest-path", error.to_string()))?;
        let root = manifest_path
            .parent()
            .ok_or_else(|| {
                PluginError::new("plugin-root-missing", manifest_path.display().to_string())
            })?
            .to_path_buf();
        let bytes = fs::read(&manifest_path)
            .map_err(|error| PluginError::new("plugin-manifest-read", error.to_string()))?;
        if bytes.len() > 1024 * 1024 {
            return Err(PluginError::new(
                "plugin-manifest-too-large",
                bytes.len().to_string(),
            ));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            PluginError::new(
                "plugin-manifest-not-utf8",
                manifest_path.display().to_string(),
            )
        })?;
        let manifest: PluginManifest = toml::from_str(text)
            .map_err(|error| PluginError::new("plugin-manifest-toml", error.to_string()))?;
        validate_manifest(&manifest, &self.engine_version)?;
        resolve_inside(&root, &manifest.entry)?;
        for tool in &manifest.contributions.tools {
            resolve_inside(&root, &tool.input_schema)?;
            if let Some(output_schema) = &tool.output_schema {
                resolve_inside(&root, output_schema)?;
            }
        }
        for path in manifest
            .contributions
            .skills
            .iter()
            .chain(&manifest.contributions.mcp_servers)
        {
            resolve_inside(&root, path)?;
        }
        let plugin = Arc::new(Mutex::new(InstalledPlugin {
            root,
            manifest_path,
            manifest,
            review: None,
            status: PluginLifecycleStatus::Disabled,
            scope: None,
            registered_tools: vec![],
            last_error: None,
        }));
        let view = plugin_view(&plugin)?;
        let mut plugins = self
            .plugins
            .lock()
            .map_err(|_| PluginError::new("plugin-registry-poisoned", "plugins"))?;
        if plugins.contains_key(&view.id) {
            return Err(PluginError::new("plugin-already-installed", view.id));
        }
        plugins.insert(view.id.clone(), plugin);
        Ok(view)
    }

    pub fn review(&self, plugin_id: &str) -> Result<PluginPermissionReview, PluginError> {
        let plugin = self.plugin(plugin_id)?;
        let mut state = plugin
            .lock()
            .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
        let manifest_bytes = fs::read(&state.manifest_path)
            .map_err(|error| PluginError::new("plugin-manifest-read", error.to_string()))?;
        let entry = resolve_inside(&state.root, &state.manifest.entry)?;
        let review = build_review(&state.manifest, &state.root, &entry, &manifest_bytes)?;
        state.review = Some(review.clone());
        Ok(review)
    }

    pub fn enable(
        self: &Arc<Self>,
        plugin_id: &str,
        scope: &str,
        approved_review_hash: &str,
        settings: serde_json::Value,
    ) -> Result<PluginView, PluginError> {
        let plugin = self.plugin(plugin_id)?;
        let (root, manifest, entry, review_hash) = {
            let mut state = plugin
                .lock()
                .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
            if state.status == PluginLifecycleStatus::Active {
                drop(state);
                return plugin_view(&plugin);
            }
            if matches!(
                state.status,
                PluginLifecycleStatus::Activating | PluginLifecycleStatus::Draining
            ) {
                return Err(PluginError::new("plugin-busy", plugin_id));
            }
            let current_manifest = fs::read(&state.manifest_path)
                .map_err(|error| PluginError::new("plugin-manifest-read", error.to_string()))?;
            let current_entry = resolve_inside(&state.root, &state.manifest.entry)?;
            let current_review = build_review(
                &state.manifest,
                &state.root,
                &current_entry,
                &current_manifest,
            )?;
            if current_review.manifest_hash != approved_review_hash {
                return Err(PluginError::new("plugin-review-hash-mismatch", plugin_id));
            }
            if state
                .review
                .as_ref()
                .is_some_and(|review| review != &current_review)
            {
                return Err(PluginError::new(
                    "plugin-review-snapshot-changed",
                    plugin_id,
                ));
            }
            state.review = Some(current_review.clone());
            self.validate_dependencies(&state.manifest)?;
            state.status = PluginLifecycleStatus::Activating;
            state.scope = Some(scope.to_owned());
            state.last_error = None;
            (
                state.root.clone(),
                state.manifest.clone(),
                resolve_inside(&state.root, &state.manifest.entry)?,
                current_review.manifest_hash,
            )
        };
        let activation = run_plugin_process(
            &root,
            &entry,
            "activate",
            &serde_json::json!({
                "pluginId":plugin_id,
                "scope":scope,
                "reviewHash":review_hash,
                "settings":settings
            }),
            activation_timeout(&manifest),
            output_limit(&manifest),
            None,
        );
        if let Err(error) = activation.and_then(expect_ok_response) {
            let mut state = plugin
                .lock()
                .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
            state.status = PluginLifecycleStatus::Failed;
            state.last_error = Some(sanitize_error(&error));
            drop(state);
            return plugin_view(&plugin);
        }

        let mut registered = Vec::new();
        for tool in &manifest.contributions.tools {
            let descriptor = match load_tool_descriptor(plugin_id, &root, tool) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.rollback_tools(plugin_id, &registered);
                    let _ = run_plugin_process(
                        &root,
                        &entry,
                        "deactivate",
                        &serde_json::json!({"pluginId":plugin_id,"scope":scope}),
                        activation_timeout(&manifest),
                        output_limit(&manifest),
                        None,
                    );
                    let mut state = plugin
                        .lock()
                        .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
                    state.status = PluginLifecycleStatus::Failed;
                    state.last_error = Some(sanitize_error(&error));
                    drop(state);
                    return plugin_view(&plugin);
                }
            };
            let canonical_name = descriptor.canonical_name.clone();
            let provider = Arc::new(PluginToolProvider {
                manager: Arc::downgrade(self),
                plugin_id: plugin_id.to_owned(),
                tool_name: tool.name.clone(),
                side_effect: tool.side_effect,
            });
            if let Err(error) = self.tools.register(descriptor, provider) {
                self.rollback_tools(plugin_id, &registered);
                let _ = run_plugin_process(
                    &root,
                    &entry,
                    "deactivate",
                    &serde_json::json!({"pluginId":plugin_id,"scope":scope}),
                    activation_timeout(&manifest),
                    output_limit(&manifest),
                    None,
                );
                let error = tool_to_plugin_error(error);
                let mut state = plugin
                    .lock()
                    .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
                state.status = PluginLifecycleStatus::Failed;
                state.last_error = Some(sanitize_error(&error));
                drop(state);
                return plugin_view(&plugin);
            }
            registered.push(canonical_name);
        }
        let mut state = plugin
            .lock()
            .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
        state.registered_tools = registered;
        state.status = PluginLifecycleStatus::Active;
        drop(state);
        plugin_view(&plugin)
    }

    pub fn disable(&self, plugin_id: &str) -> Result<PluginView, PluginError> {
        let plugin = self.plugin(plugin_id)?;
        let (root, manifest, entry, scope, registered) = {
            let mut state = plugin
                .lock()
                .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
            if state.status == PluginLifecycleStatus::Disabled {
                drop(state);
                return plugin_view(&plugin);
            }
            state.status = PluginLifecycleStatus::Draining;
            (
                state.root.clone(),
                state.manifest.clone(),
                resolve_inside(&state.root, &state.manifest.entry)?,
                state.scope.clone(),
                std::mem::take(&mut state.registered_tools),
            )
        };
        self.rollback_tools(plugin_id, &registered);
        let deactivation = run_plugin_process(
            &root,
            &entry,
            "deactivate",
            &serde_json::json!({"pluginId":plugin_id,"scope":scope}),
            activation_timeout(&manifest),
            output_limit(&manifest),
            None,
        )
        .and_then(expect_ok_response);
        let mut state = plugin
            .lock()
            .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
        state.scope = None;
        match deactivation {
            Ok(()) => {
                state.status = PluginLifecycleStatus::Disabled;
                state.last_error = None;
            }
            Err(error) => {
                state.status = PluginLifecycleStatus::Failed;
                state.last_error = Some(sanitize_error(&error));
            }
        }
        drop(state);
        plugin_view(&plugin)
    }

    pub fn list(&self) -> Result<Vec<PluginView>, PluginError> {
        self.plugins
            .lock()
            .map_err(|_| PluginError::new("plugin-registry-poisoned", "plugins"))?
            .values()
            .map(plugin_view)
            .collect()
    }

    pub fn contribution_paths(
        &self,
        plugin_id: &str,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>), PluginError> {
        let plugin = self.plugin(plugin_id)?;
        let state = plugin
            .lock()
            .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
        let skills = state
            .manifest
            .contributions
            .skills
            .iter()
            .map(|path| resolve_inside(&state.root, path))
            .collect::<Result<Vec<_>, _>>()?;
        let mcp = state
            .manifest
            .contributions
            .mcp_servers
            .iter()
            .map(|path| resolve_inside(&state.root, path))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((skills, mcp))
    }

    fn call_tool(
        &self,
        plugin_id: &str,
        tool_name: &str,
        args: serde_json::Value,
        cancellation: &harness_tool::ToolCancellationToken,
    ) -> Result<serde_json::Value, PluginError> {
        let plugin = self.plugin(plugin_id)?;
        let (root, entry, manifest) = {
            let state = plugin
                .lock()
                .map_err(|_| PluginError::new("plugin-state-poisoned", plugin_id))?;
            if state.status != PluginLifecycleStatus::Active {
                return Err(PluginError::new("plugin-not-active", plugin_id));
            }
            if !state
                .manifest
                .contributions
                .tools
                .iter()
                .any(|tool| tool.name == tool_name)
            {
                return Err(PluginError::new("plugin-tool-not-found", tool_name));
            }
            (
                state.root.clone(),
                resolve_inside(&state.root, &state.manifest.entry)?,
                state.manifest.clone(),
            )
        };
        let response = run_plugin_process(
            &root,
            &entry,
            "tool-call",
            &serde_json::json!({
                "pluginId":plugin_id,
                "tool":tool_name,
                "arguments":args
            }),
            tool_timeout(&manifest),
            output_limit(&manifest),
            Some(cancellation),
        )?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| PluginError::new("plugin-tool-result-missing", tool_name))
    }

    fn validate_dependencies(&self, manifest: &PluginManifest) -> Result<(), PluginError> {
        for dependency in &manifest.dependencies {
            let plugin = self.plugin(&dependency.id)?;
            let state = plugin
                .lock()
                .map_err(|_| PluginError::new("plugin-state-poisoned", &dependency.id))?;
            if state.status != PluginLifecycleStatus::Active {
                return Err(PluginError::new(
                    "plugin-dependency-not-active",
                    &dependency.id,
                ));
            }
            let required = VersionReq::parse(&dependency.version).map_err(|error| {
                PluginError::new("plugin-dependency-version-invalid", error.to_string())
            })?;
            let actual = Version::parse(&state.manifest.version)
                .map_err(|error| PluginError::new("plugin-version-invalid", error.to_string()))?;
            if !required.matches(&actual) {
                return Err(PluginError::new(
                    "plugin-dependency-version-mismatch",
                    format!("{} {}", dependency.id, dependency.version),
                ));
            }
        }
        Ok(())
    }

    fn rollback_tools(&self, plugin_id: &str, tools: &[String]) {
        let source = ToolSource::Plugin {
            plugin_id: plugin_id.to_owned(),
        };
        for tool in tools.iter().rev() {
            let _ = self.tools.unregister(tool, &source);
        }
    }

    fn plugin(&self, plugin_id: &str) -> Result<Arc<Mutex<InstalledPlugin>>, PluginError> {
        self.plugins
            .lock()
            .map_err(|_| PluginError::new("plugin-registry-poisoned", "plugins"))?
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginError::new("plugin-not-installed", plugin_id))
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        let plugins = self
            .plugins
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for plugin in plugins.values() {
            let mut state = plugin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = ToolSource::Plugin {
                plugin_id: state.manifest.id.clone(),
            };
            for tool in state.registered_tools.drain(..).rev() {
                let _ = self.tools.unregister(&tool, &source);
            }
        }
    }
}

struct PluginToolProvider {
    manager: Weak<PluginManager>,
    plugin_id: String,
    tool_name: String,
    side_effect: bool,
}

impl ToolProvider for PluginToolProvider {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if !value.is_object() {
            return Err(ToolError::new(
                "invalid-plugin-tool-args",
                self.tool_name.clone(),
            ));
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        Ok(PermissionAction::PluginCall {
            plugin_id: self.plugin_id.clone(),
            capability: self.tool_name.clone(),
            side_effect: self.side_effect,
            arguments_sha256: json_sha256(args)?,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        self.manager
            .upgrade()
            .ok_or_else(|| ToolError::new("plugin-manager-dropped", &self.plugin_id))?
            .call_tool(
                &self.plugin_id,
                &self.tool_name,
                input.args,
                &input.cancellation,
            )
            .map_err(plugin_to_tool_error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceCardinality {
    One,
    Many,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDefinition {
    pub id: String,
    pub version: String,
    pub cardinality: ServiceCardinality,
    pub core_owned: bool,
}

#[derive(Clone, Debug)]
struct ServiceProviderRecord {
    plugin_id: String,
    scope: String,
    value: serde_json::Value,
}

#[derive(Default)]
pub struct ServiceRegistry {
    definitions: BTreeMap<String, ServiceDefinition>,
    providers: BTreeMap<String, Vec<ServiceProviderRecord>>,
}

impl ServiceRegistry {
    pub fn define(&mut self, definition: ServiceDefinition) -> Result<(), PluginError> {
        if let Some(existing) = self.definitions.get(&definition.id)
            && existing.version != definition.version
        {
            return Err(PluginError::new(
                "service-definition-version-conflict",
                definition.id,
            ));
        }
        self.definitions.insert(definition.id.clone(), definition);
        Ok(())
    }

    pub fn provide(
        &mut self,
        plugin_id: &str,
        scope: &str,
        service_id: &str,
        value: serde_json::Value,
    ) -> Result<(), PluginError> {
        let definition = self
            .definitions
            .get(service_id)
            .ok_or_else(|| PluginError::new("service-definition-not-registered", service_id))?;
        if definition.core_owned {
            return Err(PluginError::new(
                "core-service-cannot-be-provided-by-plugin",
                service_id,
            ));
        }
        let providers = self.providers.entry(service_id.to_owned()).or_default();
        if definition.cardinality == ServiceCardinality::One
            && providers.iter().any(|provider| provider.scope == scope)
        {
            return Err(PluginError::new(
                "single-service-provider-conflict",
                format!("{service_id}@{scope}"),
            ));
        }
        providers.push(ServiceProviderRecord {
            plugin_id: plugin_id.to_owned(),
            scope: scope.to_owned(),
            value,
        });
        Ok(())
    }

    pub fn remove_plugin(&mut self, plugin_id: &str) {
        for providers in self.providers.values_mut() {
            providers.retain(|provider| provider.plugin_id != plugin_id);
        }
    }

    #[must_use]
    pub fn consume(&self, service_id: &str, scope: &str) -> Vec<serde_json::Value> {
        self.providers
            .get(service_id)
            .into_iter()
            .flatten()
            .filter(|provider| provider.scope == scope)
            .map(|provider| provider.value.clone())
            .collect()
    }
}

pub fn compose_plugin_settings(layers: &[serde_json::Value]) -> serde_json::Value {
    layers
        .iter()
        .cloned()
        .fold(serde_json::json!({}), merge_json)
}

fn merge_json(left: serde_json::Value, right: serde_json::Value) -> serde_json::Value {
    match (left, right) {
        (serde_json::Value::Object(mut left), serde_json::Value::Object(right)) => {
            for (key, value) in right {
                let merged = left
                    .remove(&key)
                    .map_or(value.clone(), |left| merge_json(left, value));
                left.insert(key, merged);
            }
            serde_json::Value::Object(left)
        }
        (_, right) => right,
    }
}

fn validate_manifest(manifest: &PluginManifest, engine: &Version) -> Result<(), PluginError> {
    validate_id(&manifest.id, "plugin-id-invalid")?;
    if manifest.name.trim().is_empty()
        || manifest.description.trim().is_empty()
        || manifest.entry.as_os_str().is_empty()
    {
        return Err(PluginError::new(
            "plugin-manifest-field-invalid",
            &manifest.id,
        ));
    }
    Version::parse(&manifest.version)
        .map_err(|error| PluginError::new("plugin-version-invalid", error.to_string()))?;
    let engine_range = VersionReq::parse(&manifest.engine_range)
        .map_err(|error| PluginError::new("plugin-engine-range-invalid", error.to_string()))?;
    if !engine_range.matches(engine) {
        return Err(PluginError::new(
            "plugin-engine-incompatible",
            format!("{} requires {}", manifest.id, manifest.engine_range),
        ));
    }
    for (name, value, max) in [
        (
            "activationTimeoutMillis",
            manifest.activation_timeout_millis.unwrap_or(5_000),
            300_000_u64,
        ),
        (
            "toolTimeoutMillis",
            manifest.tool_timeout_millis.unwrap_or(30_000),
            300_000_u64,
        ),
    ] {
        if value == 0 || value > max {
            return Err(PluginError::new(
                "plugin-timeout-invalid",
                format!("{name}={value}"),
            ));
        }
    }
    let output_limit = manifest.max_output_bytes.unwrap_or(4 * 1024 * 1024);
    if !(1024..=32 * 1024 * 1024).contains(&output_limit) {
        return Err(PluginError::new(
            "plugin-output-limit-invalid",
            output_limit.to_string(),
        ));
    }
    let mut names = BTreeMap::new();
    for tool in &manifest.contributions.tools {
        validate_id(&tool.name, "plugin-tool-name-invalid")?;
        if names.insert(tool.name.clone(), ()).is_some() {
            return Err(PluginError::new("plugin-tool-duplicate", &tool.name));
        }
        if tool.version.trim().is_empty() || tool.description.trim().is_empty() {
            return Err(PluginError::new("plugin-tool-field-invalid", &tool.name));
        }
    }
    Ok(())
}

fn load_tool_descriptor(
    plugin_id: &str,
    root: &Path,
    tool: &PluginToolManifest,
) -> Result<ToolDescriptor, PluginError> {
    let input_schema = read_json_file(&resolve_inside(root, &tool.input_schema)?, 1024 * 1024)?;
    if !input_schema.is_object() {
        return Err(PluginError::new("plugin-input-schema-invalid", &tool.name));
    }
    let output_schema = tool
        .output_schema
        .as_ref()
        .map_or(Ok(serde_json::json!({})), |path| {
            read_json_file(&resolve_inside(root, path)?, 1024 * 1024)
        })?;
    if !output_schema.is_object() {
        return Err(PluginError::new("plugin-output-schema-invalid", &tool.name));
    }
    Ok(ToolDescriptor {
        canonical_name: format!("plugin.{plugin_id}.{}", tool.name),
        version: tool.version.clone(),
        description: tool.description.clone(),
        effect_class: tool.effect_class,
        source: ToolSource::Plugin {
            plugin_id: plugin_id.to_owned(),
        },
        prompt_loading: ToolPromptLoading::OnDemand,
        keywords: tool.keywords.clone(),
        input_schema,
        output_schema,
    })
}

fn build_review(
    manifest: &PluginManifest,
    root: &Path,
    entry: &Path,
    manifest_bytes: &[u8],
) -> Result<PluginPermissionReview, PluginError> {
    let entry_sha256 = hash_file(entry, 128 * 1024 * 1024)?;
    let mut contribution_sha256 = BTreeMap::new();
    for path in manifest
        .contributions
        .tools
        .iter()
        .flat_map(|tool| std::iter::once(&tool.input_schema).chain(tool.output_schema.as_ref()))
        .chain(&manifest.contributions.skills)
        .chain(&manifest.contributions.mcp_servers)
    {
        contribution_sha256.insert(
            path.clone(),
            hash_file(&resolve_inside(root, path)?, 16 * 1024 * 1024)?,
        );
    }
    let mut hasher = Sha256::new();
    hasher.update(manifest_bytes);
    hasher.update(entry_sha256.as_bytes());
    for (path, hash) in &contribution_sha256 {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(hash.as_bytes());
    }
    let manifest_hash = format!("{:x}", hasher.finalize());
    Ok(PluginPermissionReview {
        plugin_id: manifest.id.clone(),
        manifest_hash,
        entry_sha256,
        contribution_sha256,
        permissions: manifest.permissions.clone(),
        tool_names: manifest
            .contributions
            .tools
            .iter()
            .map(|tool| format!("plugin.{}.{}", manifest.id, tool.name))
            .collect(),
        skill_paths: manifest.contributions.skills.clone(),
        mcp_server_paths: manifest.contributions.mcp_servers.clone(),
    })
}

fn hash_file(path: &Path, limit: u64) -> Result<String, PluginError> {
    let metadata = fs::metadata(path)
        .map_err(|error| PluginError::new("plugin-entry-metadata", error.to_string()))?;
    if metadata.len() > limit {
        return Err(PluginError::new(
            "plugin-entry-too-large",
            metadata.len().to_string(),
        ));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| PluginError::new("plugin-entry-read", error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| PluginError::new("plugin-entry-read", error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_json_file(path: &Path, limit: usize) -> Result<serde_json::Value, PluginError> {
    let bytes =
        fs::read(path).map_err(|error| PluginError::new("plugin-json-read", error.to_string()))?;
    if bytes.len() > limit {
        return Err(PluginError::new(
            "plugin-json-too-large",
            bytes.len().to_string(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| PluginError::new("plugin-json-invalid", error.to_string()))
}

fn plugin_view(plugin: &Arc<Mutex<InstalledPlugin>>) -> Result<PluginView, PluginError> {
    let state = plugin
        .lock()
        .map_err(|_| PluginError::new("plugin-state-poisoned", "view"))?;
    Ok(PluginView {
        id: state.manifest.id.clone(),
        name: state.manifest.name.clone(),
        version: state.manifest.version.clone(),
        description: state.manifest.description.clone(),
        status: state.status,
        scope: state.scope.clone(),
        permissions: state.manifest.permissions.clone(),
        contribution_count: state.registered_tools.len(),
        review_hash: state
            .review
            .as_ref()
            .map(|review| review.manifest_hash.clone()),
        last_error: state.last_error.clone(),
    })
}

fn expect_ok_response(value: serde_json::Value) -> Result<(), PluginError> {
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(PluginError::new(
            "plugin-response-not-ok",
            value.to_string(),
        ))
    }
}

fn activation_timeout(manifest: &PluginManifest) -> Duration {
    Duration::from_millis(manifest.activation_timeout_millis.unwrap_or(5_000))
}

fn tool_timeout(manifest: &PluginManifest) -> Duration {
    Duration::from_millis(manifest.tool_timeout_millis.unwrap_or(30_000))
}

fn output_limit(manifest: &PluginManifest) -> usize {
    manifest.max_output_bytes.unwrap_or(4 * 1024 * 1024)
}

fn validate_id(value: &str, code: &'static str) -> Result<(), PluginError> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    {
        return Err(PluginError::new(code, value));
    }
    Ok(())
}

fn sanitize_error(error: &PluginError) -> String {
    format!(
        "{}: {}",
        error.code,
        error.message.chars().take(512).collect::<String>()
    )
}

fn tool_to_plugin_error(error: ToolError) -> PluginError {
    PluginError::new(error.code, error.message)
}

fn plugin_to_tool_error(error: PluginError) -> ToolError {
    ToolError::new(error.code, error.message)
}

fn json_sha256(value: &serde_json::Value) -> Result<String, ToolError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ToolError::new("plugin-tool-args-json", error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
