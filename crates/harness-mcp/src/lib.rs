#![forbid(unsafe_code)]

//! MCP metadata discovery、lazy connection、catalog 与统一 Tool Runtime bridge。

mod http;
mod legacy;
mod oauth;
mod protocol;
mod stdio;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harness_auth::CredentialStore;
use harness_http::StreamingHttpTransport;
use harness_permission::PermissionAction;
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

pub use http::{McpStreamableHttpConfig, StreamableHttpMcpTransport};
pub use oauth::{McpOAuthConfig, McpOAuthStart, McpOAuthStatus};
pub use protocol::{
    LATEST_STABLE_PROTOCOL_VERSION, McpCallToolResult, McpClient, McpError, McpPromptDescriptor,
    McpResourceDescriptor, McpServerInfo, McpTaskSupport, McpToolAnnotations, McpToolDescriptor,
    McpTransport,
};
pub use stdio::{McpStdioConfig, StdioMcpTransport};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum McpTransportConfig {
    Stdio(McpStdioConfig),
    StreamableHttp(McpStreamableHttpConfig),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub trust_annotations: bool,
    pub transport: McpTransportConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfigFile {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

pub fn load_config_file(path: &Path) -> Result<McpConfigFile, McpError> {
    let bytes =
        fs::read(path).map_err(|error| McpError::new("mcp-config-read", error.to_string()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(McpError::new(
            "mcp-config-too-large",
            bytes.len().to_string(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| McpError::new("mcp-config-not-utf8", path.display().to_string()))?;
    toml::from_str(text).map_err(|error| McpError::new("mcp-config-toml", error.to_string()))
}

/// 以同目录 backup/swap 方式保存 MCP 配置；拒绝 symlink 和非普通目标。
pub fn save_config_file_atomic(path: &Path, config: &McpConfigFile) -> Result<(), McpError> {
    let mut ids = std::collections::BTreeSet::new();
    for server in &config.servers {
        validate_id(&server.id, "mcp-server-id")?;
        if server.name.trim().is_empty() || !ids.insert(server.id.clone()) {
            return Err(McpError::new("mcp-config-server-invalid", &server.id));
        }
        validate_transport_config(&server.transport)?;
    }
    let parent = path
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| McpError::new("mcp-config-parent-invalid", path.display().to_string()))?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.file_type().is_file())
    {
        return Err(McpError::new(
            "mcp-config-target-invalid",
            path.display().to_string(),
        ));
    }
    let mut normalized = config.clone();
    normalized
        .servers
        .sort_by(|left, right| left.id.cmp(&right.id));
    let bytes = toml::to_string_pretty(&normalized)
        .map_err(|error| McpError::new("mcp-config-serialize", error.to_string()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(McpError::new(
            "mcp-config-too-large",
            bytes.len().to_string(),
        ));
    }
    let suffix = format!("{}-{}", std::process::id(), now_millis());
    let temporary = parent.join(format!(".kernary-mcp-new-{suffix}.toml"));
    let backup = parent.join(format!(".kernary-mcp-backup-{suffix}.toml"));
    let write_result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes.as_bytes())
            .and_then(|()| file.sync_all())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(McpError::new("mcp-config-write", error.to_string()));
    }
    if !path.exists() {
        return fs::rename(&temporary, path)
            .map_err(|error| McpError::new("mcp-config-swap", error.to_string()));
    }
    fs::rename(path, &backup)
        .map_err(|error| McpError::new("mcp-config-backup", error.to_string()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::rename(&backup, path);
        let _ = fs::remove_file(&temporary);
        return Err(McpError::new("mcp-config-swap", error.to_string()));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

const fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpConnectionStatus {
    Disconnected,
    Connecting,
    Ready,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: String,
    pub authorization: String,
    pub status: McpConnectionStatus,
    pub protocol_version: Option<String>,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub tool_count: usize,
    pub supported_tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
    pub retry_after_millis: Option<i64>,
    pub last_error: Option<String>,
}

struct ManagedServerState {
    status: McpConnectionStatus,
    client: Option<McpClient>,
    tools: Vec<McpToolDescriptor>,
    resources: Vec<McpResourceDescriptor>,
    prompts: Vec<McpPromptDescriptor>,
    registered_tools: Vec<String>,
    failure_count: u32,
    retry_after_millis: Option<i64>,
    last_error: Option<String>,
}

struct ManagedServer {
    config: McpServerConfig,
    enabled: AtomicBool,
    state: Mutex<ManagedServerState>,
}

pub struct McpManager {
    project_root: PathBuf,
    credentials: Arc<dyn CredentialStore>,
    http_transport: Arc<dyn StreamingHttpTransport>,
    tools: ToolRegistry,
    servers: Mutex<BTreeMap<String, Arc<ManagedServer>>>,
    oauth: oauth::McpOAuthCoordinator,
}

impl McpManager {
    pub fn new(
        project_root: impl AsRef<Path>,
        credentials: Arc<dyn CredentialStore>,
        http_transport: Arc<dyn StreamingHttpTransport>,
        tools: ToolRegistry,
    ) -> Result<Arc<Self>, McpError> {
        let project_root = fs::canonicalize(project_root)
            .map_err(|error| McpError::new("mcp-project-root", error.to_string()))?;
        Ok(Arc::new(Self {
            project_root,
            credentials: credentials.clone(),
            http_transport: http_transport.clone(),
            tools,
            servers: Mutex::new(BTreeMap::new()),
            oauth: oauth::McpOAuthCoordinator::new(credentials, http_transport),
        }))
    }

    /// 只注册 metadata；不会启动进程、读取 credential 或发起网络。
    pub fn add_server(&self, config: McpServerConfig) -> Result<McpServerView, McpError> {
        validate_id(&config.id, "mcp-server-id")?;
        if config.name.trim().is_empty() {
            return Err(McpError::new("mcp-server-name-invalid", &config.id));
        }
        validate_transport_config(&config.transport)?;
        let server = Arc::new(ManagedServer {
            enabled: AtomicBool::new(config.enabled),
            config,
            state: Mutex::new(ManagedServerState {
                status: McpConnectionStatus::Disconnected,
                client: None,
                tools: vec![],
                resources: vec![],
                prompts: vec![],
                registered_tools: vec![],
                failure_count: 0,
                retry_after_millis: None,
                last_error: None,
            }),
        });
        let view = server_view(&server)?;
        let mut servers = self
            .servers
            .lock()
            .map_err(|_| McpError::new("mcp-registry-poisoned", "servers"))?;
        if servers.contains_key(&server.config.id) {
            return Err(McpError::new("mcp-server-exists", &server.config.id));
        }
        servers.insert(server.config.id.clone(), server);
        Ok(view)
    }

    pub fn connect(
        self: &Arc<Self>,
        server_id: &str,
        force: bool,
    ) -> Result<McpServerView, McpError> {
        let server = self.server(server_id)?;
        {
            let mut state = server
                .state
                .lock()
                .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
            if !server.enabled.load(Ordering::SeqCst) {
                return Err(McpError::new("mcp-server-disabled", server_id));
            }
            if matches!(
                state.status,
                McpConnectionStatus::Ready | McpConnectionStatus::Degraded
            ) {
                drop(state);
                return server_view(&server);
            }
            if state.status == McpConnectionStatus::Connecting {
                return Err(McpError::new("mcp-server-busy", server_id));
            }
            let now = now_millis();
            if !force && state.retry_after_millis.is_some_and(|retry| retry > now) {
                drop(state);
                return server_view(&server);
            }
            state.status = McpConnectionStatus::Connecting;
            state.last_error = None;
        }

        let connected = self.connect_inner(&server);
        match connected {
            Ok((client, tools, resources, prompts, registered_tools, degraded)) => {
                let mut state = server
                    .state
                    .lock()
                    .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
                state.status = if degraded {
                    McpConnectionStatus::Degraded
                } else {
                    McpConnectionStatus::Ready
                };
                state.client = Some(client);
                state.tools = tools;
                state.resources = resources;
                state.prompts = prompts;
                state.registered_tools = registered_tools;
                state.failure_count = 0;
                state.retry_after_millis = None;
                state.last_error = None;
            }
            Err(error) => {
                let mut state = server
                    .state
                    .lock()
                    .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
                state.status = McpConnectionStatus::Failed;
                state.client = None;
                state.failure_count = state.failure_count.saturating_add(1);
                let shift = state.failure_count.min(6);
                let backoff = 500_i64.saturating_mul(1_i64 << shift);
                state.retry_after_millis = Some(now_millis().saturating_add(backoff));
                state.last_error = Some(sanitize_error(&error));
            }
        }
        server_view(&server)
    }

    pub fn disconnect(&self, server_id: &str) -> Result<McpServerView, McpError> {
        let server = self.server(server_id)?;
        let (client, registered) = {
            let mut state = server
                .state
                .lock()
                .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
            let client = state.client.take();
            let registered = std::mem::take(&mut state.registered_tools);
            state.tools.clear();
            state.resources.clear();
            state.prompts.clear();
            state.status = McpConnectionStatus::Disconnected;
            state.last_error = None;
            state.retry_after_millis = None;
            (client, registered)
        };
        let source = ToolSource::Mcp {
            server_id: server_id.to_owned(),
        };
        for tool in registered.into_iter().rev() {
            self.tools
                .unregister(&tool, &source)
                .map_err(tool_to_mcp_error)?;
        }
        if let Some(client) = client {
            client.close()?;
        }
        server_view(&server)
    }

    pub fn remove_server(&self, server_id: &str) -> Result<bool, McpError> {
        let server = self.server(server_id)?;
        if server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?
            .status
            != McpConnectionStatus::Disconnected
        {
            return Err(McpError::new("mcp-server-must-disconnect", server_id));
        }
        Ok(self
            .servers
            .lock()
            .map_err(|_| McpError::new("mcp-registry-poisoned", "servers"))?
            .remove(server_id)
            .is_some())
    }

    pub fn enable_server(&self, server_id: &str) -> Result<McpServerView, McpError> {
        let server = self.server(server_id)?;
        server.enabled.store(true, Ordering::SeqCst);
        server_view(&server)
    }

    pub fn disable_server(&self, server_id: &str) -> Result<McpServerView, McpError> {
        let server = self.server(server_id)?;
        self.disconnect(server_id)?;
        server.enabled.store(false, Ordering::SeqCst);
        server_view(&server)
    }

    pub fn list_servers(&self) -> Result<Vec<McpServerView>, McpError> {
        self.servers
            .lock()
            .map_err(|_| McpError::new("mcp-registry-poisoned", "servers"))?
            .values()
            .map(|server| server_view(server))
            .collect()
    }

    pub fn oauth_start(&self, server_id: &str) -> Result<McpOAuthStart, McpError> {
        let server = self.server(server_id)?;
        let McpTransportConfig::StreamableHttp(http) = &server.config.transport else {
            return Err(McpError::new("mcp-oauth-http-only", server_id));
        };
        let config = http
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-oauth-not-configured", server_id))?;
        self.oauth.start(server_id, &http.endpoint, config)
    }

    pub fn oauth_finish(&self, server_id: &str) -> Result<McpOAuthStatus, McpError> {
        let server = self.server(server_id)?;
        let McpTransportConfig::StreamableHttp(http) = &server.config.transport else {
            return Err(McpError::new("mcp-oauth-http-only", server_id));
        };
        let config = http
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-oauth-not-configured", server_id))?;
        self.oauth.finish(server_id, config)
    }

    pub fn oauth_refresh(&self, server_id: &str) -> Result<McpOAuthStatus, McpError> {
        let server = self.server(server_id)?;
        let McpTransportConfig::StreamableHttp(http) = &server.config.transport else {
            return Err(McpError::new("mcp-oauth-http-only", server_id));
        };
        let config = http
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-oauth-not-configured", server_id))?;
        self.oauth.refresh(server_id, &http.endpoint, config)
    }

    pub fn oauth_status(&self, server_id: &str) -> Result<McpOAuthStatus, McpError> {
        let server = self.server(server_id)?;
        let config = match &server.config.transport {
            McpTransportConfig::StreamableHttp(http) => http.oauth.as_ref(),
            McpTransportConfig::Stdio(_) => None,
        };
        self.oauth.status(server_id, config)
    }

    pub fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDescriptor>, McpError> {
        let server = self.server(server_id)?;
        let result = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?
            .tools
            .clone();
        Ok(result)
    }

    pub fn list_resources(&self, server_id: &str) -> Result<Vec<McpResourceDescriptor>, McpError> {
        let server = self.server(server_id)?;
        let result = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?
            .resources
            .clone();
        Ok(result)
    }

    pub fn list_prompts(&self, server_id: &str) -> Result<Vec<McpPromptDescriptor>, McpError> {
        let server = self.server(server_id)?;
        let result = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?
            .prompts
            .clone();
        Ok(result)
    }

    pub fn read_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> Result<Vec<serde_json::Value>, McpError> {
        let server = self.server(server_id)?;
        let state = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
        if !state.resources.iter().any(|resource| resource.uri == uri) {
            return Err(McpError::new("mcp-resource-not-found", uri));
        }
        state
            .client
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-server-not-ready", server_id))?
            .read_resource(uri)
    }

    pub fn poll_notifications(&self, server_id: &str) -> Result<Vec<serde_json::Value>, McpError> {
        let server = self.server(server_id)?;
        let state = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
        state
            .client
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-server-not-ready", server_id))?
            .poll_notifications()
    }

    fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpCallToolResult, McpError> {
        let server = self.server(server_id)?;
        let state = server
            .state
            .lock()
            .map_err(|_| McpError::new("mcp-server-poisoned", server_id))?;
        if !state.tools.iter().any(|tool| tool.name == tool_name) {
            return Err(McpError::new("mcp-tool-not-found", tool_name));
        }
        state
            .client
            .as_ref()
            .ok_or_else(|| McpError::new("mcp-server-not-ready", server_id))?
            .call_tool(tool_name, arguments)
    }

    fn connect_inner(
        self: &Arc<Self>,
        server: &Arc<ManagedServer>,
    ) -> Result<ConnectedServer, McpError> {
        let client = match &server.config.transport {
            McpTransportConfig::Stdio(config) => {
                let transport = StdioMcpTransport::spawn(config, &self.project_root)?;
                McpClient::initialize(transport)?
            }
            McpTransportConfig::StreamableHttp(config) => {
                let transport = StreamableHttpMcpTransport::new(
                    config.clone(),
                    self.credentials.clone(),
                    self.http_transport.clone(),
                )?;
                match McpClient::initialize(transport.clone()) {
                    Ok(client) => client,
                    Err(error) if config.legacy_sse_fallback && legacy_fallback_error(&error) => {
                        let _ = transport.close();
                        let legacy = legacy::LegacySseMcpTransport::connect(
                            config,
                            self.credentials.clone(),
                            self.http_transport.clone(),
                        )?;
                        McpClient::initialize(legacy)?
                    }
                    Err(error) => {
                        let _ = transport.close();
                        return Err(error);
                    }
                }
            }
        };
        if client
            .instructions()
            .is_some_and(|value| value.len() > 64 * 1024)
        {
            client.close()?;
            return Err(McpError::new(
                "mcp-instructions-too-large",
                server.config.id.clone(),
            ));
        }
        let tools = client.list_tools()?;
        let resources = client.list_resources()?;
        let prompts = client.list_prompts()?;
        let mut registered = Vec::<String>::new();
        let mut degraded = false;
        for tool in &tools {
            if tool.task_support == Some(McpTaskSupport::Required) {
                degraded = true;
                continue;
            }
            let descriptor = mcp_tool_descriptor(&server.config, tool);
            let canonical_name = descriptor.canonical_name.clone();
            let provider = Arc::new(McpToolProvider {
                manager: Arc::downgrade(self),
                server_id: server.config.id.clone(),
                tool_name: tool.name.clone(),
                side_effect: mcp_tool_side_effect(&server.config, tool),
            });
            if let Err(error) = self.tools.register(descriptor, provider) {
                let source = ToolSource::Mcp {
                    server_id: server.config.id.clone(),
                };
                for registered_name in registered.into_iter().rev() {
                    let _ = self.tools.unregister(&registered_name, &source);
                }
                client.close()?;
                return Err(tool_to_mcp_error(error));
            }
            registered.push(canonical_name);
        }
        Ok((client, tools, resources, prompts, registered, degraded))
    }

    fn server(&self, server_id: &str) -> Result<Arc<ManagedServer>, McpError> {
        self.servers
            .lock()
            .map_err(|_| McpError::new("mcp-registry-poisoned", "servers"))?
            .get(server_id)
            .cloned()
            .ok_or_else(|| McpError::new("mcp-server-not-found", server_id))
    }
}

type ConnectedServer = (
    McpClient,
    Vec<McpToolDescriptor>,
    Vec<McpResourceDescriptor>,
    Vec<McpPromptDescriptor>,
    Vec<String>,
    bool,
);

impl Drop for McpManager {
    fn drop(&mut self) {
        let servers = self
            .servers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for server in servers.values() {
            let mut state = server
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = ToolSource::Mcp {
                server_id: server.config.id.clone(),
            };
            for tool in state.registered_tools.drain(..).rev() {
                let _ = self.tools.unregister(&tool, &source);
            }
            if let Some(client) = state.client.take() {
                let _ = client.close();
            }
        }
    }
}

struct McpToolProvider {
    manager: Weak<McpManager>,
    server_id: String,
    tool_name: String,
    side_effect: bool,
}

impl ToolProvider for McpToolProvider {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if !value.is_object() {
            return Err(ToolError::new(
                "invalid-mcp-tool-args",
                self.tool_name.clone(),
            ));
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let content = value.get("content").and_then(serde_json::Value::as_array);
        if content.is_none() {
            return Err(ToolError::new(
                "invalid-mcp-tool-result",
                self.tool_name.clone(),
            ));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        Ok(PermissionAction::McpCall {
            server_id: self.server_id.clone(),
            tool_name: self.tool_name.clone(),
            side_effect: self.side_effect,
            arguments_sha256: json_sha256(args)?,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", self.tool_name.clone()));
        }
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| ToolError::new("mcp-manager-dropped", self.server_id.clone()))?;
        let result = manager
            .call_tool(&self.server_id, &self.tool_name, input.args)
            .map_err(mcp_to_tool_error)?;
        if result.is_error {
            return Err(ToolError::new(
                "mcp-tool-returned-error",
                self.tool_name.clone(),
            ));
        }
        serde_json::to_value(result)
            .map_err(|error| ToolError::new("mcp-tool-result-json", error.to_string()))
    }
}

fn mcp_tool_descriptor(config: &McpServerConfig, tool: &McpToolDescriptor) -> ToolDescriptor {
    ToolDescriptor {
        canonical_name: format!("mcp.{}.{}", config.id, tool.name),
        version: "1".to_owned(),
        description: tool
            .description
            .clone()
            .or_else(|| tool.title.clone())
            .unwrap_or_else(|| tool.name.clone()),
        effect_class: mcp_tool_effect_class(config, tool),
        source: ToolSource::Mcp {
            server_id: config.id.clone(),
        },
        prompt_loading: ToolPromptLoading::OnDemand,
        keywords: [config.name.clone(), config.id.clone(), tool.name.clone()]
            .into_iter()
            .chain(tool.title.clone())
            .collect(),
        input_schema: tool.input_schema.clone(),
        output_schema: tool
            .output_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type":"object"})),
    }
}

fn mcp_tool_effect_class(config: &McpServerConfig, tool: &McpToolDescriptor) -> ToolEffectClass {
    if !config.trust_annotations {
        return ToolEffectClass::VerifiableEffect;
    }
    if tool.annotations.read_only_hint == Some(true) {
        ToolEffectClass::ReadOnlyRetryable
    } else if tool.annotations.idempotent_hint == Some(true) {
        ToolEffectClass::IdempotentEffect
    } else if tool.annotations.destructive_hint == Some(true) {
        ToolEffectClass::NonRepeatableEffect
    } else {
        ToolEffectClass::VerifiableEffect
    }
}

fn mcp_tool_side_effect(config: &McpServerConfig, tool: &McpToolDescriptor) -> bool {
    !config.trust_annotations || tool.annotations.read_only_hint != Some(true)
}

fn server_view(server: &ManagedServer) -> Result<McpServerView, McpError> {
    let state = server
        .state
        .lock()
        .map_err(|_| McpError::new("mcp-server-poisoned", &server.config.id))?;
    Ok(McpServerView {
        id: server.config.id.clone(),
        name: server.config.name.clone(),
        enabled: server.enabled.load(Ordering::SeqCst),
        transport: state.client.as_ref().map_or_else(
            || {
                match &server.config.transport {
                    McpTransportConfig::Stdio(_) => "stdio",
                    McpTransportConfig::StreamableHttp(_) => "streamable-http",
                }
                .to_owned()
            },
            |client| client.transport_kind().to_owned(),
        ),
        authorization: match &server.config.transport {
            McpTransportConfig::Stdio(_) => "stdio-environment".to_owned(),
            McpTransportConfig::StreamableHttp(config) if config.oauth.is_some() => {
                "oauth-2.1-pkce".to_owned()
            }
            McpTransportConfig::StreamableHttp(config) if config.bearer_credential_id.is_some() => {
                "preprovisioned-bearer".to_owned()
            }
            McpTransportConfig::StreamableHttp(_) => "none".to_owned(),
        },
        status: state.status,
        protocol_version: state
            .client
            .as_ref()
            .map(|client| client.protocol_version().to_owned()),
        server_name: state
            .client
            .as_ref()
            .map(|client| client.server_info().name.clone()),
        server_version: state
            .client
            .as_ref()
            .map(|client| client.server_info().version.clone()),
        tool_count: state.tools.len(),
        supported_tool_count: state.registered_tools.len(),
        resource_count: state.resources.len(),
        prompt_count: state.prompts.len(),
        retry_after_millis: state.retry_after_millis,
        last_error: state.last_error.clone(),
    })
}

fn validate_transport_config(config: &McpTransportConfig) -> Result<(), McpError> {
    match config {
        McpTransportConfig::Stdio(config) => {
            if !config.command.is_absolute() {
                return Err(McpError::new(
                    "mcp-command-not-absolute",
                    config.command.display().to_string(),
                ));
            }
        }
        McpTransportConfig::StreamableHttp(config) => {
            if config.endpoint.trim().is_empty() {
                return Err(McpError::new("mcp-http-endpoint-empty", "endpoint"));
            }
            if let (Some(bearer), Some(oauth)) = (&config.bearer_credential_id, &config.oauth)
                && bearer != &oauth.credential_id
            {
                return Err(McpError::new(
                    "mcp-oauth-credential-id-mismatch",
                    "bearerCredentialId must equal oauth.credentialId",
                ));
            }
        }
    }
    Ok(())
}

fn validate_id(value: &str, code: &'static str) -> Result<(), McpError> {
    if value.is_empty()
        || value.len() > 64
        || value
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    {
        return Err(McpError::new(code, value));
    }
    Ok(())
}

fn sanitize_error(error: &McpError) -> String {
    format!(
        "{}: {}",
        error.code,
        error.message.chars().take(512).collect::<String>()
    )
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn tool_to_mcp_error(error: ToolError) -> McpError {
    McpError::new(error.code, error.message)
}

fn mcp_to_tool_error(error: McpError) -> ToolError {
    ToolError::new(error.code, error.message)
}

fn legacy_fallback_error(error: &McpError) -> bool {
    error.code == "mcp-http-status" && matches!(error.message.as_str(), "400" | "404" | "405")
}

fn json_sha256(value: &serde_json::Value) -> Result<String, ToolError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ToolError::new("mcp-tool-args-json", error.to_string()))?;
    Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
}
