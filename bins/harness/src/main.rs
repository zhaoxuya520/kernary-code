#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell as CompletionShell;
use harness_agent::{
    AgentBudgetManager, AgentExecutionOutcome, AgentMessageBus, AgentStateStore, FileLeaseManager,
    SharedSteeringBuffer, builtin_agent_catalog,
};
use harness_application::{
    AgentBudgetView, AgentQueueView, AgentTeamContinuation, AgentTeamView, AgentView,
    ApplicationError, AuthView, BrowserCapabilityView, CacheView, CompactionMode, ContextView,
    GoalHistoryView, HarnessApplication, ModelView, PlanView, PreparedAgentTeam,
    ProcessIdGenerator, ProfileView, SessionSummaryView, StatusView, SystemClock, ToolRuntimeView,
    WhyView,
};
use harness_auth::{
    CredentialId, CredentialStore, OPENAI_API_KEY_CREDENTIAL_ID, OsCredentialStore, SecretString,
};
use harness_browser::{
    BrowserActionJournal, BrowserRuntime, BrowserSessionConfig, PlaywrightProcessAdapter,
    SqliteBrowserJournal, register_browser_tools,
};
use harness_builtin_tools::{
    PatchStore, WorkspacePathGuard, WorkspaceSandbox, reconcile_patch_invocations,
    register_file_tools_with_patch_store, register_process_tool,
};
use harness_cache::{CacheEngine, CachePolicy, DiskCache, MemoryCache};
use harness_codex::{CodexAuthBridge, SystemCodexProcessRunner};
use harness_config::{
    ConfigLayer, ConfigManager, EffectiveConfigView, PermissionMode, load_config_file,
};
use harness_event::{EventBus, EventEnvelope, EventSubscription, HarnessEvent};
use harness_http::UreqStreamingTransport;
use harness_lsp::{LspManager, default_lsp_config_path, register_lsp_tools};
use harness_lsp_patch::{LspPatchCoordinator, LspPatchStore, register_lsp_patch_tools};
use harness_mcp::{
    McpConfigFile, McpManager, McpServerConfig, McpStdioConfig, McpStreamableHttpConfig,
    McpTransportConfig, load_config_file as load_mcp_config_file, save_config_file_atomic,
};
use harness_memory::{
    EmbeddingConfig, HttpEmbeddingConfig, HttpEmbeddingFactory, MemoryKind, ProjectMemory,
    RepositoryIndex, RetrievalMode, SemanticCapability,
};
#[cfg(debug_assertions)]
use harness_model::FakeModelProvider;
use harness_model::{
    CancellationToken, ModelCapability, ModelRegistry, ModelRuntime, ReasoningLevel,
    UNCONFIGURED_MODEL_ID, UNCONFIGURED_PROVIDER_ID, UnconfiguredModelProvider,
};
use harness_permission::{
    ApprovalPolicy, PermissionEngine, PermissionRule, PermissionRuleAction, PermissionRuleEffect,
    load_permission_rules, save_permission_rules_atomic, workspace_write_profile,
};
use harness_plugin::PluginManager;
use harness_provider_catalog::{
    ProviderCatalog, ProviderModelCache, default_model_cache_path, default_project_catalog_path,
};
use harness_provider_compatible::{
    CompatibleProvider, CompatibleProviderConfig, CompatibleReasoningField,
};
use harness_provider_runtime::CatalogProviderRuntime;
use harness_skill::{SkillRegistry, SkillSource};
use harness_storage::{ProjectMaintenance, ProjectStateLock, SqliteKernelStore};
use harness_terminal::{
    AgentDisplayMode, BackendResponse, BrowserCommand as BrowserCliCommand, BudgetCommand,
    CommandRegistry, CompactCommandMode, FailoverCommand, GitCommand, IndexCommand,
    InputSuggestion, JsonRenderer, LspCommand, MASCOT_NAME, McpCommand, MemoryCommand,
    PRODUCT_NAME, PRODUCT_SHORT_NAME, ParsedInput, PermissionCommand, PlainRenderer, PluginCommand,
    QueueCommand, RenderStyle, SecretPrompt, SettingLayer, SettingsCommand, SkillCommand,
    SlashCommand, TAGLINE, TeamCommand, TerminalBackend, TerminalCapabilities, TerminalSnapshot,
    TraceCommand, TuiOptions, VectorCommand, run_tui,
};
use harness_tool::{ToolInvocationJournal, ToolInvocationStatus, ToolRegistry, ToolRuntime};
use harness_types::{
    BrowserSessionId, ModelId, ProjectId, ProviderId, SessionId, TaskId, ToolInvocationId,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum UiMode {
    Auto,
    Full,
    Plain,
}

#[derive(Debug, Parser)]
#[command(
    name = "kernary",
    version,
    about = "Kernary Code — terminal-native multi-agent coding runtime"
)]
struct Cli {
    #[arg(long, value_enum, default_value = "auto", global = true)]
    ui: UiMode,
    #[arg(long, global = true)]
    ascii: bool,
    #[arg(long, global = true)]
    no_color: bool,
    /// 启动后选择 provider/model；不会自动 failover。
    #[arg(long, global = true)]
    model: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 运行一个 Kernary task；默认 Fake Model，显式配置后才使用远程 Provider。
    Run {
        /// 任务文本。
        #[arg(required = true, num_args = 1..)]
        prompt: Vec<String>,
        /// 禁止动态 TUI。
        #[arg(long)]
        headless: bool,
        /// 输出 JSON Lines。
        #[arg(long)]
        json: bool,
    },
    /// 面向 CI/Automation 的严格非交互执行；不会提示输入 credential 或 approval。
    Exec {
        /// 任务文本。
        #[arg(required = true, num_args = 1..)]
        prompt: Vec<String>,
        /// 输出单一稳定 JSON document，而不是事件 JSONL。
        #[arg(long)]
        json: bool,
        /// 不输出 Event stream；只保留最终结果（配合 --output 时 stdout 为空）。
        #[arg(long)]
        quiet: bool,
        /// 将结果原子写入文件；默认拒绝覆盖已有文件。
        #[arg(long)]
        output: Option<PathBuf>,
        /// 允许原子替换已有 --output 文件。
        #[arg(long, requires = "output")]
        force: bool,
    },
    /// 检查当前 Terminal Harness 环境与 capability。
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// 安全登录；API Key 通过无回显输入或 stdin 读取。
    Login {
        #[arg(value_enum)]
        provider: AuthProvider,
    },
    /// 删除 OpenAI OS 凭证或委托官方 Codex logout。
    Logout {
        #[arg(value_enum)]
        provider: AuthProvider,
    },
    /// 查看脱敏登录状态。
    Account {
        #[arg(value_enum, default_value = "openai")]
        provider: AuthProvider,
    },
    /// 在 Kernel 启动前备份、校验或恢复项目 SQLite 状态。
    Maintenance {
        #[command(subcommand)]
        operation: MaintenanceCommand,
    },
    /// 生成 Bash/Zsh/Fish/PowerShell/Elvish completion，不创建项目状态。
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// 把完整 ROFF man page 写到 stdout，不创建项目状态。
    Man,
    /// 安全连接内置或自定义 Provider；Key 无回显并进入 OS Credential Store。
    Connect { provider: String },
    /// 列出内置和项目 Provider Catalog，不启动 Kernel。
    Providers,
    /// 列出运行时模型；只有显式 --refresh <provider> 才访问网络目录。
    Models {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, value_name = "PROVIDER")]
        refresh: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MaintenanceCommand {
    /// 创建带 manifest、SHA-256 和 quick_check 的一致备份。
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    /// 只读校验 manifest、文件 hash 和每个 SQLite。
    Verify { backup: PathBuf },
    /// 恢复前自动创建 recovery point；必须显式 --force。
    Restore {
        backup: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthProvider {
    Openai,
    Codex,
    Deepseek,
    Openrouter,
    Compatible,
    Gemini,
    Anthropic,
}

type Application = HarnessApplication<SqliteKernelStore, SystemClock, ProcessIdGenerator>;

const INTERNAL_TEST_PROVIDER: &str = "fake";
const INTERNAL_TEST_MODEL: &str = "deterministic";

fn is_internal_test_model(provider: &ProviderId, model: &ModelId) -> bool {
    provider.as_str() == INTERNAL_TEST_PROVIDER && model.as_str() == INTERNAL_TEST_MODEL
}

fn is_unconfigured_model(provider: &ProviderId, model: &ModelId) -> bool {
    provider.as_str() == UNCONFIGURED_PROVIDER_ID && model.as_str() == UNCONFIGURED_MODEL_ID
}

fn is_hidden_internal_model(provider: &ProviderId, model: &ModelId) -> bool {
    is_internal_test_model(provider, model) || is_unconfigured_model(provider, model)
}

fn is_hidden_internal_model_name(selection: &str) -> bool {
    selection == "fake/deterministic" || selection == "kernary-internal/unconfigured"
}

#[cfg(debug_assertions)]
fn internal_test_model_enabled() -> bool {
    compat_env_var("HARNESS_ENABLE_TEST_MODEL")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
}

#[cfg(not(debug_assertions))]
const fn internal_test_model_enabled() -> bool {
    false
}

fn selection_is_ready(
    catalog: &ProviderCatalog,
    credentials: &dyn CredentialStore,
    provider_id: &ProviderId,
    model_id: &ModelId,
    test_model_enabled: bool,
) -> bool {
    if is_internal_test_model(provider_id, model_id) {
        return test_model_enabled;
    }
    if is_unconfigured_model(provider_id, model_id) {
        return false;
    }
    let Some(provider) = catalog.get(provider_id) else {
        return false;
    };
    if !provider
        .routes
        .iter()
        .any(|route| route.models.iter().any(|candidate| candidate == model_id))
    {
        return false;
    }
    if !provider.credential_required {
        return true;
    }
    provider
        .credential_id
        .as_deref()
        .is_some_and(|credential_id| {
            credentials
                .get(&CredentialId::new(credential_id))
                .is_ok_and(|secret| secret.is_some())
        })
}

fn slash_requires_ready_model(command: &SlashCommand) -> bool {
    matches!(
        command,
        SlashCommand::Reasoning { .. }
            | SlashCommand::Review { .. }
            | SlashCommand::Resume
            | SlashCommand::Team {
                operation: TeamCommand::Create { .. } | TeamCommand::Workflow { .. }
            }
    )
}

struct AppBackend {
    application: Application,
    registry: CommandRegistry,
    project_root: String,
    mcp_config_path: PathBuf,
    mcp_configs: BTreeMap<String, McpServerConfig>,
    permission_rules_path: PathBuf,
    permission_rules: BTreeMap<String, PermissionRule>,
    event_log_subscription: EventSubscription,
    event_log: VecDeque<EventEnvelope>,
    background_team: Option<BackgroundTeam>,
    pending_credential: Option<PendingCredential>,
    model_ready: bool,
    test_model_enabled: bool,
    /// 最后释放，保证 Application 与后台 Worker 都已停止。
    _project_lock: ProjectStateLock,
}

struct PendingCredential {
    request_id: String,
    provider_id: ProviderId,
    display_name: String,
    credential_id: String,
}

struct BackgroundTeam {
    receiver: Receiver<Result<Vec<AgentExecutionOutcome>, ApplicationError>>,
    continuation: AgentTeamContinuation,
    cancellations: BTreeMap<TaskId, CancellationToken>,
    steering: SharedSteeringBuffer,
    cancellation_requested: bool,
    cancelled_tasks: BTreeSet<TaskId>,
}

impl AppBackend {
    fn response_error(error: impl std::fmt::Display) -> BackendResponse {
        BackendResponse {
            lines: vec![format!("! {error}")],
            ..BackendResponse::default()
        }
    }

    fn model_not_configured_response(&self) -> BackendResponse {
        Self::response_error(
            "MODEL_NOT_CONFIGURED: 尚未配置可用的真实模型；输入 /connect 选择 Provider，再输入 /model 选择模型",
        )
    }

    fn provider_credential_ready(&self, provider_id: &ProviderId) -> Result<bool, String> {
        let catalog = load_provider_catalog(Path::new(&self.project_root))
            .map_err(|error| error.to_string())?;
        let provider = catalog
            .get(provider_id)
            .ok_or_else(|| format!("Provider Catalog 中不存在 {provider_id}"))?;
        if !provider.credential_required {
            return Ok(true);
        }
        let credential_id = provider
            .credential_id
            .as_deref()
            .ok_or_else(|| format!("Provider {provider_id} 缺少 credential_id"))?;
        let store =
            OsCredentialStore::new("dev.openai.harness").map_err(|error| error.to_string())?;
        store
            .get(&CredentialId::new(credential_id))
            .map(|secret| secret.is_some())
            .map_err(|error| error.to_string())
    }

    fn select_model(&mut self, provider: String, model: String) -> BackendResponse {
        let provider_id = ProviderId::from(provider);
        let model_id = ModelId::from(model);
        if is_hidden_internal_model(&provider_id, &model_id)
            && !(self.test_model_enabled && is_internal_test_model(&provider_id, &model_id))
        {
            return Self::response_error(
                "fake/deterministic 仅供显式测试使用，发布版本不能选择测试模型",
            );
        }
        match self.provider_credential_ready(&provider_id) {
            Ok(true) => {}
            Ok(false) => {
                return Self::response_error(format!(
                    "CredentialRequired: {provider_id} 尚未连接；先输入 /connect {provider_id}"
                ));
            }
            Err(error) => return Self::response_error(error),
        }
        match self.application.select_model(provider_id, model_id) {
            Ok(model) => {
                self.model_ready = true;
                BackendResponse {
                    lines: Self::model_lines(&model),
                    ..BackendResponse::default()
                }
            }
            Err(error) => Self::response_error(error),
        }
    }

    fn input_suggestions(&self, input: &str) -> Vec<InputSuggestion> {
        let normalized = input.trim_start();
        let Some((command, remainder)) = normalized.split_once(char::is_whitespace) else {
            return Vec::new();
        };
        let prefix = remainder.trim_start();
        if matches!(command, "/connect" | "/logout")
            || (command == "/models" && prefix.starts_with("refresh "))
        {
            let (replacement_prefix, provider_prefix) = if command == "/models" {
                (
                    "/models refresh ",
                    prefix.trim_start_matches("refresh ").trim(),
                )
            } else {
                (
                    if command == "/connect" {
                        "/connect "
                    } else {
                        "/logout "
                    },
                    prefix,
                )
            };
            let Ok(catalog) = load_provider_catalog(Path::new(&self.project_root)) else {
                return Vec::new();
            };
            return catalog
                .list()
                .into_iter()
                .filter(|provider| provider.id.as_str().starts_with(provider_prefix))
                .map(|provider| {
                    let credential = if provider.credential_required {
                        "secure credential"
                    } else {
                        "no credential required"
                    };
                    InputSuggestion::new(
                        format!("{replacement_prefix}{}", provider.id),
                        format!("{} · {}", provider.id, provider.display_name),
                        format!("{} route(s) · {credential}", provider.routes.len()),
                    )
                })
                .collect();
        }
        if command != "/model" {
            return Vec::new();
        }
        let catalog = load_provider_catalog(Path::new(&self.project_root)).ok();
        self.application
            .models()
            .into_iter()
            .filter(|capability| {
                !is_unconfigured_model(&capability.provider_id, &capability.model_id)
                    && (self.test_model_enabled
                        || !is_internal_test_model(&capability.provider_id, &capability.model_id))
            })
            .filter_map(|capability| {
                let selection = format!("{}/{}", capability.provider_id, capability.model_id);
                selection.starts_with(prefix).then(|| {
                    let credential = catalog
                        .as_ref()
                        .and_then(|catalog| catalog.get(&capability.provider_id))
                        .map_or("custom route", |provider| {
                            if provider.credential_required {
                                "use /connect before selection"
                            } else {
                                "local/keyless route"
                            }
                        });
                    InputSuggestion::new(
                        format!("/model {selection}"),
                        selection,
                        format!(
                            "ctx {} · output {} · tools {} · {credential}",
                            capability.context_window_tokens,
                            capability.max_output_tokens,
                            capability.tool_calling
                        ),
                    )
                })
            })
            .collect()
    }

    fn persist_mcp_configs(
        &self,
        configs: &BTreeMap<String, McpServerConfig>,
    ) -> Result<(), harness_mcp::McpError> {
        save_config_file_atomic(
            &self.mcp_config_path,
            &McpConfigFile {
                servers: configs.values().cloned().collect(),
            },
        )
    }

    fn add_mcp_server(&mut self, config: McpServerConfig) -> BackendResponse {
        if self.mcp_configs.contains_key(&config.id) {
            return Self::response_error(format!("MCP server 已存在：{}", config.id));
        }
        let view = match self.application.mcp_add_server(config.clone()) {
            Ok(view) => view,
            Err(error) => return Self::response_error(error),
        };
        let mut candidate = self.mcp_configs.clone();
        candidate.insert(config.id.clone(), config);
        if let Err(error) = self.persist_mcp_configs(&candidate) {
            let _ = self.application.mcp_remove_server(&view.id);
            return Self::response_error(error);
        }
        self.mcp_configs = candidate;
        BackendResponse {
            lines: vec![format!(
                "MCP {} · added · enabled={} · {} · persisted={}",
                view.id,
                view.enabled,
                view.transport,
                self.mcp_config_path.display()
            )],
            ..BackendResponse::default()
        }
    }

    fn remove_mcp_server(&mut self, server_id: &str) -> BackendResponse {
        if !self.mcp_configs.contains_key(server_id) {
            return Self::response_error(format!("MCP server 不属于可写项目配置：{server_id}"));
        }
        let mut candidate = self.mcp_configs.clone();
        candidate.remove(server_id);
        if let Err(error) = self.persist_mcp_configs(&candidate) {
            return Self::response_error(error);
        }
        if let Err(error) = self.application.mcp_remove_server(server_id) {
            let _ = self.persist_mcp_configs(&self.mcp_configs);
            return Self::response_error(error);
        }
        self.mcp_configs = candidate;
        BackendResponse {
            lines: vec![format!("MCP {server_id} · removed=true")],
            ..BackendResponse::default()
        }
    }

    fn set_mcp_enabled(&mut self, server_id: &str, enabled: bool) -> BackendResponse {
        if !self.mcp_configs.contains_key(server_id) {
            return Self::response_error(format!("MCP server 不属于可写项目配置：{server_id}"));
        }
        let mut candidate = self.mcp_configs.clone();
        if let Some(config) = candidate.get_mut(server_id) {
            config.enabled = enabled;
        }
        if let Err(error) = self.persist_mcp_configs(&candidate) {
            return Self::response_error(error);
        }
        let view = match if enabled {
            self.application.mcp_enable_server(server_id)
        } else {
            self.application.mcp_disable_server(server_id)
        } {
            Ok(view) => view,
            Err(error) => {
                let _ = self.persist_mcp_configs(&self.mcp_configs);
                return Self::response_error(error);
            }
        };
        self.mcp_configs = candidate;
        BackendResponse {
            lines: vec![format!(
                "MCP {} · enabled={} · {:?} · persisted={}",
                view.id,
                view.enabled,
                view.status,
                self.mcp_config_path.display()
            )],
            ..BackendResponse::default()
        }
    }

    fn add_permission_rule(
        &mut self,
        effect: PermissionRuleEffect,
        action: PermissionRuleAction,
        pattern: String,
    ) -> BackendResponse {
        let rule = match self
            .application
            .add_permission_rule(effect, action, pattern)
        {
            Ok(rule) => rule,
            Err(error) => return Self::response_error(error),
        };
        let mut candidate = self.permission_rules.clone();
        candidate.insert(rule.id.clone(), rule.clone());
        if let Err(error) = save_permission_rules_atomic(
            &self.permission_rules_path,
            &candidate.values().cloned().collect::<Vec<_>>(),
        ) {
            let _ = self.application.remove_permission_rule(&rule.id);
            return Self::response_error(error);
        }
        self.permission_rules = candidate;
        BackendResponse {
            lines: vec![format!(
                "Permission rule {} · {:?} {:?} {} · persisted={}",
                rule.id,
                rule.effect,
                rule.action,
                rule.pattern,
                self.permission_rules_path.display()
            )],
            ..BackendResponse::default()
        }
    }

    fn remove_permission_rule(&mut self, rule_id: &str) -> BackendResponse {
        if !self.permission_rules.contains_key(rule_id) {
            return Self::response_error(format!("Permission rule 不存在：{rule_id}"));
        }
        let mut candidate = self.permission_rules.clone();
        candidate.remove(rule_id);
        if let Err(error) = save_permission_rules_atomic(
            &self.permission_rules_path,
            &candidate.values().cloned().collect::<Vec<_>>(),
        ) {
            return Self::response_error(error);
        }
        match self.application.remove_permission_rule(rule_id) {
            Ok(true) => {
                self.permission_rules = candidate;
                BackendResponse {
                    lines: vec![format!("Permission rule {rule_id} · removed=true")],
                    ..BackendResponse::default()
                }
            }
            Ok(false) => {
                let _ = save_permission_rules_atomic(
                    &self.permission_rules_path,
                    &self.permission_rules.values().cloned().collect::<Vec<_>>(),
                );
                Self::response_error(format!("Permission runtime rule 不存在：{rule_id}"))
            }
            Err(error) => {
                let _ = save_permission_rules_atomic(
                    &self.permission_rules_path,
                    &self.permission_rules.values().cloned().collect::<Vec<_>>(),
                );
                Self::response_error(error)
            }
        }
    }

    fn begin_provider_connect(&mut self, provider: Option<String>) -> BackendResponse {
        if self.background_team.is_some() {
            return Self::response_error("Agent Team 运行期间不能输入凭证；请先取消或等待完成");
        }
        let Some(provider) = provider else {
            return BackendResponse {
                lines: vec!["用法：/connect <provider>；先运行 /providers 查看目录".to_owned()],
                ..BackendResponse::default()
            };
        };
        let catalog = match load_provider_catalog(Path::new(&self.project_root)) {
            Ok(catalog) => catalog,
            Err(error) => return Self::response_error(error),
        };
        let provider_id = ProviderId::from(provider);
        let Some(definition) = catalog.get(&provider_id) else {
            return Self::response_error(format!("Provider Catalog 中不存在 {provider_id}"));
        };
        if !definition.credential_required {
            return BackendResponse {
                lines: vec![format!("{} 不需要 API key", definition.display_name)],
                ..BackendResponse::default()
            };
        }
        let Some(credential_id) = definition.credential_id.clone() else {
            return Self::response_error("Provider 缺少 credential_id");
        };
        let request_id = format!("provider-credential:{provider_id}");
        self.pending_credential = Some(PendingCredential {
            request_id: request_id.clone(),
            provider_id,
            display_name: definition.display_name.clone(),
            credential_id,
        });
        BackendResponse {
            lines: vec![format!(
                "Kernary secure lane · 输入 {} API key；Esc/Ctrl+C 取消",
                definition.display_name
            )],
            secret_prompt: Some(SecretPrompt {
                request_id,
                prompt: format!("{} API key", definition.display_name),
            }),
            ..BackendResponse::default()
        }
    }

    fn provider_catalog_lines(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let project_root = Path::new(&self.project_root);
        let catalog = load_provider_catalog(project_root)?;
        let cache = load_provider_model_cache(project_root);
        let now_millis = unix_millis()?;
        let store = OsCredentialStore::new("dev.openai.harness")?;
        Ok(std::iter::once("Kernary Provider Catalog".to_owned())
            .chain(catalog.list().into_iter().map(|provider| {
                let models = provider
                    .routes
                    .iter()
                    .map(|route| route.models.len())
                    .sum::<usize>();
                let credential = match provider.credential_id.as_deref() {
                    None => "not-required",
                    Some(id) => match store.get(&CredentialId::new(id)) {
                        Ok(Some(_)) => "configured",
                        Ok(None) => "missing",
                        Err(_) => "store-unavailable",
                    },
                };
                let discovery = cache.status(&provider, now_millis);
                let discovery_counts = cache.get(&provider.id).map_or_else(
                    || "discovered=0 · routable=0".to_owned(),
                    |entry| {
                        format!(
                            "discovered={} · routable={}",
                            entry.discovered_models.len(),
                            entry.routable_models.len()
                        )
                    },
                );
                format!(
                    "{} · {} · routes={} · models={} · credential={credential} · discovery={discovery} · {discovery_counts}",
                    provider.id,
                    provider.display_name,
                    provider.routes.len(),
                    models
                )
            }))
            .collect())
    }

    fn status_lines(status: &StatusView) -> Vec<String> {
        let model = if is_hidden_internal_model_name(&status.model) {
            "未配置".to_owned()
        } else {
            status.model.clone()
        };
        vec![
            format!(
                "Session  {} · {:?}",
                status.session_id, status.session_status
            ),
            format!(
                "Goal     {}{}",
                status.goal.as_deref().unwrap_or("<empty>"),
                if status.goal_locked { " [locked]" } else { "" }
            ),
            format!("Model    {model} · reasoning {}", status.reasoning),
            format!(
                "Mode     {} · session version {}",
                status.mode, status.session_version
            ),
        ]
    }

    fn goal_history_lines(history: &GoalHistoryView) -> Vec<String> {
        let mut lines = vec![format!(
            "Goal history · {} revision(s) · current={} · locked={}",
            history.revisions.len(),
            history
                .current_revision_id
                .as_ref()
                .map_or_else(|| "<cleared>".to_owned(), ToString::to_string),
            history.locked
        )];
        lines.extend(history.revisions.iter().map(|revision| {
            format!(
                "{}{} · parent={} · by={} · at={} · {}",
                revision.id,
                if history.current_revision_id.as_ref() == Some(&revision.id) {
                    " [current]"
                } else {
                    ""
                },
                revision
                    .parent_revision_id
                    .as_ref()
                    .map_or_else(|| "<root>".to_owned(), ToString::to_string),
                revision.created_by,
                revision.created_at_millis,
                revision.text
            )
        }));
        lines
    }

    fn session_lines(sessions: &[SessionSummaryView]) -> Vec<String> {
        let mut lines = vec![format!("Sessions · {}", sessions.len())];
        lines.extend(sessions.iter().map(|session| {
            format!(
                "{}{} · {:?} · version={} · goal={} · parent={} · checkpoint={}",
                session.session_id,
                if session.current { " [current]" } else { "" },
                session.status,
                session.version,
                session.goal.as_deref().unwrap_or("<empty>"),
                session
                    .parent_session_id
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), ToString::to_string),
                session
                    .forked_from_checkpoint_id
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), ToString::to_string)
            )
        }));
        lines
    }

    fn config_lines(
        view: &EffectiveConfigView,
        key: Option<&str>,
        include_paths: bool,
    ) -> Vec<String> {
        let values = view.values();
        let mut lines = Vec::new();
        if include_paths {
            lines.push(format!(
                "Config   global={} · project={}",
                view.global_path
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
                view.project_path
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string())
            ));
            lines.push("Layers   default < global < project < session < runtime".to_owned());
        }
        match key {
            Some(key) if values.contains_key(key) => {
                let value = &values[key];
                lines.push(format!(
                    "{key}={value} · source={}",
                    view.provenance
                        .get(key)
                        .map_or_else(|| "unknown".to_owned(), ToString::to_string)
                ));
            }
            Some(category) => {
                let prefix = format!("{category}.");
                let category_values = values
                    .iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .collect::<Vec<_>>();
                if category_values.is_empty() {
                    lines.push(format!("! config-key-unsupported: {category}"));
                } else {
                    lines.extend(category_values.into_iter().map(|(key, value)| {
                        format!(
                            "{key}={value} · source={}",
                            view.provenance
                                .get(key)
                                .map_or_else(|| "unknown".to_owned(), ToString::to_string)
                        )
                    }));
                }
            }
            None => lines.extend(values.into_iter().map(|(key, value)| {
                format!(
                    "{key}={value} · source={}",
                    view.provenance
                        .get(&key)
                        .map_or_else(|| "unknown".to_owned(), ToString::to_string)
                )
            })),
        }
        lines
    }

    fn settings_show(&self, key: Option<&str>) -> BackendResponse {
        let category = key.unwrap_or("");
        let result: Result<Vec<String>, ApplicationError> = match category {
            "model" => self.application.model().map(|view| Self::model_lines(&view)),
            "reasoning" => self.application.model().map(|view| {
                vec![format!(
                    "Reasoning requested={:?} effective={} mapping={:?}",
                    view.reasoning_requested,
                    view.reasoning_effective
                        .map_or_else(|| "unsupported".to_owned(), |level| format!("{level:?}")),
                    view.reasoning_mapping
                )]
            }),
            "agent" | "agents" => self.application.agents().map(|team| {
                let mut lines = Self::budget_lines(&self.application.agent_budget());
                lines.extend(Self::team_lines(&team, AgentDisplayMode::Compact));
                lines
            }),
            "context" => self
                .application
                .context()
                .map(|view| Self::context_lines(&view)),
            "compression" => self.application.context().map(|view| {
                vec![
                    "Compression autoThreshold=80% · modes=safe/aggressive · checkpointBefore=true"
                        .to_owned(),
                    format!("Current context={}%, checkpoints={}", view.percent, view.checkpoint_count),
                ]
            }),
            "memory" => self.application.memory_view().map(|view| {
                vec![format!(
                    "Memory records={} fts={} path={}",
                    view.record_count,
                    view.fts_indexed_count,
                    view.database_path.display()
                )]
            }),
            "vector" => self.vector_settings_lines(),
            "cache" => Ok(Self::cache_lines(&self.application.cache())),
            "mcp" => self.application.mcp_servers().map(|servers| {
                vec![format!(
                    "MCP servers={} enabled={} ready={} · config={}",
                    servers.len(),
                    servers.iter().filter(|server| server.enabled).count(),
                    servers
                        .iter()
                        .filter(|server| matches!(
                            server.status,
                            harness_mcp::McpConnectionStatus::Ready
                                | harness_mcp::McpConnectionStatus::Degraded
                        ))
                        .count(),
                    self.mcp_config_path.display()
                )]
            }),
            "plugin" | "plugins" => self.application.plugins().map(|plugins| {
                vec![format!(
                    "Plugins discovered={} enabled={} · metadata-first=true",
                    plugins.len(),
                    plugins
                        .iter()
                        .filter(|plugin| matches!(
                            plugin.status,
                            harness_plugin::PluginLifecycleStatus::Active
                        ))
                        .count()
                )]
            }),
            "permissions" => self.application.tools().map(|view| {
                let mut lines = Self::config_lines(
                    &self.application.config(),
                    Some("permissions"),
                    false,
                );
                lines.push(format!(
                    "ApprovalPolicy={:?} rules={} pending={} grants={}",
                    view.approval_policy,
                    view.permission_rules.len(),
                    view.pending_approvals,
                    view.active_grants
                ));
                lines
            }),
            "sandbox" => self.application.tools().map(|view| {
                vec![format!(
                    "Sandbox workspaceContainment=true processAllowlist={} browser={} hardDeny=true",
                    view.tools
                        .iter()
                        .any(|tool| tool.canonical_name == "process.exec"),
                    self.application
                        .browser_view()
                        .is_ok_and(|browser| browser.configured)
                )]
            }),
            "browser" => self.application.browser_view().map(|view| {
                vec![format!(
                    "Browser configured={} runtime={}",
                    view.configured,
                    view.runtime.map_or_else(|| "sleeping".to_owned(), |runtime| format!("{:?}", runtime.status))
                )]
            }),
            "terminal" | "ui" => Ok(Self::config_lines(
                &self.application.config(),
                Some("ui"),
                false,
            )),
            "logging" => Ok(Self::config_lines(
                &self.application.config(),
                Some("logging"),
                false,
            )),
            "performance" => self
                .application
                .profile()
                .map(|profile| Self::profile_lines(&profile)),
            "" => Ok(Self::config_lines(&self.application.config(), None, false)),
            _ => Ok(Self::config_lines(
                &self.application.config(),
                Some(category),
                false,
            )),
        };
        match result {
            Ok(lines) => BackendResponse {
                lines,
                ..BackendResponse::default()
            },
            Err(error) => Self::response_error(error),
        }
    }

    fn vector_settings_lines(&self) -> Result<Vec<String>, ApplicationError> {
        let memory = self.application.memory_view()?;
        let repository = self.application.repository_view()?;
        let mut lines = Self::config_lines(&self.application.config(), Some("vector"), false);
        let (embedding_model, backend, hybrid, reranking) = match &memory.semantic {
            SemanticCapability::Absent { reason } => (
                format!("<none> ({reason})"),
                "disabled".to_owned(),
                "off · lexical-only".to_owned(),
                "lexical/path/symbol ranking".to_owned(),
            ),
            SemanticCapability::Blocked { reason } => (
                format!("blocked ({reason})"),
                "disabled".to_owned(),
                "off · lexical fallback".to_owned(),
                "lexical/path/symbol ranking".to_owned(),
            ),
            SemanticCapability::Ready {
                model,
                provider,
                dimensions,
            } => (
                format!("{provider}/{model} ({dimensions}d)"),
                "embedded · ready/lazy".to_owned(),
                "auto · first semantic demand activates".to_owned(),
                "RRF after activation".to_owned(),
            ),
            SemanticCapability::Active {
                model,
                provider,
                dimensions,
                generation,
            } => (
                format!("{provider}/{model} ({dimensions}d)"),
                format!("embedded · active generation={generation}"),
                "on · semantic + lexical + symbol".to_owned(),
                "RRF".to_owned(),
            ),
            SemanticCapability::Degraded { reason } => (
                format!("degraded ({reason})"),
                "embedded · degraded".to_owned(),
                "off · lexical fallback".to_owned(),
                "lexical/path/symbol ranking".to_owned(),
            ),
        };
        lines.extend([
            format!("Backend          {backend}"),
            format!("Embedding Model  {embedding_model}"),
            format!("Storage Path     {}", memory.database_path.display()),
            "Top K            6 automatic / command-bounded 1..50".to_owned(),
            format!("Hybrid Search    {hybrid}"),
            format!("Reranking        {reranking}"),
            format!("Index Code       on · {} files", repository.file_count),
            "Index Chat       off · Session Context is not embedded by default".to_owned(),
            format!("Vector Schema    {}", memory.vector_schema_present),
        ]);
        Ok(lines)
    }

    fn drain_event_log(&mut self) {
        while let Ok(envelope) = self.event_log_subscription.try_recv() {
            self.event_log.push_back(envelope);
            while self.event_log.len() > 1_024 {
                self.event_log.pop_front();
            }
        }
    }

    fn event_log_lines(&self, limit: usize, trace_only: bool) -> Vec<String> {
        let filtered = self
            .event_log
            .iter()
            .filter(|entry| !trace_only || entry.scope.trace_id.is_some())
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            return vec![if trace_only {
                "Trace log: no traced events; use /trace on before the operation".to_owned()
            } else {
                "Event log: no events".to_owned()
            }];
        }
        let skip = filtered.len().saturating_sub(limit);
        filtered
            .into_iter()
            .skip(skip)
            .map(|entry| {
                format!(
                    "#{} · {} · {} · trace={} · scope={}",
                    entry.sequence,
                    entry.recorded_at_millis,
                    event_kind(&entry.event),
                    entry
                        .scope
                        .trace_id
                        .as_ref()
                        .map_or("-", |trace| trace.as_str()),
                    event_scope_summary(entry)
                )
            })
            .collect()
    }

    fn profile_lines(profile: &ProfileView) -> Vec<String> {
        let mut lines = vec![format!("Profile uptime={}ms", profile.uptime_millis)];
        if profile.metrics.is_empty() {
            lines.push("No profile samples yet".to_owned());
        } else {
            lines.extend(profile.metrics.iter().map(|metric| {
                format!(
                    "{} · count={} total={}ms p50={}ms p95={}ms max={}ms last={}ms",
                    metric.name,
                    metric.count,
                    metric.total_millis,
                    metric.p50_millis,
                    metric.p95_millis,
                    metric.max_millis,
                    metric.last_millis
                )
            }));
        }
        lines
    }

    fn why_lines(why: &WhyView) -> Vec<String> {
        let mut lines = vec![
            "Why · auditable evidence summary (private Chain-of-Thought is never exposed)"
                .to_owned(),
            format!("Goal    {}", why.goal.as_deref().unwrap_or("<empty>")),
            format!(
                "Mission {}",
                why.mission_id
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), ToString::to_string)
            ),
            format!("Basis   {}", why.summary),
        ];
        lines.push(format!(
            "Context {}",
            if why.context_sources.is_empty() {
                "<none>".to_owned()
            } else {
                why.context_sources.join(" | ")
            }
        ));
        if why.recent_tools.is_empty() {
            lines.push("Tools   <none>".to_owned());
        } else {
            lines.extend(why.recent_tools.iter().map(|tool| {
                format!(
                    "Tool    {} · {} · {:?} · at {}",
                    tool.invocation_id, tool.tool_name, tool.status, tool.updated_at_millis
                )
            }));
        }
        lines
    }

    fn debug_lines(&self) -> Vec<String> {
        let mut lines = vec!["Debug snapshot · read-only".to_owned()];
        match self.application.status() {
            Ok(status) => lines.extend(Self::status_lines(&status)),
            Err(error) => lines.push(format!("Session unavailable · {error}")),
        }
        if let Ok(context) = self.application.context() {
            lines.push(format!(
                "Context {} items · {}/{} tokens · {} checkpoints",
                context.item_count,
                context.used_tokens,
                context.max_tokens,
                context.checkpoint_count
            ));
        }
        if let Ok(team) = self.application.agents() {
            lines.push(format!(
                "Agents total={} sleeping={} reserved={} running={} recoverable={}",
                team.total, team.sleeping, team.reserved, team.running, team.recoverable_sessions
            ));
        }
        if let Ok(plan) = self.application.plan()
            && plan.mission_id.is_some()
        {
            lines.push(format!(
                "Schedule accepted={} running={} pending={} blocked={}",
                plan.accepted, plan.running, plan.pending, plan.blocked
            ));
        }
        let budget = self.application.agent_budget();
        lines.push(format!(
            "Budget agents={}/{} tokens={} tools={} runtime={}ms retries={} costUnits={}",
            budget.max_parallel_agents,
            budget.max_agents,
            budget.max_total_tokens,
            budget.max_tool_calls,
            budget.max_runtime_millis,
            budget.max_retries,
            budget.max_cost_units
        ));
        if let Ok(tools) = self.application.tools() {
            lines.push(format!(
                "Tools registered={} pendingApprovals={} grants={}",
                tools.tools.len(),
                tools.pending_approvals,
                tools.active_grants
            ));
        }
        if let Ok(memory) = self.application.memory_view() {
            lines.push(format!(
                "Memory records={} fts={} semantic={:?} vectorSchema={}",
                memory.record_count,
                memory.fts_indexed_count,
                memory.semantic,
                memory.vector_schema_present
            ));
        }
        if let Ok(repository) = self.application.repository_view() {
            lines.push(format!(
                "Repository files={} symbols={} imports={} lspSymbols={} diagnostics={} revision={}",
                repository.file_count,
                repository.symbol_count,
                repository.import_count,
                repository.lsp_symbol_count,
                repository.lsp_diagnostic_count,
                repository.revision
            ));
        }
        lines.extend(Self::cache_lines(&self.application.cache()));
        if let Ok(profile) = self.application.profile() {
            lines.extend(Self::profile_lines(&profile));
        }
        let errors = self
            .event_log
            .iter()
            .filter(|entry| matches!(entry.event, HarnessEvent::Error { .. }))
            .count();
        lines.push(format!(
            "Events retained={} errors={} subscribers={}",
            self.event_log.len(),
            errors,
            self.application.event_bus().subscriber_count()
        ));
        lines
    }

    fn doctor_lines(&self) -> Vec<String> {
        let mut lines = vec!["Doctor · runtime diagnostics".to_owned()];
        lines.push(format!(
            "Project root={} · exists={} · directory={}",
            self.project_root,
            Path::new(&self.project_root).exists(),
            Path::new(&self.project_root).is_dir()
        ));
        lines.push(format!(
            "Kernel state={} · config={} · model={} · tools={} · memory={} · repository={}",
            Path::new(&self.project_root)
                .join(".harness/kernel.sqlite")
                .is_file(),
            "ok",
            self.application.model().is_ok(),
            self.application.tools().is_ok(),
            self.application.memory_view().is_ok(),
            self.application.repository_view().is_ok()
        ));
        let git = compat_env_var_os("HARNESS_GIT_EXECUTABLE")
            .map(PathBuf::from)
            .filter(|path| path.is_file());
        lines.push(format!(
            "Git configured={} · executable={}",
            git.is_some(),
            git.as_ref()
                .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string())
        ));
        match self.application.mcp_servers() {
            Ok(servers) => lines.push(format!(
                "MCP servers={} enabled={} failed={} · config={}",
                servers.len(),
                servers.iter().filter(|server| server.enabled).count(),
                servers
                    .iter()
                    .filter(|server| matches!(
                        server.status,
                        harness_mcp::McpConnectionStatus::Failed
                    ))
                    .count(),
                self.mcp_config_path.display()
            )),
            Err(error) => lines.push(format!("MCP unavailable · {error}")),
        }
        match self.application.plugins() {
            Ok(plugins) => lines.push(format!(
                "Plugins discovered={} enabled={} failed={}",
                plugins.len(),
                plugins
                    .iter()
                    .filter(|plugin| matches!(
                        plugin.status,
                        harness_plugin::PluginLifecycleStatus::Active
                    ))
                    .count(),
                plugins
                    .iter()
                    .filter(|plugin| plugin.last_error.is_some())
                    .count()
            )),
            Err(error) => lines.push(format!("Plugins unavailable · {error}")),
        }
        if let Ok(browser) = self.application.browser_view() {
            lines.push(format!("Browser configured={}", browser.configured));
        }
        if let Ok(memory) = self.application.memory_view() {
            lines.push(format!(
                "Vector semantic={:?} schemaPresent={} hardGate=embedding-model",
                memory.semantic, memory.vector_schema_present
            ));
        }
        let cache = self.application.cache();
        lines.push(format!(
            "Cache l1={} hits/{} misses · l2={}",
            cache.l1.hits,
            cache.l1.misses,
            cache.l2.is_some()
        ));
        lines.extend(Self::config_lines(&self.application.config(), None, true));
        lines
    }

    fn inspect(&self, target: &str) -> BackendResponse {
        let result = match target {
            "session" => self.application.status().map(|view| Self::status_lines(&view)),
            "config" => Ok(Self::config_lines(&self.application.config(), None, true)),
            "context" => self
                .application
                .context()
                .map(|view| Self::context_lines(&view)),
            "memory" | "vector" => self.application.memory_view().map(|view| {
                vec![format!(
                    "Memory project={} records={} fts={} semantic={:?} vectorSchema={} path={}",
                    view.project_id,
                    view.record_count,
                    view.fts_indexed_count,
                    view.semantic,
                    view.vector_schema_present,
                    view.database_path.display()
                )]
            }),
            "repository" => self.application.repository_view().map(|view| {
                vec![format!(
                    "Repository root={} files={} symbols={} imports={} lspSymbols={} diagnostics={} revision={}",
                    view.root.display(),
                    view.file_count,
                    view.symbol_count,
                    view.import_count,
                    view.lsp_symbol_count,
                    view.lsp_diagnostic_count,
                    view.revision
                )]
            }),
            "cache" => Ok(Self::cache_lines(&self.application.cache())),
            "tool" | "tools" => self.application.tools().map(|view| Self::tool_lines(&view)),
            "agent" | "agents" => self
                .application
                .agents()
                .map(|view| Self::team_lines(&view, AgentDisplayMode::Normal)),
            "plan" => self.application.plan().map(|view| Self::plan_lines(&view)),
            _ => unreachable!("Command parser validates inspect target"),
        };
        match result {
            Ok(lines) => BackendResponse {
                lines,
                ..BackendResponse::default()
            },
            Err(error) => Self::response_error(error),
        }
    }

    fn lsp_location_response(
        result: Result<Vec<harness_lsp::LspLocation>, ApplicationError>,
    ) -> BackendResponse {
        match result {
            Ok(locations) => BackendResponse {
                lines: if locations.is_empty() {
                    vec!["LSP locations: no results".to_owned()]
                } else {
                    locations
                        .into_iter()
                        .map(|location| {
                            let (line, character, mapped) = Self::lsp_display_start(
                                &location.range,
                                location.human_range.as_ref(),
                            );
                            format!(
                                "{}:{line}:{character}{}{}",
                                location.path.as_deref().unwrap_or("<unknown>"),
                                if location.external {
                                    " · external"
                                } else {
                                    ""
                                },
                                if mapped {
                                    String::new()
                                } else {
                                    format!(" · protocol-{:?}", location.position_encoding)
                                }
                            )
                        })
                        .collect()
                },
                ..BackendResponse::default()
            },
            Err(error) => Self::response_error(error),
        }
    }

    fn lsp_display_start(
        protocol: &harness_lsp::LspRange,
        human: Option<&harness_lsp::HumanRange>,
    ) -> (u32, u32, bool) {
        human.map_or(
            (protocol.start.line + 1, protocol.start.character + 1, false),
            |range| (range.start.line, range.start.character, true),
        )
    }

    fn lsp_patch_tool_response(&self, tool: &str, args: serde_json::Value) -> BackendResponse {
        match self.application.invoke_tool(tool, args) {
            Ok(invocation) if invocation.status == ToolInvocationStatus::Completed => {
                let result = invocation.result.unwrap_or_default();
                let preview_id = result
                    .get("previewId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown>");
                let status = result
                    .get("status")
                    .map_or_else(|| "ready".to_owned(), serde_json::Value::to_string);
                let files = result
                    .get("files")
                    .or_else(|| result.get("paths"))
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                BackendResponse {
                    lines: vec![format!(
                        "LSP Patch {preview_id} · status={status} · files={files} · tool={tool}"
                    )],
                    ..BackendResponse::default()
                }
            }
            Ok(invocation) if invocation.status == ToolInvocationStatus::WaitingApproval => {
                BackendResponse {
                    lines: vec![format!(
                        "Workspace patch approval required: /approve {} once",
                        invocation.id
                    )],
                    ..BackendResponse::default()
                }
            }
            Ok(invocation) => Self::response_error(format!(
                "Tool {} ended {:?}: {}",
                invocation.id,
                invocation.status,
                invocation.error.as_deref().unwrap_or("no detail")
            )),
            Err(error) => Self::response_error(error),
        }
    }

    fn handle_lsp(&self, operation: LspCommand) -> BackendResponse {
        match operation {
            LspCommand::List => match self.application.lsp_servers() {
                Ok(servers) => BackendResponse {
                    lines: if servers.is_empty() {
                        vec!["LSP: no configured servers".to_owned()]
                    } else {
                        servers
                            .into_iter()
                            .map(|server| {
                                format!(
                                    "{} · {} · languages={} · open={} · capabilities={}",
                                    server.id,
                                    server.status,
                                    server.languages.join(","),
                                    server.open_documents,
                                    server.capabilities.map_or_else(
                                        || "not-initialized".to_owned(),
                                        |capability| format!(
                                            "symbols:{}/definition:{}/references:{}/diagnostics:{}/encoding:{:?}",
                                            capability.document_symbols,
                                            capability.definition,
                                            capability.references,
                                            capability.diagnostics,
                                            capability.position_encoding
                                        )
                                    )
                                )
                            })
                            .collect()
                    },
                    ..BackendResponse::default()
                },
                Err(error) => Self::response_error(error),
            },
            LspCommand::Start { server_id } => match self.application.lsp_start(&server_id) {
                Ok(server) => BackendResponse {
                    lines: vec![format!(
                        "LSP {} ready · languages={}",
                        server.id,
                        server.languages.join(",")
                    )],
                    ..BackendResponse::default()
                },
                Err(error) => Self::response_error(error),
            },
            LspCommand::Stop { server_id } => match self.application.lsp_stop(&server_id) {
                Ok(stopped) => BackendResponse {
                    lines: vec![format!("LSP {server_id} stopped={stopped}")],
                    ..BackendResponse::default()
                },
                Err(error) => Self::response_error(error),
            },
            LspCommand::Symbols { server_id, path } => {
                match self.application.lsp_symbols(&server_id, Path::new(&path)) {
                    Ok(symbols) => BackendResponse {
                        lines: if symbols.is_empty() {
                            vec!["LSP symbols: no results".to_owned()]
                        } else {
                            symbols
                                .into_iter()
                                .map(|symbol| {
                                    let (line, character, _) = Self::lsp_display_start(
                                        &symbol.location.range,
                                        symbol.location.human_range.as_ref(),
                                    );
                                    format!(
                                        "{}:{line}:{character} · kind={} · {}{}",
                                        symbol.location.path.as_deref().unwrap_or("<external>"),
                                        symbol.kind,
                                        symbol.name,
                                        symbol
                                            .container_name
                                            .as_deref()
                                            .map_or_else(String::new, |name| format!(
                                                " · in {name}"
                                            ))
                                    )
                                })
                                .collect()
                        },
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                }
            }
            LspCommand::Definition {
                server_id,
                path,
                line,
                character,
            } => Self::lsp_location_response(self.application.lsp_definition(
                &server_id,
                Path::new(&path),
                line,
                character,
            )),
            LspCommand::References {
                server_id,
                path,
                line,
                character,
            } => Self::lsp_location_response(self.application.lsp_references(
                &server_id,
                Path::new(&path),
                line,
                character,
            )),
            LspCommand::Diagnostics { server_id, path } => {
                match self
                    .application
                    .lsp_diagnostics(&server_id, Path::new(&path))
                {
                    Ok(diagnostics) => BackendResponse {
                        lines: if diagnostics.is_empty() {
                            vec!["LSP diagnostics: clean or not published".to_owned()]
                        } else {
                            diagnostics
                                .into_iter()
                                .map(|diagnostic| {
                                    let (line, character, _) = Self::lsp_display_start(
                                        &diagnostic.range,
                                        diagnostic.human_range.as_ref(),
                                    );
                                    format!(
                                        "{}:{line}:{character} · severity={} · {}{}",
                                        diagnostic.path.as_deref().unwrap_or("<unknown>"),
                                        diagnostic.severity.unwrap_or(0),
                                        diagnostic.message,
                                        diagnostic
                                            .code
                                            .as_deref()
                                            .map_or_else(String::new, |code| format!(" · {code}"))
                                    )
                                })
                                .collect()
                        },
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                }
            }
            LspCommand::RenamePreview {
                server_id,
                path,
                line,
                character,
                new_name,
            } => self.lsp_patch_tool_response(
                "lsp.rename.preview",
                serde_json::json!({
                    "serverId":server_id,"path":path,"line":line,
                    "character":character,"newName":new_name
                }),
            ),
            LspCommand::CodeActionPreview {
                server_id,
                path,
                start_line,
                start_character,
                end_line,
                end_character,
                action_index,
                only,
            } => {
                let mut args = serde_json::json!({
                    "serverId":server_id,"path":path,
                    "startLine":start_line,"startCharacter":start_character,
                    "endLine":end_line,"endCharacter":end_character,
                    "actionIndex":action_index
                });
                if let Some(only) = only {
                    args["only"] = serde_json::Value::String(only);
                }
                self.lsp_patch_tool_response("lsp.code-action.preview", args)
            }
            LspCommand::ApplyPreview { preview_id } => self.lsp_patch_tool_response(
                "lsp.patch.apply",
                serde_json::json!({"previewId":preview_id}),
            ),
            LspCommand::UndoPreview { preview_id } => self.lsp_patch_tool_response(
                "lsp.patch.undo",
                serde_json::json!({"previewId":preview_id}),
            ),
        }
    }

    fn plan_lines(plan: &PlanView) -> Vec<String> {
        let Some(mission_id) = &plan.mission_id else {
            return vec!["Plan: no active mission".to_owned()];
        };
        let status = plan.status.map_or_else(
            || "unknown".to_owned(),
            |status| format!("{status:?}").to_lowercase(),
        );
        vec![format!(
            "Plan {} · {} · {} accepted · {} running · {} pending · {} blocked",
            mission_id, status, plan.accepted, plan.running, plan.pending, plan.blocked
        )]
    }

    fn context_lines(context: &ContextView) -> Vec<String> {
        vec![
            format!("Context  {}", context.series_id),
            format!(
                "Budget   {}/{} tokens · {}%",
                context.used_tokens, context.max_tokens, context.percent
            ),
            format!(
                "Items    {} total · {} selected · {} excluded",
                context.item_count, context.selected_count, context.excluded_count
            ),
            format!("Recovery {} checkpoint(s)", context.checkpoint_count),
        ]
    }

    fn cache_lines(cache: &CacheView) -> Vec<String> {
        let rate = cache
            .effective_hit_rate_percent
            .map_or_else(|| "n/a".to_owned(), |value| format!("{value}%"));
        let mut lines = vec![
            format!("Cache hit rate {rate}"),
            format!(
                "L1  hit={} miss={} write={} evict={} reject={}",
                cache.l1.hits,
                cache.l1.misses,
                cache.l1.writes,
                cache.l1.evictions,
                cache.l1.rejected_writes
            ),
        ];
        if let Some(l2) = cache.l2 {
            lines.push(format!(
                "L2  hit={} miss={} write={} evict={} reject={}",
                l2.hits, l2.misses, l2.writes, l2.evictions, l2.rejected_writes
            ));
        }
        lines
    }

    fn model_lines(model: &ModelView) -> Vec<String> {
        if is_hidden_internal_model(&model.provider_id, &model.model_id) {
            return vec![
                "Model      未配置".to_owned(),
                "下一步     /connect 选择 Provider，然后 /model 选择真实模型".to_owned(),
            ];
        }
        vec![
            format!("Model      {}/{}", model.provider_id, model.model_id),
            format!(
                "Reasoning  {:?} → {} ({:?})",
                model.reasoning_requested,
                model
                    .reasoning_effective
                    .map_or_else(|| "unsupported".to_owned(), |level| format!("{level:?}")),
                model.reasoning_mapping
            ),
            format!(
                "Limits     context={} output={}",
                model.context_window_tokens, model.max_output_tokens
            ),
            format!(
                "Capability tools={} structured={}",
                model.tool_calling, model.structured_output
            ),
        ]
    }

    fn account_lines(account: &AuthView) -> Vec<String> {
        vec![
            format!("Provider  {}", account.provider_id),
            format!("Auth      {}", account.auth_method),
            format!("Configured {}", account.configured),
            format!("Storage   {}", account.storage),
        ]
    }

    fn agent_lines(agent: &AgentView) -> Vec<String> {
        vec![
            format!("Agent     {} · {}", agent.id, agent.name),
            format!(
                "State     {:?} · active {}/{}",
                agent.lifecycle, agent.active, agent.max_concurrency
            ),
            format!("Roles     {:?}", agent.roles),
            format!("Capability {}", agent.capabilities.join(", ")),
            format!(
                "Boundary  {}",
                if agent.control_plane {
                    "control-plane / 禁止编码"
                } else {
                    "worker"
                }
            ),
        ]
    }

    fn team_lines(team: &AgentTeamView, mode: AgentDisplayMode) -> Vec<String> {
        if mode == AgentDisplayMode::Compact {
            return vec![
                team.agents
                    .iter()
                    .map(|agent| format!("{}:{:?}", agent.id, agent.lifecycle))
                    .collect::<Vec<_>>()
                    .join(" │ "),
            ];
        }
        if mode == AgentDisplayMode::Tree {
            let mut lines = vec!["◆ Kernary Kernel".to_owned(), "├─ Control Plane".to_owned()];
            lines.extend(
                team.agents
                    .iter()
                    .filter(|agent| agent.control_plane)
                    .map(|agent| format!("│  ├─ {:?} {}", agent.lifecycle, agent.id)),
            );
            lines.push("└─ Worker Pool".to_owned());
            lines.extend(
                team.agents
                    .iter()
                    .filter(|agent| !agent.control_plane)
                    .map(|agent| {
                        format!("   ├─ {:?} {} {:?}", agent.lifecycle, agent.id, agent.roles)
                    }),
            );
            return lines;
        }
        let mut lines = vec![format!(
            "Team {} · sleeping {} · reserved {} · running {} · messages={} · leases={} · controls={} · recoverable={}",
            team.total,
            team.sleeping,
            team.reserved,
            team.running,
            team.durable_messages,
            team.file_leases,
            team.active_run_controls,
            team.recoverable_sessions
        )];
        lines.extend(team.agents.iter().map(|agent| match mode {
            AgentDisplayMode::Verbose => format!(
                "{:?}  {} · {:?} · {}/{} · capabilities=[{}] · {}",
                agent.lifecycle,
                agent.id,
                agent.roles,
                agent.active,
                agent.max_concurrency,
                agent.capabilities.join(","),
                if agent.control_plane {
                    "control-plane"
                } else {
                    "worker"
                }
            ),
            AgentDisplayMode::Normal => format!(
                "{:?}  {} · {:?} · {}/{}{}",
                agent.lifecycle,
                agent.id,
                agent.roles,
                agent.active,
                agent.max_concurrency,
                if agent.control_plane {
                    " · control-plane"
                } else {
                    ""
                }
            ),
            AgentDisplayMode::Compact | AgentDisplayMode::Tree => unreachable!(),
        }));
        lines
    }

    fn queue_lines(queue: &AgentQueueView) -> Vec<String> {
        let Some(mission_id) = &queue.mission_id else {
            return vec!["Agent queue: no active mission".to_owned()];
        };
        let mut lines = vec![format!(
            "Agent queue {} · {} item(s)",
            mission_id,
            queue.items.len()
        )];
        lines.extend(queue.items.iter().map(|item| {
            format!(
                "{:?}{}  {} → {} · priority={} · {}",
                item.status,
                if item.ready { " [ready]" } else { "" },
                item.task_id,
                item.agent_definition_id,
                item.priority,
                item.title
            )
        }));
        lines
    }

    fn budget_lines(budget: &AgentBudgetView) -> Vec<String> {
        vec![
            format!("Agent budget · scope={}", budget.scope),
            format!(
                "Agents     total={} parallel={}",
                budget.max_agents, budget.max_parallel_agents
            ),
            format!(
                "Usage cap  tokens={} tools={} runtime={}ms retries={} costUnits={}",
                budget.max_total_tokens,
                budget.max_tool_calls,
                budget.max_runtime_millis,
                budget.max_retries,
                budget.max_cost_units
            ),
        ]
    }

    fn browser_lines(browser: &BrowserCapabilityView) -> Vec<String> {
        let Some(runtime) = &browser.runtime else {
            return vec![
                "Browser unavailable · no process started".to_owned(),
                "Configure HARNESS_BROWSER_PYTHON, HARNESS_BROWSER_EXECUTABLE, HARNESS_BROWSER_ALLOWED_ORIGINS"
                    .to_owned(),
            ];
        };
        vec![
            format!(
                "Browser {} · {:?} · alive={}",
                runtime.session_id, runtime.status, runtime.adapter_alive
            ),
            format!(
                "Origin {} · snapshot generation {} · actions {}",
                runtime.current_origin.as_deref().unwrap_or("<none>"),
                runtime.snapshot_generation,
                runtime.action_count
            ),
        ]
    }

    fn start_background_team(&mut self, count: usize, objective: &str) -> BackendResponse {
        if self.background_team.is_some() {
            return Self::response_error("已有 Agent Team 正在运行");
        }
        let prepared = match self
            .application
            .prepare_parallel_agent_team(objective, count)
        {
            Ok(prepared) => prepared,
            Err(error) => return Self::response_error(error),
        };
        self.launch_background_team(prepared, format!("agents={count}"))
    }

    fn launch_background_team(
        &mut self,
        prepared: PreparedAgentTeam,
        detail: String,
    ) -> BackendResponse {
        if self.background_team.is_some() {
            return Self::response_error("已有 Agent Team 正在运行");
        }
        let mission_id = prepared.continuation.mission_id().clone();
        let cancellations = prepared.job.cancellation_controls().into_iter().collect();
        let steering = prepared.job.steering_buffer();
        let (sender, receiver) = mpsc::sync_channel(1);
        let job = prepared.job;
        thread::spawn(move || {
            let _ = sender.send(job.execute());
        });
        self.background_team = Some(BackgroundTeam {
            receiver,
            continuation: prepared.continuation,
            cancellations,
            steering,
            cancellation_requested: false,
            cancelled_tasks: BTreeSet::new(),
        });
        BackendResponse {
            lines: vec![format!(
                "Background Team started · mission={} · {detail}",
                mission_id,
            )],
            ..BackendResponse::default()
        }
    }

    fn poll_background_team(&mut self) -> BackendResponse {
        enum PollResult {
            Ready(Result<Vec<AgentExecutionOutcome>, ApplicationError>),
            Disconnected,
        }
        let poll = match self.background_team.as_ref() {
            None => return BackendResponse::default(),
            Some(background) => match background.receiver.try_recv() {
                Ok(result) => PollResult::Ready(result),
                Err(TryRecvError::Empty) => return BackendResponse::default(),
                Err(TryRecvError::Disconnected) => PollResult::Disconnected,
            },
        };
        let background = self.background_team.take().expect("刚检查后台 Team");
        match poll {
            PollResult::Ready(Ok(outcomes)) => match self
                .application
                .finalize_parallel_agent_team_step(
                    background.continuation,
                    outcomes,
                    &background.cancelled_tasks,
                    background.cancellation_requested,
                ) {
                Ok(step) => {
                    let plan = step.plan;
                    if let Some(prepared) = step.next {
                        let mut response = self
                            .launch_background_team(prepared, "tool-continuation=true".to_owned());
                        response
                            .lines
                            .insert(0, "Background Tool wave finished".to_owned());
                        response.lines.splice(1..1, Self::plan_lines(&plan));
                        return response;
                    }
                    if !background.cancellation_requested
                        && plan.running == 0
                        && plan.pending > 0
                        && let Some(mission_id) = plan.mission_id.clone()
                    {
                        match self.application.prepare_next_agent_wave(&mission_id) {
                            Ok(prepared) => {
                                let mut response = self
                                    .launch_background_team(prepared, "next-wave=true".to_owned());
                                response
                                    .lines
                                    .insert(0, "Background Agent wave finished".to_owned());
                                response.lines.splice(1..1, Self::plan_lines(&plan));
                                return response;
                            }
                            Err(error) if error.code == "agent-wave-not-ready" => {}
                            Err(error) => return Self::response_error(error),
                        }
                    }
                    let mut lines = vec![if plan.blocked > 0 {
                        "Background Team paused · approval or reconciliation required".to_owned()
                    } else {
                        "Background Team finished".to_owned()
                    }];
                    lines.extend(Self::plan_lines(&plan));
                    BackendResponse {
                        lines,
                        ..BackendResponse::default()
                    }
                }
                Err(error) => Self::response_error(error),
            },
            PollResult::Ready(Err(error)) => {
                let cleanup = self.application.finalize_parallel_agent_team(
                    background.continuation,
                    vec![],
                    &background.cancelled_tasks,
                    true,
                );
                let mut response = Self::response_error(error);
                if let Err(cleanup_error) = cleanup {
                    response
                        .lines
                        .push(format!("! background cleanup: {cleanup_error}"));
                }
                response
            }
            PollResult::Disconnected => {
                let cleanup = self.application.finalize_parallel_agent_team(
                    background.continuation,
                    vec![],
                    &background.cancelled_tasks,
                    true,
                );
                let mut response = Self::response_error("Background Agent worker disconnected");
                if let Err(cleanup_error) = cleanup {
                    response
                        .lines
                        .push(format!("! background cleanup: {cleanup_error}"));
                }
                response
            }
        }
    }

    fn tool_lines(view: &ToolRuntimeView) -> Vec<String> {
        let mut lines = vec![format!(
            "Tools {} · pending approvals {} · active grants {}",
            view.tools.len(),
            view.pending_approvals,
            view.active_grants
        )];
        lines.extend(view.tools.iter().map(|tool| {
            format!(
                "{}@{} · {:?}",
                tool.canonical_name, tool.version, tool.effect_class
            )
        }));
        lines
    }

    fn run_process_tool(&self, executable_env: &str, arguments: Vec<String>) -> BackendResponse {
        let Some(executable) = compat_env_var_os(executable_env) else {
            return Self::response_error(format!("未配置 {executable_env}"));
        };
        let executable = PathBuf::from(executable).display().to_string();
        match self.application.invoke_tool(
            "process.exec",
            serde_json::json!({
                "executable":executable,
                "arguments":arguments,
                "cwd":self.project_root,
                "timeoutMs":300_000
            }),
        ) {
            Ok(invocation) if invocation.status == ToolInvocationStatus::Completed => {
                let result = invocation.result.unwrap_or_default();
                let mut lines = Vec::new();
                if let Some(stdout) = result.get("stdout").and_then(serde_json::Value::as_str)
                    && !stdout.is_empty()
                {
                    lines.extend(stdout.lines().map(str::to_owned));
                }
                if let Some(stderr) = result.get("stderr").and_then(serde_json::Value::as_str)
                    && !stderr.is_empty()
                {
                    lines.extend(stderr.lines().map(|line| format!("stderr: {line}")));
                }
                if lines.is_empty() {
                    lines.push(format!("Tool {} completed", invocation.id));
                }
                BackendResponse {
                    lines,
                    ..BackendResponse::default()
                }
            }
            Ok(invocation) if invocation.status == ToolInvocationStatus::WaitingApproval => {
                BackendResponse {
                    lines: vec![format!(
                        "Approval required: /approve {} once",
                        invocation.id
                    )],
                    ..BackendResponse::default()
                }
            }
            Ok(invocation) => Self::response_error(format!(
                "Tool {} ended {:?}: {}",
                invocation.id,
                invocation.status,
                invocation.error.as_deref().unwrap_or("no detail")
            )),
            Err(error) => Self::response_error(error),
        }
    }
}

impl TerminalBackend for AppBackend {
    fn handle_input(&mut self, input: &str) -> BackendResponse {
        self.drain_event_log();
        let parsed = match self.registry.parse(input) {
            Ok(parsed) => parsed,
            Err(error) => return Self::response_error(error),
        };
        match parsed {
            ParsedInput::Text(_) if !self.model_ready => self.model_not_configured_response(),
            ParsedInput::Text(text) if self.background_team.is_some() => {
                match self.application.steer(&text) {
                    Ok(steering) => {
                        if let Some(background) = &self.background_team
                            && let Err(error) = background.steering.push(&text)
                        {
                            return Self::response_error(error);
                        }
                        BackendResponse {
                            lines: vec![format!(
                                "Steering {} · queued for active Team",
                                steering.message_id
                            )],
                            ..BackendResponse::default()
                        }
                    }
                    Err(error) => Self::response_error(error),
                }
            }
            ParsedInput::Text(text) => match self.application.run_fake_task(&text) {
                Ok(plan) => BackendResponse {
                    lines: Self::plan_lines(&plan),
                    ..BackendResponse::default()
                },
                Err(error) => Self::response_error(error),
            },
            ParsedInput::Command(command)
                if !self.model_ready && slash_requires_ready_model(&command) =>
            {
                self.model_not_configured_response()
            }
            ParsedInput::Command(command) => match command {
                SlashCommand::Account => match self.application.account() {
                    Ok(account) => BackendResponse {
                        lines: Self::account_lines(&account),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::AgentShow { agent_id } => match self
                    .application
                    .agent(&harness_types::AgentDefinitionId::from(agent_id))
                {
                    Ok(agent) => BackendResponse {
                        lines: Self::agent_lines(&agent),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Agents { mode } => match self.application.agents() {
                    Ok(team) => BackendResponse {
                        lines: Self::team_lines(&team, mode),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Team { operation } => match operation {
                    TeamCommand::Status => match self.application.agents() {
                        Ok(team) => BackendResponse {
                            lines: Self::team_lines(&team, AgentDisplayMode::Normal),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    TeamCommand::Create { count, objective } => {
                        let objective = objective.or_else(|| {
                            self.application.status().ok().and_then(|status| status.goal)
                        });
                        let Some(objective) = objective else {
                            return Self::response_error(
                                "Team objective 为空；请提供 objective 或先设置 /goal",
                            );
                        };
                        self.start_background_team(count, &objective)
                    }
                    TeamCommand::Workflow { workers, objective } => {
                        let objective = objective.or_else(|| {
                            self.application.status().ok().and_then(|status| status.goal)
                        });
                        let Some(objective) = objective else {
                            return Self::response_error(
                                "Workflow objective 为空；请提供 objective 或先设置 /goal",
                            );
                        };
                        if self.background_team.is_some() {
                            Self::response_error("已有 Agent Team 正在运行")
                        } else {
                            match self
                                .application
                                .prepare_role_evidence_team(&objective, workers)
                            {
                                Ok(prepared) => self.launch_background_team(
                                    prepared,
                                    format!("workflow-workers={workers}"),
                                ),
                                Err(error) => Self::response_error(error),
                            }
                        }
                    }
                },
                SlashCommand::Budget { operation } => {
                    let result = match operation {
                        BudgetCommand::Show => Ok(self.application.agent_budget()),
                        BudgetCommand::Set { field, value } => {
                            self.application.set_agent_budget(&field, value)
                        }
                    };
                    match result {
                        Ok(budget) => BackendResponse {
                            lines: Self::budget_lines(&budget),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Browser { operation } => match operation {
                    BrowserCliCommand::Status => match self.application.browser_view() {
                        Ok(view) => BackendResponse {
                            lines: Self::browser_lines(&view),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    BrowserCliCommand::Open => match self.application.open_browser() {
                        Ok(view) => BackendResponse {
                            lines: Self::browser_lines(&view),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    BrowserCliCommand::Navigate { url } => {
                        match self.application.navigate_browser(&url) {
                            Ok(_) => match self.application.browser_view() {
                                Ok(view) => BackendResponse {
                                    lines: Self::browser_lines(&view),
                                    ..BackendResponse::default()
                                },
                                Err(error) => Self::response_error(error),
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    BrowserCliCommand::Actions => match self.application.browser_actions() {
                        Ok(actions) => BackendResponse {
                            lines: actions
                                .into_iter()
                                .map(|action| {
                                    format!(
                                        "{} · {:?} · {:?} · {}",
                                        action.sequence,
                                        action.action,
                                        action.status,
                                        action.target.as_deref().unwrap_or("")
                                    )
                                })
                                .collect(),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    BrowserCliCommand::Handoff => match self.application.handoff_browser() {
                        Ok(view) => BackendResponse {
                            lines: Self::browser_lines(&view),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    BrowserCliCommand::Reclaim => match self.application.reclaim_browser() {
                        Ok(view) => BackendResponse {
                            lines: Self::browser_lines(&view),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    BrowserCliCommand::Close => match self.application.close_browser() {
                        Ok(view) => BackendResponse {
                            lines: Self::browser_lines(&view),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                },
                SlashCommand::Approve {
                    invocation_id,
                    scope,
                } => match self
                    .application
                    .approve_tool(ToolInvocationId::from(invocation_id), scope)
                {
                    Ok(invocation) => {
                        let line = format!("Tool {} · {:?}", invocation.id, invocation.status);
                        if let Some(prepared) = self.application.take_ready_parallel_resume() {
                            let mut response = self.launch_background_team(
                                prepared,
                                "approved-tool-continuation=true".to_owned(),
                            );
                            response.lines.insert(0, line);
                            response
                        } else {
                            BackendResponse {
                                lines: vec![line],
                                ..BackendResponse::default()
                            }
                        }
                    }
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Cache => BackendResponse {
                    lines: Self::cache_lines(&self.application.cache()),
                    ..BackendResponse::default()
                },
                SlashCommand::Checkpoint { name } => {
                    match self.application.create_checkpoint(name.as_deref()) {
                        Ok(checkpoint) => BackendResponse {
                            lines: vec![format!(
                                "Checkpoint {} · series {} · durable",
                                checkpoint.checkpoint_id, checkpoint.context_series_id
                            )],
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Compact { mode } => {
                    if mode == CompactCommandMode::Auto {
                        return match self.application.auto_compact_if_needed() {
                            Ok(Some(compaction)) => BackendResponse {
                                lines: vec![format!(
                                    "Auto compacted · {} → {} tokens · checkpoint {}",
                                    compaction.token_cost_before,
                                    compaction.token_cost_after,
                                    compaction.checkpoint_id
                                )],
                                ..BackendResponse::default()
                            },
                            Ok(None) => match self.application.context() {
                                Ok(context) => BackendResponse {
                                    lines: vec![format!(
                                        "Auto compact active · threshold 80% · current {}%",
                                        context.percent
                                    )],
                                    ..BackendResponse::default()
                                },
                                Err(error) => Self::response_error(error),
                            },
                            Err(error) => Self::response_error(error),
                        };
                    }
                    let mode = match mode {
                        CompactCommandMode::Auto => unreachable!("auto 已提前处理"),
                        CompactCommandMode::Safe => CompactionMode::Safe,
                        CompactCommandMode::Aggressive => CompactionMode::Aggressive,
                    };
                    match self.application.compact(mode) {
                        Ok(compaction) => BackendResponse {
                            lines: vec![format!(
                                "Context compacted {:?} · {} → {} tokens · checkpoint {}",
                                compaction.mode,
                                compaction.token_cost_before,
                                compaction.token_cost_after,
                                compaction.checkpoint_id
                            )],
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Config => BackendResponse {
                    lines: Self::config_lines(&self.application.config(), None, true),
                    ..BackendResponse::default()
                },
                SlashCommand::Context => match self.application.context() {
                    Ok(context) => BackendResponse {
                        lines: Self::context_lines(&context),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::DenyTool { invocation_id } => match self
                    .application
                    .deny_tool(ToolInvocationId::from(invocation_id))
                {
                    Ok(invocation) => BackendResponse {
                        lines: vec![format!("Tool {} · {:?}", invocation.id, invocation.status)],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Debug => BackendResponse {
                    lines: self.debug_lines(),
                    ..BackendResponse::default()
                },
                SlashCommand::Diff => self.run_process_tool(
                    "HARNESS_GIT_EXECUTABLE",
                    vec!["diff".to_owned(), "--".to_owned()],
                ),
                SlashCommand::Doctor => BackendResponse {
                    lines: self.doctor_lines(),
                    ..BackendResponse::default()
                },
                SlashCommand::Focus { value } => match self.application.focus(value.as_deref()) {
                    Ok(context) => BackendResponse {
                        lines: Self::context_lines(&context),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Failover { operation } => {
                    let result = match operation {
                        FailoverCommand::Status => Ok(self.application.failover()),
                        FailoverCommand::Off => {
                            self.application.configure_failover(false, false, &[])
                        }
                        FailoverCommand::On { targets } => {
                            self.application.configure_failover(true, true, &targets)
                        }
                    };
                    match result {
                        Ok(view) => {
                            let mut lines = Self::config_lines(
                                &self.application.config(),
                                Some("failover"),
                                false,
                            );
                            lines.push(format!(
                                "Failover enabled={} costConfirmed={} targets={}",
                                view.enabled,
                                view.cost_confirmed,
                                if view.targets.is_empty() {
                                    "<none>".to_owned()
                                } else {
                                    view.targets.join(",")
                                }
                            ));
                            BackendResponse {
                                lines,
                                ..BackendResponse::default()
                            }
                        }
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Forget { id } => match self.application.forget_memory(&id) {
                    Ok(deleted) => BackendResponse {
                        lines: vec![format!("Memory {id} deleted={deleted}")],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Fork {
                    checkpoint_id,
                    child_session_id,
                } => match self
                    .application
                    .fork_session_reference(&checkpoint_id, child_session_id.map(SessionId::from))
                {
                    Ok(fork) => BackendResponse {
                        lines: vec![format!(
                            "Forked child {} · parent {} unchanged · context {}",
                            fork.child_session_id,
                            fork.parent_session_id,
                            fork.child_context_series_id
                        )],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Git { operation } => {
                    let arguments = match operation {
                        GitCommand::Status => vec![
                            "status".to_owned(),
                            "--short".to_owned(),
                            "--branch".to_owned(),
                        ],
                        GitCommand::Diff => vec!["diff".to_owned(), "--".to_owned()],
                        GitCommand::Log => vec![
                            "log".to_owned(),
                            "-n".to_owned(),
                            "20".to_owned(),
                            "--oneline".to_owned(),
                            "--decorate".to_owned(),
                        ],
                        GitCommand::Branch => {
                            vec!["branch".to_owned(), "--show-current".to_owned()]
                        }
                    };
                    self.run_process_tool("HARNESS_GIT_EXECUTABLE", arguments)
                }
                SlashCommand::Help { command } => BackendResponse {
                    lines: self.registry.help(command.as_deref()),
                    ..BackendResponse::default()
                },
                SlashCommand::Connect { provider } => self.begin_provider_connect(provider),
                SlashCommand::Lsp { operation } => self.handle_lsp(operation),
                SlashCommand::Memory { operation } => match operation {
                    MemoryCommand::Stats => match self.application.memory_view(){Ok(view)=>BackendResponse{lines:vec![format!("Memory records={} fts={} semantic={:?} vectorSchema={}",view.record_count,view.fts_indexed_count,view.semantic,view.vector_schema_present)],..Default::default()},Err(error)=>Self::response_error(error)},
                    MemoryCommand::Search{mode,query}=>{let mode=match mode.as_str(){"metadata"=>RetrievalMode::Metadata,"lexical"=>RetrievalMode::Lexical,"semantic"=>RetrievalMode::Semantic,"hybrid"=>RetrievalMode::Hybrid,"auto"=>RetrievalMode::Auto,_=>return Self::response_error("Memory mode 仅支持 metadata/lexical/semantic/hybrid/auto")};match self.application.search_memory(&query,mode,8){Ok(response)=>BackendResponse{lines:std::iter::once(format!("Memory {:?} -> {:?} degraded={}",response.requested_mode,response.executed_mode,response.degraded)).chain(response.results.into_iter().map(|result|format!("{} · {} · {:.4} · {}",result.record.id,result.record.title,result.score,result.matched_by))).collect(),..Default::default()},Err(error)=>Self::response_error(error)}},
                    MemoryCommand::Add{kind,title,content,tags}=>{let kind=match kind.as_str(){"architecture"=>MemoryKind::Architecture,"decision"=>MemoryKind::Decision,"contract"=>MemoryKind::Contract,"lesson"=>MemoryKind::Lesson,"failure"=>MemoryKind::Failure,"verification"=>MemoryKind::Verification,"meeting"=>MemoryKind::Meeting,_=>return Self::response_error("Memory kind 无效")};match self.application.add_memory(kind,title,content,tags){Ok(record)=>BackendResponse{lines:vec![format!("Memory added {} · {}",record.id,record.title)],..Default::default()},Err(error)=>Self::response_error(error)}},
                    MemoryCommand::Forget{id}=>match self.application.forget_memory(&id){Ok(deleted)=>BackendResponse{lines:vec![format!("Memory {id} deleted={deleted}")],..Default::default()},Err(error)=>Self::response_error(error)},
                },
                SlashCommand::Index { operation } => match operation {
                    IndexCommand::Status=>match self.application.repository_view(){Ok(view)=>BackendResponse{lines:vec![format!("Repository files={} symbols={} imports={} lspSymbols={} lspDiagnostics={} revision={}",view.file_count,view.symbol_count,view.import_count,view.lsp_symbol_count,view.lsp_diagnostic_count,view.revision)],..Default::default()},Err(error)=>Self::response_error(error)},
                    IndexCommand::Update=>match self.application.update_repository(){Ok(stats)=>BackendResponse{lines:vec![format!("Index discovered={} indexed={} unchangedMetadata={} unchangedContent={} deleted={} skipped={}",stats.discovered,stats.indexed,stats.unchanged_metadata,stats.unchanged_content,stats.deleted,stats.skipped)],..Default::default()},Err(error)=>Self::response_error(error)},
                    IndexCommand::Clear=>match self.application.clear_repository(){Ok(view)=>BackendResponse{lines:vec![format!("Repository index cleared · revision {}",view.revision)],..Default::default()},Err(error)=>Self::response_error(error)},
                    IndexCommand::Map=>match self.application.repository_map(){Ok(map)=>BackendResponse{lines:map.lines().map(str::to_owned).collect(),..Default::default()},Err(error)=>Self::response_error(error)},
                    IndexCommand::Search{query}=>match self.application.search_repository(&query,8){Ok(results)=>BackendResponse{lines:results.into_iter().map(|result|format!("{} · {} · {:.4} · {} · symbols={} · diagnostics={}",result.path,result.language,result.score,result.matched_by,result.symbols.join(","),result.diagnostics.join(" | "))).collect(),..Default::default()},Err(error)=>Self::response_error(error)},
                },
                SlashCommand::Inspect { target } => self.inspect(&target),
                SlashCommand::Mcp { operation } => match operation {
                    McpCommand::List => match self.application.mcp_servers() {
                        Ok(servers) => BackendResponse {
                            lines: if servers.is_empty() {
                                vec!["No configured MCP servers".to_owned()]
                            } else {
                                servers
                                    .into_iter()
                                    .map(|server| {
                                        format!(
                                            "{} · {:?} · {} · enabled={} · auth {} · tools {}/{} · resources {} · prompts {}{}",
                                            server.id,
                                            server.status,
                                            server.transport,
                                            server.enabled,
                                            server.authorization,
                                            server.supported_tool_count,
                                            server.tool_count,
                                            server.resource_count,
                                            server.prompt_count,
                                            server.last_error.map_or_else(String::new, |error| {
                                                format!(" · {error}")
                                            })
                                        )
                                    })
                                    .collect()
                            },
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    McpCommand::AddStdio {
                        server_id,
                        command,
                        args,
                    } => self.add_mcp_server(McpServerConfig {
                        id: server_id.clone(),
                        name: server_id,
                        enabled: true,
                        trust_annotations: false,
                        transport: McpTransportConfig::Stdio(McpStdioConfig {
                            command: PathBuf::from(command),
                            args,
                            cwd: None,
                            inherit_env: Vec::new(),
                            request_timeout_millis: Some(10_000),
                            max_message_bytes: Some(1024 * 1024),
                        }),
                    }),
                    McpCommand::AddHttp {
                        server_id,
                        endpoint,
                    } => self.add_mcp_server(McpServerConfig {
                        id: server_id.clone(),
                        name: server_id,
                        enabled: true,
                        trust_annotations: false,
                        transport: McpTransportConfig::StreamableHttp(
                            McpStreamableHttpConfig {
                                endpoint,
                                bearer_credential_id: None,
                                oauth: None,
                                legacy_sse_fallback: false,
                                request_timeout_millis: Some(30_000),
                                max_response_bytes: Some(4 * 1024 * 1024),
                            },
                        ),
                    }),
                    McpCommand::Remove { server_id } => self.remove_mcp_server(&server_id),
                    McpCommand::Enable { server_id } => self.set_mcp_enabled(&server_id, true),
                    McpCommand::Disable { server_id } => self.set_mcp_enabled(&server_id, false),
                    McpCommand::AuthStart { server_id } => {
                        match self.application.mcp_oauth_start(&server_id) {
                            Ok(started) => BackendResponse {
                                lines: vec![
                                    format!("Open this URL in your browser: {}", started.authorization_url),
                                    format!("Callback: {}", started.redirect_uri),
                                    format!("Then run: /mcp auth finish {}", started.server_id),
                                ],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::AuthFinish { server_id } => {
                        match self.application.mcp_oauth_finish(&server_id) {
                            Ok(status) => BackendResponse {
                                lines: vec![format!(
                                    "MCP OAuth {} · authenticated={} · pending={}",
                                    status.server_id, status.authenticated, status.pending
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::AuthRefresh { server_id } => {
                        match self.application.mcp_oauth_refresh(&server_id) {
                            Ok(status) => BackendResponse {
                                lines: vec![format!(
                                    "MCP OAuth {} · refreshed={}",
                                    status.server_id, status.authenticated
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::AuthStatus { server_id } => {
                        match self.application.mcp_oauth_status(&server_id) {
                            Ok(status) => BackendResponse {
                                lines: vec![format!(
                                    "MCP OAuth {} · configured={} · authenticated={} · pending={}",
                                    status.server_id,
                                    status.configured,
                                    status.authenticated,
                                    status.pending
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Connect { server_id, force } => {
                        match self.application.mcp_connect(&server_id, force) {
                            Ok(server) => BackendResponse {
                                lines: vec![format!(
                                    "MCP {} · {:?} · protocol {} · tools {}/{}",
                                    server.id,
                                    server.status,
                                    server.protocol_version.as_deref().unwrap_or("n/a"),
                                    server.supported_tool_count,
                                    server.tool_count
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Disconnect { server_id } => {
                        match self.application.mcp_disconnect(&server_id) {
                            Ok(server) => BackendResponse {
                                lines: vec![format!("MCP {} · {:?}", server.id, server.status)],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Tools { server_id } => {
                        match self.application.mcp_tools(&server_id) {
                            Ok(tools) => BackendResponse {
                                lines: tools
                                    .into_iter()
                                    .map(|tool| {
                                        format!(
                                            "{} · task={:?} · readOnly={:?} · {}",
                                            tool.name,
                                            tool.task_support,
                                            tool.annotations.read_only_hint,
                                            tool.description.unwrap_or_default()
                                        )
                                    })
                                    .collect(),
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Resources { server_id } => {
                        match self.application.mcp_resources(&server_id) {
                            Ok(resources) => BackendResponse {
                                lines: resources
                                    .into_iter()
                                    .map(|resource| {
                                        format!(
                                            "{} · {} · {}",
                                            resource.uri,
                                            resource.name,
                                            resource.mime_type.unwrap_or_else(|| "unknown".to_owned())
                                        )
                                    })
                                    .collect(),
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Prompts { server_id } => {
                        match self.application.mcp_prompts(&server_id) {
                            Ok(prompts) => BackendResponse {
                                lines: prompts
                                    .into_iter()
                                    .map(|prompt| {
                                        format!(
                                            "{} · {} args · {}",
                                            prompt.name,
                                            prompt.arguments.len(),
                                            prompt.description.unwrap_or_default()
                                        )
                                    })
                                    .collect(),
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    McpCommand::Poll { server_id } => match self.application.mcp_poll(&server_id) {
                        Ok(events) => BackendResponse {
                            lines: if events.is_empty() {
                                vec![format!("MCP {server_id} · no notifications")]
                            } else {
                                events.into_iter().map(|event| event.to_string()).collect()
                            },
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    McpCommand::Read { server_id, uri } => {
                        match self.application.mcp_read_resource(&server_id, &uri) {
                            Ok(contents) => BackendResponse {
                                lines: contents
                                    .into_iter()
                                    .map(|content| content.to_string())
                                    .collect(),
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                },
                SlashCommand::Logout { provider } if provider == "openai" => {
                    match self.application.logout("openai") {
                        Ok(deleted) => BackendResponse {
                            lines: vec![format!("OpenAI credential deleted={deleted}")],
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Logout { provider } => BackendResponse {
                    lines: vec![format!("请在独立安全流程运行：kernary logout {provider}")],
                    ..BackendResponse::default()
                },
                SlashCommand::Logs { limit } => BackendResponse {
                    lines: self.event_log_lines(limit, false),
                    ..BackendResponse::default()
                },
                SlashCommand::ModelShow => match self.application.model() {
                    Ok(model) => BackendResponse {
                        lines: Self::model_lines(&model),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::ModelSelect { provider, model } => {
                    self.select_model(provider, model)
                }
                SlashCommand::Models { refresh, provider } => {
                    let refreshed_provider = provider.clone();
                    let mut models = if refresh {
                        match self
                            .application
                            .refresh_models(provider.map(ProviderId::from))
                        {
                            Ok(models) => models,
                            Err(error) => return Self::response_error(error),
                        }
                    } else {
                        self.application.models()
                    };
                    models.retain(|model| {
                        !is_unconfigured_model(&model.provider_id, &model.model_id)
                            && (self.test_model_enabled
                                || !is_internal_test_model(&model.provider_id, &model.model_id))
                    });
                    if refresh
                        && let Some(provider) = refreshed_provider.as_deref()
                    {
                        models.retain(|model| model.provider_id.as_str() == provider);
                    }
                    let mut lines = if models.is_empty() {
                            vec!["No registered models".to_owned()]
                        } else {
                            models
                                .into_iter()
                                .map(|model| {
                                    format!(
                                        "{}/{} · context={} · tools={} · structured={}",
                                        model.provider_id,
                                        model.model_id,
                                        model.context_window_tokens,
                                        model.tool_calling,
                                        model.structured_output
                                    )
                                })
                                .collect()
                        };
                    if refresh
                        && let Some(provider) = refreshed_provider
                        && let Some(summary) = discovery_summary(
                            Path::new(&self.project_root),
                            &ProviderId::from(provider),
                        )
                    {
                        lines.insert(0, summary);
                    }
                    BackendResponse {
                        lines,
                        ..BackendResponse::default()
                    }
                }
                SlashCommand::Mode { mode } => match mode {
                    None => BackendResponse {
                        lines: Self::config_lines(&self.application.config(), Some("mode"), false),
                        ..BackendResponse::default()
                    },
                    Some(mode) => match self.application.set_setting(
                        "mode",
                        &mode,
                        ConfigLayer::Session,
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some("mode"), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                },
                SlashCommand::GoalShow => match self.application.status() {
                    Ok(status) => BackendResponse {
                        lines: vec![format!(
                            "Goal: {}{}",
                            status.goal.as_deref().unwrap_or("<empty>"),
                            if status.goal_locked { " [locked]" } else { "" }
                        )],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::GoalSet { text } => match self.application.set_goal(&text) {
                    Ok(status) => BackendResponse {
                        lines: Self::status_lines(&status),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::GoalClear => match self.application.clear_goal() {
                    Ok(status) => BackendResponse {
                        lines: Self::status_lines(&status),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::GoalHistory { limit } => match self.application.goal_history(limit) {
                    Ok(history) => BackendResponse {
                        lines: Self::goal_history_lines(&history),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::GoalLock { locked } => match self.application.set_goal_lock(locked) {
                    Ok(status) => BackendResponse {
                        lines: Self::status_lines(&status),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Plan => match self.application.plan() {
                    Ok(plan) => BackendResponse {
                        lines: Self::plan_lines(&plan),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::PatchList => match self.application.patches() {
                    Ok(patches) => BackendResponse {
                        lines: if patches.is_empty() {
                            vec!["PatchQueue empty".to_owned()]
                        } else {
                            patches
                                .into_iter()
                                .rev()
                                .map(|patch| {
                                    format!(
                                        "{} · {:?} · {}",
                                        patch.id,
                                        patch.status,
                                        patch.path.display()
                                    )
                                })
                                .collect()
                        },
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Pin { value } => match self.application.pin(&value) {
                    Ok(context) => BackendResponse {
                        lines: Self::context_lines(&context),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Plugins { operation } => match operation {
                    PluginCommand::List => match self.application.plugins() {
                        Ok(plugins) => BackendResponse {
                            lines: if plugins.is_empty() {
                                vec!["No discovered plugins".to_owned()]
                            } else {
                                plugins
                                    .into_iter()
                                    .map(|plugin| {
                                        format!(
                                            "{} · {} · {:?} · contributions {}{}",
                                            plugin.id,
                                            plugin.version,
                                            plugin.status,
                                            plugin.contribution_count,
                                            plugin.last_error.map_or_else(String::new, |error| {
                                                format!(" · {error}")
                                            })
                                        )
                                    })
                                    .collect()
                            },
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    PluginCommand::Review { plugin_id } => {
                        match self.application.plugin_review(&plugin_id) {
                            Ok(review) => BackendResponse {
                                lines: vec![
                                    format!("Plugin: {}", review.plugin_id),
                                    format!("Review hash: {}", review.manifest_hash),
                                    format!("Entry SHA-256: {}", review.entry_sha256),
                                    format!("Permissions: {}", review.permissions.join(", ")),
                                    format!("Tools: {}", review.tool_names.join(", ")),
                                    format!(
                                        "Enable: /plugins enable {} {}",
                                        review.plugin_id, review.manifest_hash
                                    ),
                                ],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    PluginCommand::Enable {
                        plugin_id,
                        review_hash,
                    } => match self.application.enable_plugin(&plugin_id, &review_hash) {
                        Ok(plugin) => BackendResponse {
                            lines: vec![format!(
                                "Plugin {} · {:?} · contributions {}",
                                plugin.id, plugin.status, plugin.contribution_count
                            )],
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    PluginCommand::Disable { plugin_id } => {
                        match self.application.disable_plugin(&plugin_id) {
                            Ok(plugin) => BackendResponse {
                                lines: vec![format!("Plugin {} · {:?}", plugin.id, plugin.status)],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                },
                SlashCommand::Permissions { operation } => match operation {
                    PermissionCommand::Show | PermissionCommand::Mode { .. } => {
                        if let PermissionCommand::Mode { mode } = operation
                            && let Err(error) = self.application.set_setting(
                                "permissions.mode",
                                &mode,
                                ConfigLayer::Session,
                            )
                        {
                            return Self::response_error(error);
                        }
                        match self.application.tools() {
                            Ok(view) => {
                                let mut lines = Self::config_lines(
                                    &self.application.config(),
                                    Some("permissions.mode"),
                                    false,
                                );
                                lines.extend([
                                    format!("Approval policy: {:?}", view.approval_policy),
                                    format!("Permission rules: {}", view.permission_rules.len()),
                                    format!("Pending approvals: {}", view.pending_approvals),
                                    format!("Active grants: {}", view.active_grants),
                                    "Sandbox hard denies and WorkspacePatch second approval remain mandatory"
                                        .to_owned(),
                                ]);
                                BackendResponse {
                                    lines,
                                    ..BackendResponse::default()
                                }
                            }
                            Err(error) => Self::response_error(error),
                        }
                    }
                    PermissionCommand::RuleList => match self.application.tools() {
                        Ok(view) => BackendResponse {
                            lines: if view.permission_rules.is_empty() {
                                vec!["No custom Permission rules".to_owned()]
                            } else {
                                view.permission_rules
                                    .into_iter()
                                    .map(|rule| {
                                        format!(
                                            "{} · {:?} {:?} {}",
                                            rule.id, rule.effect, rule.action, rule.pattern
                                        )
                                    })
                                    .collect()
                            },
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    PermissionCommand::RuleAdd {
                        effect,
                        action,
                        pattern,
                    } => self.add_permission_rule(
                        match effect.as_str() {
                            "allow" => PermissionRuleEffect::Allow,
                            "ask" => PermissionRuleEffect::Ask,
                            "deny" => PermissionRuleEffect::Deny,
                            _ => unreachable!("parser validates effect"),
                        },
                        match action.as_str() {
                            "read" => PermissionRuleAction::Read,
                            "write" => PermissionRuleAction::Write,
                            "execute" => PermissionRuleAction::Execute,
                            "network" => PermissionRuleAction::Network,
                            "browser" => PermissionRuleAction::Browser,
                            "mcp" => PermissionRuleAction::Mcp,
                            "plugin" => PermissionRuleAction::Plugin,
                            _ => unreachable!("parser validates action"),
                        },
                        pattern,
                    ),
                    PermissionCommand::RuleRemove { rule_id } => {
                        self.remove_permission_rule(&rule_id)
                    }
                },
                SlashCommand::Provider => match self.application.model() {
                    Ok(model) => BackendResponse {
                        lines: vec![if is_hidden_internal_model(
                            &model.provider_id,
                            &model.model_id,
                        ) {
                            "Provider: 未配置".to_owned()
                        } else {
                            format!("Provider: {}", model.provider_id)
                        }],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Providers => match self.provider_catalog_lines() {
                    Ok(lines) => BackendResponse {
                        lines,
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Profile => match self.application.profile() {
                    Ok(profile) => BackendResponse {
                        lines: Self::profile_lines(&profile),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Queue { operation } => {
                    let result = match operation {
                        QueueCommand::Status => self.application.agent_queue(),
                        QueueCommand::Cancel { task_id } => {
                            let task_id = TaskId::from(task_id);
                            if let Some(background) = self.background_team.as_mut() {
                                let Some(cancellation) = background.cancellations.get(&task_id)
                                else {
                                    return Self::response_error(format!(
                                        "Task 不属于当前后台 Team：{task_id}"
                                    ));
                                };
                                cancellation.cancel();
                                background.cancelled_tasks.insert(task_id.clone());
                                return BackendResponse {
                                    lines: vec![format!(
                                        "Cancellation requested for {task_id}; Kernel 终态将在 worker 回收后提交"
                                    )],
                                    ..BackendResponse::default()
                                };
                            }
                            self.application
                                .cancel_queue_task(&task_id, "user-queue-cancel")
                        }
                        QueueCommand::Priority { task_id, priority } => self
                            .application
                            .set_queue_priority(&harness_types::TaskId::from(task_id), priority),
                    };
                    match result {
                        Ok(queue) => BackendResponse {
                            lines: Self::queue_lines(&queue),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Reasoning { level } => match self.application.set_reasoning(level) {
                    Ok(model) => BackendResponse {
                        lines: Self::model_lines(&model),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Review { staged } => {
                    let mut arguments = vec!["diff".to_owned()];
                    if staged {
                        arguments.push("--cached".to_owned());
                    }
                    arguments.push("--".to_owned());
                    let response = self.run_process_tool("HARNESS_GIT_EXECUTABLE", arguments);
                    if response.lines.iter().any(|line| line.starts_with('!')) {
                        return response;
                    }
                    let input = response.lines.join("\n");
                    match self.application.prepare_review_agent(&input) {
                        Ok(prepared) => {
                            let mut response = self.launch_background_team(
                                prepared,
                                format!(
                                    "review={}",
                                    if staged { "staged" } else { "unstaged" }
                                ),
                            );
                            response.lines.insert(0, format!(
                                "Review Agent created from {} diff",
                                if staged { "staged" } else { "unstaged" }
                            ));
                            response
                        }
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::Rollback { checkpoint_id } => {
                    match self.application.rollback_reference(&checkpoint_id) {
                        Ok(context) => BackendResponse {
                            lines: vec![format!(
                                "Rolled back into new series {}",
                                context.series_id
                            )],
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    }
                }
                SlashCommand::RetryTool { invocation_id } => match self
                    .application
                    .retry_tool(ToolInvocationId::from(invocation_id))
                {
                    Ok(invocation) => BackendResponse {
                        lines: vec![format!("Tool {} · {:?}", invocation.id, invocation.status)],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Resume => {
                    if self.background_team.is_some() {
                        Self::response_error("已有 Agent Team 正在运行")
                    } else {
                        match self.application.prepare_recovered_agent_team() {
                            Ok(prepared) => {
                                self.launch_background_team(prepared, "recovered=true".to_owned())
                            }
                            Err(error) => Self::response_error(error),
                        }
                    }
                }
                SlashCommand::Reset => {
                    if self.background_team.is_some() {
                        Self::response_error("Agent Team 运行期间不能 reset Context")
                    } else {
                        match self.application.reset_context() {
                            Ok(reset) => BackendResponse {
                                lines: vec![format!(
                                    "Context reset · checkpoint={} · {} -> {} · removed={} retained={}",
                                    reset.checkpoint_id,
                                    reset.previous_series_id,
                                    reset.next_series_id,
                                    reset.removed_items,
                                    reset.retained_items
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                }
                SlashCommand::Sandbox => match self.application.tools() {
                    Ok(view) => BackendResponse {
                        lines: vec![
                            "Filesystem tools: canonical workspace containment active".to_owned(),
                            format!(
                                "Process execution: {}",
                                if view
                                    .tools
                                    .iter()
                                    .any(|tool| tool.canonical_name == "process.exec")
                                {
                                    "exact executable allowlist; Agent high-risk calls require approval"
                                } else {
                                    "disabled (no executable allowlist)"
                                }
                            ),
                            format!(
                                "Process tree containment: {}",
                                if cfg!(windows) {
                                    "Windows Job Object"
                                } else if cfg!(unix) {
                                    "POSIX Process Group"
                                } else {
                                    "unavailable"
                                }
                            ),
                            "OS filesystem isolation for subprocesses: unavailable (不会冒充 AppContainer/seccomp/sandbox-exec)".to_owned(),
                            "Extension proxy: typed MCP/Plugin source binding active; remote MCP only via configured HTTPS/loopback endpoint".to_owned(),
                            format!(
                                "Direct Network Tool: unavailable · Browser Sandbox: {}",
                                if view
                                    .tools
                                    .iter()
                                    .any(|tool| tool.canonical_name.starts_with("browser."))
                                {
                                    "dedicated process adapter"
                                } else {
                                    "unavailable"
                                }
                            ),
                        ],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Skills { operation } => match operation {
                    SkillCommand::List => match self.application.skills() {
                        Ok(skills) => BackendResponse {
                            lines: if skills.is_empty() {
                                vec!["No discovered skills".to_owned()]
                            } else {
                                skills
                                    .into_iter()
                                    .map(|skill| {
                                        format!(
                                            "{} · {} · {:?} · refs {} · tools {}",
                                            skill.id,
                                            skill.version,
                                            skill.status,
                                            skill.reference_count,
                                            skill.required_tools.join(",")
                                        )
                                    })
                                    .collect()
                            },
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    SkillCommand::Search { query } => {
                        match self.application.search_skills(&query) {
                            Ok(skills) => BackendResponse {
                                lines: skills
                                    .into_iter()
                                    .map(|skill| format!("{} · {}", skill.id, skill.description))
                                    .collect(),
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    SkillCommand::Load { skill_id } => {
                        match self.application.load_skill(&skill_id) {
                            Ok(skill) => BackendResponse {
                                lines: vec![format!(
                                    "Skill {} · loaded · prompt {} bytes · refs {} · hash {}",
                                    skill.view.id,
                                    skill.prompt.len(),
                                    skill.references.len(),
                                    skill.view.content_hash.as_deref().unwrap_or("n/a")
                                )],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                    SkillCommand::Unload { skill_id } => {
                        match self.application.unload_skill(&skill_id) {
                            Ok(skill) => BackendResponse {
                                lines: vec![format!("Skill {} · {:?}", skill.id, skill.status)],
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                },
                SlashCommand::Status | SlashCommand::Session => match self.application.status() {
                    Ok(status) => BackendResponse {
                        lines: Self::status_lines(&status),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Sessions => match self.application.sessions() {
                    Ok(sessions) => BackendResponse {
                        lines: Self::session_lines(&sessions),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Settings { operation } => match operation {
                    SettingsCommand::Show { key } => self.settings_show(key.as_deref()),
                    SettingsCommand::Set {
                        key,
                        value,
                        layer,
                    } => match self.application.set_setting(
                        &key,
                        &value,
                        match layer {
                            SettingLayer::Session => ConfigLayer::Session,
                            SettingLayer::Runtime => ConfigLayer::Runtime,
                        },
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some(&key), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    SettingsCommand::Reset { key, layer } => match self.application.clear_setting(
                        &key,
                        match layer {
                            SettingLayer::Session => ConfigLayer::Session,
                            SettingLayer::Runtime => ConfigLayer::Runtime,
                        },
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some(&key), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                },
                SlashCommand::Steer { instruction } => match self.application.steer(&instruction) {
                    Ok(steering) => {
                        if let Some(background) = &self.background_team
                            && let Err(error) = background.steering.push(&instruction)
                        {
                            return Self::response_error(error);
                        }
                        BackendResponse {
                            lines: vec![format!(
                                "Steering {} · {} → {} · queued={}",
                                steering.message_id,
                                steering.mission_id,
                                steering.recipient,
                                steering.queued
                            )],
                            ..BackendResponse::default()
                        }
                    }
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Test { arguments } => {
                    self.run_process_tool("HARNESS_TEST_EXECUTABLE", arguments)
                }
                SlashCommand::Tools => match self.application.tools() {
                    Ok(view) => BackendResponse {
                        lines: Self::tool_lines(&view),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Trace { operation } => match operation {
                    TraceCommand::Status => BackendResponse {
                        lines: Self::config_lines(
                            &self.application.config(),
                            Some("trace.enabled"),
                            false,
                        ),
                        ..BackendResponse::default()
                    },
                    TraceCommand::On => match self.application.set_setting(
                        "trace.enabled",
                        "true",
                        ConfigLayer::Runtime,
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some("trace.enabled"), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    TraceCommand::Off => match self.application.set_setting(
                        "trace.enabled",
                        "false",
                        ConfigLayer::Runtime,
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some("trace.enabled"), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                    TraceCommand::Last { limit } => BackendResponse {
                        lines: self.event_log_lines(limit, true),
                        ..BackendResponse::default()
                    },
                },
                SlashCommand::Undo { patch_id } => match patch_id.map_or_else(
                    || self.application.undo_latest_patch(),
                    |patch_id| self.application.undo_patch(&patch_id),
                ) {
                    Ok(patch) => BackendResponse {
                        lines: vec![format!(
                            "Patch {} · {:?} · {}",
                            patch.id,
                            patch.status,
                            patch.path.display()
                        )],
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Vector { operation } => match operation {
                    VectorCommand::Status=>match self.application.memory_view(){Ok(view)=>BackendResponse{lines:vec![format!("Vector {:?} · schemaPresent={}",view.semantic,view.vector_schema_present)],..Default::default()},Err(error)=>Self::response_error(error)},
                    VectorCommand::Purge=>match self.application.purge_vectors(){Ok(view)=>BackendResponse{lines:vec![format!("Vector projection purged · schemaPresent={}",view.vector_schema_present)],..Default::default()},Err(error)=>Self::response_error(error)},
                    VectorCommand::Mode { mode } => match self.application.set_setting(
                        "vector.mode",
                        &mode,
                        ConfigLayer::Session,
                    ) {
                        Ok(view) => BackendResponse {
                            lines: Self::config_lines(&view, Some("vector.mode"), false),
                            ..BackendResponse::default()
                        },
                        Err(error) => Self::response_error(error),
                    },
                },
                SlashCommand::Why => match self.application.why() {
                    Ok(why) => BackendResponse {
                        lines: Self::why_lines(&why),
                        ..BackendResponse::default()
                    },
                    Err(error) => Self::response_error(error),
                },
                SlashCommand::Clear => BackendResponse {
                    clear_view: true,
                    ..BackendResponse::default()
                },
                SlashCommand::Exit => {
                    if self.background_team.is_some() {
                        let mut response = self.cancel_current();
                        response.lines.push(
                            "后台 Team 正在回收；完成后再次 /exit，避免丢失 Kernel 终态".to_owned(),
                        );
                        response
                    } else {
                        match self.application.shutdown("user-exit") {
                            Ok(()) => BackendResponse {
                                should_exit: true,
                                ..BackendResponse::default()
                            },
                            Err(error) => Self::response_error(error),
                        }
                    }
                }
            },
        }
    }

    fn snapshot(&self) -> TerminalSnapshot {
        let status = self.application.status();
        let plan = self.application.plan();
        let context = self.application.context().ok();
        let cache = self.application.cache();
        match status {
            Ok(status) => TerminalSnapshot {
                model: if self.model_ready {
                    status.model
                } else if is_hidden_internal_model_name(&status.model) {
                    "未配置".to_owned()
                } else {
                    format!("{} [未连接]", status.model)
                },
                model_configured: self.model_ready,
                mode: status.mode,
                reasoning: status.reasoning,
                context_percent: context.as_ref().map_or(0, |context| context.percent),
                cache_percent: cache.effective_hit_rate_percent,
                agents: plan.map_or(0, |plan| plan.running),
                project: self.project_root.clone(),
                branch: None,
                statusbar_visible: self.application.statusbar_visible(),
            },
            Err(_) => TerminalSnapshot {
                model: "unavailable".to_owned(),
                model_configured: false,
                mode: "lite".to_owned(),
                reasoning: "off".to_owned(),
                context_percent: context.as_ref().map_or(0, |context| context.percent),
                cache_percent: cache.effective_hit_rate_percent,
                agents: 0,
                project: self.project_root.clone(),
                branch: None,
                statusbar_visible: self.application.statusbar_visible(),
            },
        }
    }

    fn cancel_current(&mut self) -> BackendResponse {
        if let Some(background) = self.background_team.as_mut() {
            for cancellation in background.cancellations.values() {
                cancellation.cancel();
            }
            background.cancellation_requested = true;
            return BackendResponse {
                lines: vec![
                    "Cancellation requested for active Team; waiting for worker reconciliation"
                        .to_owned(),
                ],
                ..BackendResponse::default()
            };
        }
        match self.application.cancel_active_mission("terminal-ctrl-c") {
            Ok(plan) => BackendResponse {
                lines: Self::plan_lines(&plan),
                ..BackendResponse::default()
            },
            Err(error) if error.code == "active-mission-missing" => BackendResponse {
                lines: vec!["No active Agent run".to_owned()],
                ..BackendResponse::default()
            },
            Err(error) => Self::response_error(error),
        }
    }

    fn submit_secret(&mut self, request_id: &str, secret: String) -> BackendResponse {
        let Some(pending) = self.pending_credential.take() else {
            return Self::response_error("没有等待中的 secure credential request");
        };
        if pending.request_id != request_id {
            self.pending_credential = Some(pending);
            return Self::response_error("secure credential request ID 不匹配");
        }
        if secret.is_empty() {
            return BackendResponse {
                lines: vec![format!("{} credential 输入已取消", pending.display_name)],
                ..BackendResponse::default()
            };
        }
        let store = match OsCredentialStore::new("dev.openai.harness") {
            Ok(store) => store,
            Err(error) => return Self::response_error(error),
        };
        if let Err(error) = store.put(
            &CredentialId::new(&pending.credential_id),
            SecretString::new(secret),
        ) {
            return Self::response_error(error);
        }
        if self
            .application
            .model()
            .is_ok_and(|model| model.provider_id == pending.provider_id)
        {
            self.model_ready = true;
        }
        BackendResponse {
            lines: vec![
                format!(
                    "{} credential 已保存到 OS Credential Store",
                    pending.display_name
                ),
                if self.model_ready {
                    "当前模型已可用，可以开始输入任务。".to_owned()
                } else {
                    format!("下一步输入 /model 并选择 {} 模型。", pending.provider_id)
                },
            ],
            ..BackendResponse::default()
        }
    }

    fn complete_input(&self, input: &str) -> Vec<InputSuggestion> {
        self.input_suggestions(input)
    }

    fn poll(&mut self) -> BackendResponse {
        self.drain_event_log();
        self.poll_background_team()
    }
}

const fn event_kind(event: &HarnessEvent) -> &'static str {
    match event {
        HarnessEvent::SystemStarted { .. } => "system.started",
        HarnessEvent::SystemReady { .. } => "system.ready",
        HarnessEvent::SessionChanged { .. } => "session.changed",
        HarnessEvent::GoalChanged { .. } => "goal.changed",
        HarnessEvent::ModelChanged { .. } => "model.changed",
        HarnessEvent::ModelUsage { .. } => "model.usage",
        HarnessEvent::PlanChanged { .. } => "plan.changed",
        HarnessEvent::AgentStatus { .. } => "agent.status",
        HarnessEvent::ReasoningSummary { .. } => "reasoning.summary",
        HarnessEvent::TextOutput { .. } => "text.output",
        HarnessEvent::ToolStatus { .. } => "tool.status",
        HarnessEvent::BrowserStatus { .. } => "browser.status",
        HarnessEvent::McpStatus { .. } => "mcp.status",
        HarnessEvent::PluginStatus { .. } => "plugin.status",
        HarnessEvent::SkillStatus { .. } => "skill.status",
        HarnessEvent::PermissionRequested { .. } => "permission.requested",
        HarnessEvent::ContextChanged { .. } => "context.changed",
        HarnessEvent::Error { .. } => "error",
        HarnessEvent::SystemShutdown { .. } => "system.shutdown",
    }
}

fn event_scope_summary(entry: &EventEnvelope) -> String {
    let mut parts = Vec::new();
    if let Some(session_id) = &entry.scope.session_id {
        parts.push(format!("session={session_id}"));
    }
    if let Some(mission_id) = &entry.scope.mission_id {
        parts.push(format!("mission={mission_id}"));
    }
    if let Some(task_id) = &entry.scope.task_id {
        parts.push(format!("task={task_id}"));
    }
    if let Some(run_id) = &entry.scope.run_id {
        parts.push(format!("run={run_id}"));
    }
    if parts.is_empty() {
        "global".to_owned()
    } else {
        parts.join(",")
    }
}

pub fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("KernaryError: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let command_name = invoked_command_name();
    let cli = Cli::from_arg_matches(&invocation_command(command_name).get_matches())?;
    let current_directory = std::env::current_dir()?;
    match &cli.command {
        Some(Command::Doctor { json }) => {
            return doctor(&current_directory, *json, cli.ascii, cli.no_color);
        }
        Some(Command::Login { provider }) => return login(*provider),
        Some(Command::Logout { provider }) => return logout(*provider),
        Some(Command::Account { provider }) => return account(*provider),
        Some(Command::Maintenance { operation }) => {
            return maintenance(&current_directory, operation);
        }
        Some(Command::Connect { provider }) => {
            return connect_provider(&current_directory, provider);
        }
        Some(Command::Providers) => return list_providers(&current_directory),
        Some(Command::Completions { shell }) => {
            let mut command = invocation_command(command_name);
            clap_complete::generate(*shell, &mut command, command_name, &mut io::stdout());
            return Ok(());
        }
        Some(Command::Man) => {
            clap_mangen::Man::new(invocation_command(command_name))
                .title(command_name.to_ascii_uppercase())
                .section("1")
                .manual("Kernary Code User Commands")
                .render(&mut io::stdout())?;
            return Ok(());
        }
        _ => {}
    }

    let exec_mode = matches!(&cli.command, Some(Command::Exec { .. }));
    let requested_model = cli.model.clone();
    if let Some(selection) = requested_model.as_deref() {
        ensure_selection_credential(&current_directory, selection, !exec_mode)?;
    }
    let (mut backend, subscription) =
        build_backend(&current_directory, requested_model.as_deref())?;
    backend.application.boot()?;
    if let Some(selection) = requested_model.as_deref() {
        let (provider, model) = parse_model_selection(selection)?;
        backend.application.select_model(provider, model)?;
    }
    if matches!(
        &cli.command,
        Some(Command::Run { .. } | Command::Exec { .. })
    ) && !backend.model_ready
    {
        return Err(
            "MODEL_NOT_CONFIGURED: 先运行 `kernary connect <provider>`，再使用 `--model provider/model`"
                .into(),
        );
    }
    match cli.command {
        Some(Command::Run {
            prompt,
            headless: _,
            json,
        }) => run_headless(
            &mut backend,
            &subscription,
            &prompt.join(" "),
            json,
            cli.ascii,
        ),
        Some(Command::Exec {
            prompt,
            json,
            quiet,
            output,
            force,
        }) => run_exec(
            &mut backend,
            &subscription,
            &prompt.join(" "),
            ExecOptions {
                json,
                quiet,
                ascii: cli.ascii,
                output,
                force,
            },
        ),
        Some(Command::Doctor { .. }) => unreachable!("doctor 已提前处理"),
        Some(Command::Login { .. } | Command::Logout { .. } | Command::Account { .. }) => {
            unreachable!("auth command 已提前处理")
        }
        Some(Command::Maintenance { .. }) => unreachable!("maintenance 已提前处理"),
        Some(Command::Completions { .. } | Command::Man) => {
            unreachable!("文档生成命令已提前处理")
        }
        Some(Command::Connect { .. } | Command::Providers) => {
            unreachable!("Provider 命令已提前处理")
        }
        Some(Command::Models {
            provider,
            refresh,
            json,
        }) => run_models_command(&mut backend.application, provider, refresh, json),
        None => run_interactive(backend, subscription, cli.ui, cli.ascii, cli.no_color),
    }
}

fn invoked_command_name() -> &'static str {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .filter(|name| name.eq_ignore_ascii_case("harness"))
        .map_or("kernary", |_| "harness")
}

fn invocation_command(command_name: &'static str) -> clap::Command {
    match command_name {
        "harness" => Cli::command().name("harness"),
        _ => Cli::command().name("kernary"),
    }
}

fn run_models_command(
    application: &mut Application,
    provider: Option<String>,
    refresh: Option<String>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let refreshed_provider = refresh.clone();
    let mut models = if let Some(refresh) = refresh {
        application.refresh_models(Some(ProviderId::from(refresh)))?
    } else {
        application.models()
    };
    models.retain(|model| !is_hidden_internal_model(&model.provider_id, &model.model_id));
    let filter_provider = provider.or_else(|| refreshed_provider.clone());
    if let Some(provider) = filter_provider {
        models.retain(|model| model.provider_id.as_str() == provider);
    }
    if json {
        println!("{}", serde_json::to_string(&models)?);
        return Ok(());
    }
    if let Some(provider) = refreshed_provider
        && let Ok(project_root) = std::env::current_dir()
        && let Some(summary) = discovery_summary(&project_root, &ProviderId::from(provider))
    {
        println!("{summary}");
    }
    if models.is_empty() {
        println!("No registered models");
        return Ok(());
    }
    for model in models {
        println!(
            "{}/{} | context={} | tools={} | structured={}",
            model.provider_id,
            model.model_id,
            model.context_window_tokens,
            model.tool_calling,
            model.structured_output
        );
    }
    Ok(())
}

fn maintenance(
    project_root: &Path,
    operation: &MaintenanceCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let resolve = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            project_root.join(path)
        }
    };
    match operation {
        MaintenanceCommand::Verify { backup } => {
            let backup = resolve(backup);
            let manifest = harness_storage::verify_project_backup(&backup)?;
            println!(
                "Backup verified · entries={} · version={} · {}",
                manifest.entries.len(),
                manifest.product_version,
                backup.display()
            );
        }
        MaintenanceCommand::Backup { output } => {
            let state = project_root.join(".harness");
            if !state.is_dir() {
                return Err("项目还没有 .harness durable state，无法备份".into());
            }
            let lock = ProjectStateLock::acquire(&state)?;
            let output = resolve(output);
            let manifest = ProjectMaintenance::new(&lock).create_backup(
                &output,
                env!("CARGO_PKG_VERSION"),
                unix_millis()?,
            )?;
            println!(
                "Backup complete · entries={} · {}",
                manifest.entries.len(),
                output.display()
            );
        }
        MaintenanceCommand::Restore {
            backup: _,
            force: false,
        } => {
            return Err("restore 会替换项目数据库；确认后重新运行并添加 --force".into());
        }
        MaintenanceCommand::Restore {
            backup,
            force: true,
        } => {
            let state = project_root.join(".harness");
            let lock = ProjectStateLock::acquire(&state)?;
            let backup = resolve(backup);
            let report = ProjectMaintenance::new(&lock).restore_backup(
                &backup,
                env!("CARGO_PKG_VERSION"),
                unix_millis()?,
            )?;
            println!(
                "Restore complete · entries={} · recoveryPoint={}",
                report.restored_entries,
                report
                    .recovery_point
                    .as_deref()
                    .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string())
            );
        }
    }
    Ok(())
}

fn unix_millis() -> Result<i64, Box<dyn std::error::Error>> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn load_provider_catalog(
    project_root: &Path,
) -> Result<ProviderCatalog, Box<dyn std::error::Error>> {
    let mut catalog = ProviderCatalog::built_in()?;
    let path = configured_path(project_root, "KERNARY_PROVIDER_CONFIG")
        .unwrap_or_else(|| default_project_catalog_path(project_root));
    if path.exists()
        && let Err(error) = catalog.load_project_file(&path)
    {
        eprintln!(
            "[WARN] Provider config isolated · {} · {}",
            path.display(),
            error
        );
    }
    Ok(catalog)
}

fn load_provider_model_cache(project_root: &Path) -> ProviderModelCache {
    let path = default_model_cache_path(project_root);
    match ProviderModelCache::load_isolated(&path) {
        Ok(loaded) => {
            for isolated in loaded.isolated_entries {
                eprintln!("[WARN] Provider model cache entry isolated · {isolated}");
            }
            loaded.cache
        }
        Err(error) => {
            eprintln!(
                "[WARN] Provider model cache isolated · {} · {}",
                path.display(),
                error
            );
            ProviderModelCache::default()
        }
    }
}

fn discovery_summary(project_root: &Path, provider_id: &ProviderId) -> Option<String> {
    let catalog = load_provider_catalog(project_root).ok()?;
    let provider = catalog.get(provider_id)?;
    let cache = load_provider_model_cache(project_root);
    let status = cache.status(provider, unix_millis().ok()?);
    let (discovered, routable) = cache.get(provider_id).map_or((0, 0), |entry| {
        (entry.discovered_models.len(), entry.routable_models.len())
    });
    Some(format!(
        "Discovery {provider_id} · {status} · discovered={discovered} · routable={routable} · unroutable={}",
        discovered.saturating_sub(routable)
    ))
}

fn connect_provider(project_root: &Path, provider: &str) -> Result<(), Box<dyn std::error::Error>> {
    let catalog = load_provider_catalog(project_root)?;
    let provider_id = ProviderId::from(provider.to_owned());
    let definition = catalog
        .get(&provider_id)
        .ok_or_else(|| format!("Provider Catalog 中不存在 {provider}"))?;
    if !definition.credential_required {
        println!("{} 不需要 API key。", definition.display_name);
        return Ok(());
    }
    let credential_id = definition
        .credential_id
        .as_deref()
        .ok_or("Provider 缺少 credential_id")?;
    let secret = read_secret(&format!("{} API key: ", definition.display_name))?;
    OsCredentialStore::new("dev.openai.harness")?.put(&CredentialId::new(credential_id), secret)?;
    println!(
        "{} credential 已保存到 OS Credential Store。",
        definition.display_name
    );
    Ok(())
}

fn list_providers(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let catalog = load_provider_catalog(project_root)?;
    let cache = load_provider_model_cache(project_root);
    let now_millis = unix_millis()?;
    let store = OsCredentialStore::new("dev.openai.harness")?;
    println!("Kernary Provider Catalog");
    for provider in catalog.list() {
        let models = provider
            .routes
            .iter()
            .map(|route| route.models.len())
            .sum::<usize>();
        let credential = match provider.credential_id.as_deref() {
            None => "not-required".to_owned(),
            Some(id) => match store.get(&CredentialId::new(id)) {
                Ok(Some(_)) => "configured".to_owned(),
                Ok(None) => "missing".to_owned(),
                Err(_) => "store-unavailable".to_owned(),
            },
        };
        let discovery = cache.status(&provider, now_millis);
        let (discovered, routable) = cache.get(&provider.id).map_or((0, 0), |entry| {
            (entry.discovered_models.len(), entry.routable_models.len())
        });
        println!(
            "{} | {} | routes={} | models={} | credential={} | discovery={} | discovered={} | routable={}",
            provider.id,
            provider.display_name,
            provider.routes.len(),
            models,
            credential,
            discovery,
            discovered,
            routable
        );
    }
    Ok(())
}

fn ensure_selection_credential(
    project_root: &Path,
    selection: &str,
    allow_interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (provider_id, _) = parse_model_selection(selection)?;
    let catalog = load_provider_catalog(project_root)?;
    let Some(provider) = catalog.get(&provider_id) else {
        return Ok(());
    };
    if !provider.credential_required {
        return Ok(());
    }
    let credential_id = provider
        .credential_id
        .as_deref()
        .ok_or("Provider 缺少 credential_id")?;
    let interactive = allow_interactive && io::stdin().is_terminal();
    // Headless Linux CI 往往没有 Secret Service。非交互选择模型时，
    // 凭证库不可用等价于“当前没有可用凭证”，应返回稳定的连接指引，
    // 而不是把平台 keyring 的实现错误暴露给用户。
    if let Err(error) = OsCredentialStore::available() {
        if !interactive {
            return Err(format!(
                "CredentialRequired: {}；先运行 `kernary connect {}`",
                provider.display_name, provider.id
            )
            .into());
        }
        return Err(error.into());
    }
    let store = OsCredentialStore::new("dev.openai.harness")?;
    if store.get(&CredentialId::new(credential_id))?.is_some() {
        return Ok(());
    }
    if !interactive {
        return Err(format!(
            "CredentialRequired: {}；先运行 `kernary connect {}`",
            provider.display_name, provider.id
        )
        .into());
    }
    let secret = read_secret(&format!("{} API key: ", provider.display_name))?;
    store.put(&CredentialId::new(credential_id), secret)?;
    println!(
        "{} credential 已安全保存；继续选择 {selection}。",
        provider.display_name
    );
    Ok(())
}

fn build_backend(
    project_root: &Path,
    requested_model: Option<&str>,
) -> Result<(AppBackend, EventSubscription), Box<dyn std::error::Error>> {
    let startup_started = Instant::now();
    let state_directory = project_root.join(".harness");
    fs::create_dir_all(&state_directory)?;
    let project_lock = ProjectStateLock::acquire(&state_directory)?;
    let database_path = state_directory.join("kernel.sqlite");
    let store = SqliteKernelStore::open(&database_path)?;
    let session_id = SessionId::from("session:default");
    let persisted = store.recover_session(&session_id)?;
    let config = load_runtime_config(project_root, &persisted.settings)?;
    let test_model_enabled = internal_test_model_enabled();
    let persisted_selection = persisted
        .model
        .provider_id
        .clone()
        .zip(persisted.model.model_id.clone())
        .filter(|(provider, model)| {
            !is_unconfigured_model(provider, model)
                && (test_model_enabled || !is_internal_test_model(provider, model))
        });
    let requested_selection = requested_model.map(parse_model_selection).transpose()?;
    if let Some((provider, model)) = requested_selection.as_ref()
        && is_hidden_internal_model(provider, model)
        && !(test_model_enabled && is_internal_test_model(provider, model))
    {
        return Err("内部占位/测试模型不能用于发布运行".into());
    }
    let initial_selection = requested_selection
        .clone()
        .or_else(|| persisted_selection.clone())
        .unwrap_or_else(|| {
            if test_model_enabled {
                (
                    ProviderId::from(INTERNAL_TEST_PROVIDER),
                    ModelId::from(INTERNAL_TEST_MODEL),
                )
            } else {
                (
                    ProviderId::from(UNCONFIGURED_PROVIDER_ID),
                    ModelId::from(UNCONFIGURED_MODEL_ID),
                )
            }
        });

    let credentials: Arc<dyn CredentialStore> =
        Arc::new(OsCredentialStore::new("dev.openai.harness")?);
    let mut provider_catalog = load_provider_catalog(project_root)?;
    for (provider_id, model_id) in requested_selection.iter().chain(persisted_selection.iter()) {
        if provider_catalog.get(provider_id).is_some() {
            provider_catalog.extend_explicit_single_route_model(provider_id, model_id.clone())?;
        }
    }
    let model_ready = selection_is_ready(
        &provider_catalog,
        credentials.as_ref(),
        &initial_selection.0,
        &initial_selection.1,
        test_model_enabled,
    );
    let mut model_registry = ModelRegistry::new();
    model_registry.register(Arc::new(UnconfiguredModelProvider))?;
    #[cfg(debug_assertions)]
    if test_model_enabled {
        let fake_delay = compat_env_var("HARNESS_FAKE_EVENT_DELAY_MILLIS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::ZERO);
        model_registry.register(Arc::new(if fake_delay.is_zero() {
            FakeModelProvider::echo()
        } else {
            FakeModelProvider::echo_with_delay(fake_delay)
        }))?;
    }
    for (provider_name, model_env, endpoint, credential_id, reasoning_field) in [(
        "compatible",
        "HARNESS_COMPAT_MODEL",
        compat_env_var("HARNESS_COMPAT_ENDPOINT").unwrap_or_default(),
        Some("compatible:default"),
        if compat_env_var("HARNESS_COMPAT_REASONING_EFFORT")
            .ok()
            .as_deref()
            == Some("1")
        {
            CompatibleReasoningField::ReasoningEffort
        } else {
            CompatibleReasoningField::Omit
        },
    )] {
        let models = configured_provider_models(
            provider_name,
            model_env,
            requested_selection.as_ref(),
            persisted_selection.as_ref(),
        );
        if models.is_empty() {
            continue;
        }
        let provider_id = ProviderId::from(provider_name);
        if provider_catalog.get(&provider_id).is_some() {
            continue;
        }
        if endpoint.is_empty() {
            return Err(
                format!("{provider_name} 已配置 Model，但缺少 HARNESS_COMPAT_ENDPOINT").into(),
            );
        }
        let provider = CompatibleProvider::with_ureq(
            CompatibleProviderConfig {
                provider_id: provider_id.clone(),
                endpoint,
                credential_id: credential_id.map(CredentialId::new),
                models: models
                    .iter()
                    .map(|model| compatible_capability(&provider_id, model))
                    .collect(),
                reasoning_field,
                headers: if provider_id.as_str() == "gemini" {
                    [(
                        "x-goog-api-client".to_owned(),
                        format!("kernary-code-oai/{}", env!("CARGO_PKG_VERSION")),
                    )]
                    .into_iter()
                    .collect()
                } else {
                    Default::default()
                },
            },
            credentials.clone(),
        )?;
        model_registry.register(Arc::new(provider))?;
    }
    let provider_runtime = CatalogProviderRuntime::with_ureq(
        default_model_cache_path(project_root),
        credentials.clone(),
    )?;
    for isolated in provider_runtime.isolated_cache_entries() {
        eprintln!("[WARN] Provider model cache entry isolated · {isolated}");
    }
    provider_runtime.register_all(&mut model_registry, &provider_catalog)?;
    let model_runtime = ModelRuntime::new(
        model_registry,
        initial_selection.0,
        initial_selection.1,
        persisted.model.reasoning,
    )?;
    let event_bus = EventBus::new();
    let subscription = event_bus.subscribe(1_024)?;
    let event_log_subscription = event_bus.subscribe(4_096)?;
    let clock = SystemClock;
    let ids = ProcessIdGenerator::new(&clock);
    let cache_policy = CachePolicy::safe_default(512, 32 * 1024 * 1024);
    let cache = CacheEngine::new(
        MemoryCache::new(cache_policy.clone()),
        Some(DiskCache::open(
            state_directory.join("cache"),
            cache_policy,
        )?),
    );
    let guard = WorkspacePathGuard::new(project_root)?;
    let patch_store = Arc::new(PatchStore::open(
        state_directory.join("patches"),
        guard.clone(),
    )?);
    let recovery_time = harness_types::Clock::now_unix_millis(&clock);
    let reconciled_patches = patch_store.reconcile_prepared(recovery_time)?;
    let embedding_model_raw = compat_env_var("HARNESS_EMBEDDING_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (embedding_provider, embedding_model) =
        embedding_model_raw
            .as_deref()
            .map_or((None, None), |value| {
                value.split_once('/').map_or_else(
                    || {
                        (
                            compat_env_var("HARNESS_EMBEDDING_PROVIDER").ok(),
                            Some(value.to_owned()),
                        )
                    },
                    |(provider, model)| (Some(provider.to_owned()), Some(model.to_owned())),
                )
            });
    let embedding_dimensions = compat_env_var("HARNESS_EMBEDDING_DIMENSIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    let embedding_config = EmbeddingConfig {
        model: embedding_model,
        provider: embedding_provider,
        dimensions: embedding_dimensions,
    };
    let mut memory = ProjectMemory::open(
        "project:local",
        state_directory.join("memory.sqlite"),
        embedding_config,
    )?;
    if let SemanticCapability::Ready { provider, .. } = memory.view()?.semantic {
        let endpoint = compat_env_var("HARNESS_EMBEDDING_ENDPOINT").unwrap_or_else(|_| {
            if provider == "openai" {
                "https://api.openai.com/v1/embeddings".to_owned()
            } else {
                "http://127.0.0.1:11434/v1/embeddings".to_owned()
            }
        });
        match HttpEmbeddingFactory::new(
            HttpEmbeddingConfig {
                provider: provider.clone(),
                endpoint,
                credential_id: (provider == "openai").then(|| {
                    compat_env_var("HARNESS_EMBEDDING_CREDENTIAL_ID")
                        .unwrap_or_else(|_| OPENAI_API_KEY_CREDENTIAL_ID.to_owned())
                }),
                allow_remote_project_private: compat_env_var("HARNESS_EMBEDDING_ALLOW_REMOTE")
                    .ok()
                    .as_deref()
                    == Some("1"),
                timeout_millis: Some(30_000),
            },
            credentials.clone(),
            Arc::new(UreqStreamingTransport::default()),
        ) {
            Ok(factory) => memory.attach_embedding_factory(Arc::new(factory))?,
            Err(error) => memory.block_semantic(error.code),
        }
    }
    let repository =
        RepositoryIndex::open(project_root, state_directory.join("repository.sqlite"))?;
    let lsp = configured_lsp_manager(project_root);
    let lsp_process_specs = lsp
        .as_ref()
        .and_then(|manager| match manager.process_specs() {
            Ok(specs) => Some(specs),
            Err(error) => {
                eprintln!("[WARN] LSP tools unavailable · {error}");
                None
            }
        });
    let agent_messages = AgentMessageBus::open(state_directory.join("agents.sqlite"))?;
    let agent_state = AgentStateStore::open(state_directory.join("agents.sqlite"))?;
    let agent_budgets = AgentBudgetManager::open(state_directory.join("agents.sqlite"))?;
    let file_leases =
        FileLeaseManager::open(project_root, state_directory.join("file-leases.sqlite"))?;
    let mut tool_registry = ToolRegistry::new();
    register_file_tools_with_patch_store(
        &mut tool_registry,
        guard.clone(),
        4 * 1024 * 1024,
        Some(patch_store.clone()),
    )?;
    let browser_runtime =
        configured_browser_runtime(project_root, &state_directory, recovery_time)?;
    if let Some(browser) = &browser_runtime {
        register_browser_tools(&mut tool_registry, browser.clone())?;
    }
    if let (Some(lsp), Some(_)) = (&lsp, &lsp_process_specs) {
        register_lsp_tools(&tool_registry, lsp.clone())?;
        let preview_store = Arc::new(LspPatchStore::new(
            state_directory.join("lsp-previews"),
            guard.clone(),
        ));
        let patch_coordinator = Arc::new(LspPatchCoordinator::new(
            project_root,
            state_directory.join("file-leases.sqlite"),
            preview_store.clone(),
            patch_store.clone(),
        )?);
        patch_coordinator.reconcile(recovery_time)?;
        register_lsp_patch_tools(
            &tool_registry,
            lsp.clone(),
            preview_store,
            patch_coordinator,
        )?;
    }
    let mut process_tool_executables = compat_env_var_os("HARNESS_PROCESS_EXECUTABLES")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    for name in ["HARNESS_GIT_EXECUTABLE", "HARNESS_TEST_EXECUTABLE"] {
        if let Some(path) = compat_env_var_os(name) {
            process_tool_executables.push(PathBuf::from(path));
        }
    }
    process_tool_executables.sort();
    process_tool_executables.dedup();
    process_tool_executables = process_tool_executables
        .into_iter()
        .map(fs::canonicalize)
        .collect::<Result<Vec<_>, _>>()?;
    if !process_tool_executables.is_empty() {
        register_process_tool(
            &mut tool_registry,
            guard.clone(),
            process_tool_executables.clone(),
            Duration::from_secs(300),
            4 * 1024 * 1024,
        )?;
    }
    let mut sandbox_executables = process_tool_executables;
    if let Some(specs) = &lsp_process_specs {
        sandbox_executables.extend(specs.iter().map(|spec| spec.executable.clone()));
    }
    sandbox_executables.sort();
    sandbox_executables.dedup();
    let mcp_manager = McpManager::new(
        project_root,
        credentials.clone(),
        Arc::new(UreqStreamingTransport::default()),
        tool_registry.clone(),
    )?;
    let mcp_config_path = configured_path(project_root, "HARNESS_MCP_CONFIG")
        .unwrap_or_else(|| project_root.join("kernary.mcp.toml"));
    let mcp_config = if mcp_config_path.is_file() {
        load_mcp_config_file(&mcp_config_path)?
    } else {
        McpConfigFile::default()
    };
    let mcp_configs = mcp_config
        .servers
        .iter()
        .cloned()
        .map(|server| (server.id.clone(), server))
        .collect::<BTreeMap<_, _>>();
    let mut configured_mcp_server_ids = Vec::new();
    for server in mcp_config.servers {
        let enabled = server.enabled;
        let view = mcp_manager.add_server(server)?;
        if enabled {
            configured_mcp_server_ids.push(view.id);
        }
    }
    let plugin_manager = PluginManager::new(env!("CARGO_PKG_VERSION"), tool_registry.clone())?;
    let plugin_roots = configured_directories(project_root, "HARNESS_PLUGIN_DIRS");
    let plugin_views = plugin_manager.discover_isolated(&plugin_roots)?.plugins;
    let skill_registry = Arc::new(SkillRegistry::new(tool_registry.clone()));
    let mut skill_roots = configured_directories(project_root, "HARNESS_SKILL_DIRS")
        .into_iter()
        .map(|path| (path, SkillSource::Project))
        .collect::<Vec<_>>();
    skill_roots.extend(
        configured_directories(project_root, "HARNESS_USER_SKILL_DIRS")
            .into_iter()
            .map(|path| (path, SkillSource::User)),
    );
    skill_registry.discover_isolated(&skill_roots)?;
    let mut permission_profile = workspace_write_profile(guard.root().to_path_buf());
    permission_profile.filesystem.denied_roots = [
        guard.root().join(".git"),
        guard.root().join(".harness"),
        guard.root().join(".env"),
    ]
    .into_iter()
    .collect();
    permission_profile.subprocess.allowed_executables = sandbox_executables.clone();
    permission_profile.browser.enabled = browser_runtime.is_some();
    permission_profile.browser.allow_uploads = compat_env_var("HARNESS_BROWSER_ALLOW_UPLOADS")
        .ok()
        .as_deref()
        == Some("1");
    permission_profile.browser.allow_downloads = compat_env_var("HARNESS_BROWSER_ALLOW_DOWNLOADS")
        .ok()
        .as_deref()
        == Some("1");
    permission_profile.mcp.allowed_server_ids = configured_mcp_server_ids;
    permission_profile.mcp.allowed_tool_patterns = vec!["*".to_owned()];
    permission_profile.plugin.allowed_plugin_ids =
        plugin_views.into_iter().map(|plugin| plugin.id).collect();
    permission_profile.plugin.allowed_capability_patterns = vec!["*".to_owned()];
    let permission_rules_path = configured_path(project_root, "HARNESS_PERMISSION_RULES")
        .unwrap_or_else(|| project_root.join("kernary.permissions.toml"));
    let loaded_permission_rules = if permission_rules_path.is_file() {
        load_permission_rules(&permission_rules_path)?.rules
    } else {
        Vec::new()
    };
    let permission_rules = loaded_permission_rules
        .iter()
        .cloned()
        .map(|rule| (rule.id.clone(), rule))
        .collect::<BTreeMap<_, _>>();
    let mut permission_engine = PermissionEngine::new(
        permission_profile,
        permission_policy(config.effective().settings.permission_mode),
    );
    permission_engine.replace_rules(loaded_permission_rules)?;
    let tool_journal: Arc<dyn ToolInvocationJournal> =
        Arc::new(SqliteKernelStore::open(&database_path)?);
    let tool_runtime = Arc::new(ToolRuntime::new(
        tool_registry.clone(),
        permission_engine,
        tool_journal,
        Arc::new(WorkspaceSandbox::with_processes(
            guard,
            sandbox_executables,
        )?),
    ));
    tool_runtime.rehydrate_pending_approvals()?;
    tool_runtime.recover_interrupted(recovery_time)?;
    reconcile_patch_invocations(
        tool_runtime.journal().as_ref(),
        &reconciled_patches,
        recovery_time,
    )?;
    let mut application = HarnessApplication::new(
        store,
        event_bus,
        clock,
        ids,
        ProjectId::from("project:local"),
        project_root.display().to_string(),
        session_id,
    )
    .with_cache(cache)
    .with_model_runtime(model_runtime)
    .with_credentials(credentials)
    .with_tool_runtime(tool_runtime)
    .with_patch_store(patch_store)
    .with_mcp_manager(mcp_manager)
    .with_plugin_manager(plugin_manager)
    .with_skill_registry(skill_registry)
    .with_memory(memory)
    .with_repository(repository)
    .with_agent_catalog(builtin_agent_catalog()?)
    .with_agent_control_plane(agent_messages, file_leases, agent_state, agent_budgets);
    application = application.with_config(config)?;
    if let Some(browser) = browser_runtime {
        application = application.with_browser_runtime(browser);
    }
    if let Some(lsp) = lsp {
        application = application.with_lsp(lsp);
    }
    application.record_startup_millis(
        u64::try_from(startup_started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    Ok((
        AppBackend {
            application,
            registry: CommandRegistry::new(),
            project_root: project_root.display().to_string(),
            mcp_config_path,
            mcp_configs,
            permission_rules_path,
            permission_rules,
            event_log_subscription,
            event_log: VecDeque::new(),
            background_team: None,
            pending_credential: None,
            model_ready,
            test_model_enabled,
            _project_lock: project_lock,
        },
        subscription,
    ))
}

fn parse_model_selection(value: &str) -> Result<(ProviderId, ModelId), Box<dyn std::error::Error>> {
    let Some((provider, model)) = value.split_once('/') else {
        return Err("Model 必须使用 provider/model 格式".into());
    };
    if provider.trim().is_empty() || model.trim().is_empty() {
        return Err("Provider/Model 不能为空".into());
    }
    Ok((ProviderId::from(provider), ModelId::from(model)))
}

/// 新品牌环境变量优先；旧 HARNESS_* 在一个兼容周期内作为 fallback。
fn compat_env_var(name: &str) -> Result<String, std::env::VarError> {
    let Some(suffix) = name.strip_prefix("HARNESS_") else {
        return std::env::var(name);
    };
    let primary = format!("KERNARY_{suffix}");
    match std::env::var(primary) {
        Err(std::env::VarError::NotPresent) => std::env::var(name),
        result => result,
    }
}

fn compat_env_var_os(name: &str) -> Option<std::ffi::OsString> {
    let Some(suffix) = name.strip_prefix("HARNESS_") else {
        return std::env::var_os(name);
    };
    std::env::var_os(format!("KERNARY_{suffix}")).or_else(|| std::env::var_os(name))
}

fn load_runtime_config(
    project_root: &Path,
    session_values: &BTreeMap<String, String>,
) -> Result<ConfigManager, Box<dyn std::error::Error>> {
    let global_path = compat_env_var_os("HARNESS_GLOBAL_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(default_global_config_path);
    let project_path = compat_env_var_os("HARNESS_PROJECT_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            }
        })
        .unwrap_or_else(|| project_root.join("kernary.toml"));
    let empty = BTreeMap::new();
    let mut global = load_isolated_settings(global_path.as_deref(), "global");
    if let Some(candidate) = global.clone()
        && let Err(error) = ConfigManager::new(Some(candidate.clone()), None, &empty)
    {
        eprintln!(
            "[WARN] global Kernary config isolated · {} · {error}",
            candidate.0.display()
        );
        global = None;
    }
    let mut project = load_isolated_settings(Some(&project_path), "project");
    if let Some(candidate) = project.clone()
        && let Err(error) = ConfigManager::new(global.clone(), Some(candidate.clone()), &empty)
    {
        eprintln!(
            "[WARN] project Kernary config isolated · {} · {error}",
            candidate.0.display()
        );
        project = None;
    }
    let mut manager = ConfigManager::new(global, project, session_values)?;
    if let Ok(policy) = compat_env_var("HARNESS_APPROVAL_POLICY") {
        let mode = match policy.as_str() {
            "always" => "safe",
            "on-request" => "ask",
            "untrusted-only" => "auto",
            "never-within-sandbox" => "full",
            _ => {
                return Err(format!(
                    "HARNESS_APPROVAL_POLICY 无效：{policy}；支持 always/on-request/untrusted-only/never-within-sandbox"
                )
                .into());
            }
        };
        manager.set_runtime("permissions.mode", mode)?;
    }
    Ok(manager)
}

fn default_global_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|root| root.join("Kernary").join("config.toml"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join("kernary").join("config.toml"))
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|root| root.join(".config").join("kernary").join("config.toml"))
        })
}

fn load_isolated_settings(
    path: Option<&Path>,
    layer: &str,
) -> Option<(PathBuf, harness_config::SettingsPatch)> {
    let path = path?.to_path_buf();
    if !path.exists() {
        return None;
    }
    match load_config_file(&path) {
        Ok(settings) => Some((path, settings)),
        Err(error) => {
            eprintln!(
                "[WARN] {layer} Kernary config isolated · {} · {error}",
                path.display()
            );
            None
        }
    }
}

fn configured_path(project_root: &Path, name: &str) -> Option<PathBuf> {
    compat_env_var_os(name).map(PathBuf::from).map(|path| {
        if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        }
    })
}

fn configured_directories(project_root: &Path, name: &str) -> Vec<PathBuf> {
    compat_env_var_os(name)
        .map(|value| {
            std::env::split_paths(&value)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        project_root.join(path)
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn configured_lsp_manager(project_root: &Path) -> Option<LspManager> {
    let path = configured_path(project_root, "KERNARY_LSP_CONFIG")
        .unwrap_or_else(|| default_lsp_config_path(project_root));
    if !path.exists() {
        return None;
    }
    match LspManager::load(&path, project_root) {
        Ok(manager) => Some(manager),
        Err(error) => {
            eprintln!(
                "[WARN] LSP config isolated · {} · {}",
                path.display(),
                error
            );
            None
        }
    }
}

fn configured_browser_runtime(
    project_root: &Path,
    state_directory: &Path,
    recovery_time: i64,
) -> Result<Option<Arc<BrowserRuntime>>, Box<dyn std::error::Error>> {
    let python = compat_env_var_os("HARNESS_BROWSER_PYTHON").map(PathBuf::from);
    let browser = compat_env_var_os("HARNESS_BROWSER_EXECUTABLE").map(PathBuf::from);
    let origins = compat_env_var("HARNESS_BROWSER_ALLOWED_ORIGINS")
        .ok()
        .map(|value| {
            value
                .split([',', ';'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    if python.is_none() && browser.is_none() && origins.is_empty() {
        return Ok(None);
    }
    let python = python.ok_or("HARNESS_BROWSER_PYTHON 未配置")?;
    let browser = browser.ok_or("HARNESS_BROWSER_EXECUTABLE 未配置")?;
    if origins.is_empty() {
        return Err("HARNESS_BROWSER_ALLOWED_ORIGINS 至少需要一个 http(s) Origin".into());
    }
    let allow_uploads = compat_env_var("HARNESS_BROWSER_ALLOW_UPLOADS")
        .ok()
        .as_deref()
        == Some("1");
    let allow_downloads = compat_env_var("HARNESS_BROWSER_ALLOW_DOWNLOADS")
        .ok()
        .as_deref()
        == Some("1");
    let upload_roots = if allow_uploads {
        let roots = configured_directories(project_root, "HARNESS_BROWSER_UPLOAD_ROOTS");
        if roots.is_empty() {
            return Err("启用 Browser upload 时必须配置 HARNESS_BROWSER_UPLOAD_ROOTS".into());
        }
        roots
            .into_iter()
            .map(fs::canonicalize)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let browser_state = state_directory.join("browser");
    let journal = Arc::new(SqliteBrowserJournal::open(
        state_directory.join("browser.sqlite"),
    )?);
    journal.recover_interrupted(recovery_time)?;
    let adapter = Arc::new(PlaywrightProcessAdapter::new(python)?);
    let runtime = Arc::new(BrowserRuntime::new(
        BrowserSessionConfig {
            id: BrowserSessionId::from("browser:default"),
            browser_executable: browser,
            profile_directory: browser_state.join("profile"),
            artifact_directory: browser_state.join("artifacts"),
            download_directory: browser_state.join("downloads"),
            headless: compat_env_var("HARNESS_BROWSER_HEADLESS").ok().as_deref() != Some("0"),
            allowed_origins: origins,
            upload_roots,
            allow_uploads,
            allow_downloads,
            timeout_millis: compat_env_var("HARNESS_BROWSER_TIMEOUT_MILLIS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30_000),
        },
        adapter,
        journal,
    )?);
    Ok(Some(runtime))
}

fn configured_provider_models(
    provider: &str,
    model_env: &str,
    requested: Option<&(ProviderId, ModelId)>,
    persisted: Option<&(ProviderId, ModelId)>,
) -> Vec<ModelId> {
    let mut models = requested
        .into_iter()
        .chain(persisted)
        .filter(|(candidate, _)| candidate.as_str() == provider)
        .map(|(_, model)| model.clone())
        .collect::<Vec<_>>();
    if let Ok(model) = compat_env_var(model_env)
        && !model.trim().is_empty()
    {
        models.push(ModelId::from(model));
    }
    models.sort();
    models.dedup();
    models
}

fn compatible_capability(provider_id: &ProviderId, model_id: &ModelId) -> ModelCapability {
    let reasoning_levels = match provider_id.as_str() {
        "deepseek" | "ollama" => [
            ReasoningLevel::Off,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
        .into_iter()
        .collect(),
        "gemini" => [
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
        .into_iter()
        .collect(),
        _ => Default::default(),
    };
    let prefix = provider_id.as_str().to_ascii_uppercase();
    ModelCapability {
        provider_id: provider_id.clone(),
        model_id: model_id.clone(),
        streaming: true,
        tool_calling: true,
        structured_output: true,
        image_input: false,
        prompt_cache_metrics: true,
        conversation_continuation: false,
        provider_compaction: false,
        context_window_tokens: env_u32(&format!("HARNESS_{prefix}_CONTEXT_TOKENS"), 32_768),
        max_output_tokens: env_u32(&format!("HARNESS_{prefix}_MAX_OUTPUT_TOKENS"), 4_096),
        reasoning_levels,
    }
}

fn env_u32(name: &str, fallback: u32) -> u32 {
    compat_env_var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

const fn permission_policy(mode: PermissionMode) -> ApprovalPolicy {
    match mode {
        PermissionMode::Safe => ApprovalPolicy::Always,
        PermissionMode::Ask | PermissionMode::Custom => ApprovalPolicy::OnRequest,
        PermissionMode::Auto => ApprovalPolicy::UntrustedOnly,
        PermissionMode::Full => ApprovalPolicy::NeverWithinSandbox,
    }
}

fn login(provider: AuthProvider) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        AuthProvider::Openai
        | AuthProvider::Deepseek
        | AuthProvider::Openrouter
        | AuthProvider::Compatible
        | AuthProvider::Gemini
        | AuthProvider::Anthropic => {
            let name = auth_provider_name(provider);
            let secret = read_secret(&format!("{name} API key: "))?;
            let store = OsCredentialStore::new("dev.openai.harness")?;
            store.put(&CredentialId::new(provider_credential_id(provider)), secret)?;
            println!("{name} API key 已保存到 OS Credential Store。");
            Ok(())
        }
        AuthProvider::Codex => {
            CodexAuthBridge::new(SystemCodexProcessRunner).login()?;
            Ok(())
        }
    }
}

fn logout(provider: AuthProvider) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        AuthProvider::Openai
        | AuthProvider::Deepseek
        | AuthProvider::Openrouter
        | AuthProvider::Compatible
        | AuthProvider::Gemini
        | AuthProvider::Anthropic => {
            let store = OsCredentialStore::new("dev.openai.harness")?;
            let deleted = store.delete(&CredentialId::new(provider_credential_id(provider)))?;
            println!(
                "{} credential deleted={deleted}",
                auth_provider_name(provider)
            );
            Ok(())
        }
        AuthProvider::Codex => {
            CodexAuthBridge::new(SystemCodexProcessRunner).logout()?;
            println!("Official Codex logout completed.");
            Ok(())
        }
    }
}

fn account(provider: AuthProvider) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        AuthProvider::Openai
        | AuthProvider::Deepseek
        | AuthProvider::Openrouter
        | AuthProvider::Compatible
        | AuthProvider::Gemini
        | AuthProvider::Anthropic => {
            let store = OsCredentialStore::new("dev.openai.harness")?;
            let configured = store
                .get(&CredentialId::new(provider_credential_id(provider)))?
                .is_some();
            println!("Provider: {}", auth_provider_name(provider));
            println!("Auth: api-key");
            println!("Configured: {configured}");
            println!("Storage: OS Credential Store");
            Ok(())
        }
        AuthProvider::Codex => {
            let status = CodexAuthBridge::new(SystemCodexProcessRunner).status()?;
            println!("{status}");
            Ok(())
        }
    }
}

const fn auth_provider_name(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::Openai => "openai",
        AuthProvider::Codex => "codex",
        AuthProvider::Deepseek => "deepseek",
        AuthProvider::Openrouter => "openrouter",
        AuthProvider::Compatible => "compatible",
        AuthProvider::Gemini => "gemini",
        AuthProvider::Anthropic => "anthropic",
    }
}

const fn provider_credential_id(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::Openai => OPENAI_API_KEY_CREDENTIAL_ID,
        AuthProvider::Deepseek => "deepseek:default",
        AuthProvider::Openrouter => "openrouter:default",
        AuthProvider::Compatible => "compatible:default",
        AuthProvider::Gemini => "gemini:default",
        AuthProvider::Anthropic => "anthropic:default",
        AuthProvider::Codex => "codex:delegated",
    }
}

fn read_secret(prompt: &str) -> Result<SecretString, Box<dyn std::error::Error>> {
    let value = if io::stdin().is_terminal() {
        read_secret_tty(prompt)?
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    if value.is_empty() {
        return Err("API key 不能为空".into());
    }
    Ok(SecretString::new(value))
}

fn read_secret_tty(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            eprintln!();
        }
    }

    eprint!("{prompt}");
    io::stderr().flush()?;
    enable_raw_mode()?;
    let _guard = RawModeGuard;
    let mut value = String::new();
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Enter => break,
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "login cancelled").into());
            }
            KeyCode::Esc => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "login cancelled").into());
            }
            KeyCode::Char(character) => value.push(character),
            _ => {}
        }
    }
    Ok(value)
}

fn run_headless(
    backend: &mut AppBackend,
    subscription: &EventSubscription,
    prompt: &str,
    json: bool,
    ascii: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = backend.handle_input(prompt);
    backend.application.shutdown("headless-complete")?;
    let plain = PlainRenderer::new(RenderStyle {
        ascii,
        color: false,
    });
    let json_renderer = JsonRenderer;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    while let Ok(envelope) = subscription.try_recv() {
        if json {
            writeln!(output, "{}", json_renderer.render_event(&envelope)?)?;
        } else {
            writeln!(output, "{}", plain.render_event(&envelope))?;
        }
    }
    if json {
        writeln!(
            output,
            "{}",
            serde_json::json!({
                "schemaVersion": 1,
                "type": "command.result",
                "lines": response.lines,
                "exit": response.should_exit
            })
        )?;
    } else {
        for line in response.lines {
            writeln!(output, "{}", plain.sanitize(&line))?;
        }
    }
    Ok(())
}

struct ExecOptions {
    json: bool,
    quiet: bool,
    ascii: bool,
    output: Option<PathBuf>,
    force: bool,
}

fn run_exec(
    backend: &mut AppBackend,
    subscription: &EventSubscription,
    prompt: &str,
    options: ExecOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let response = backend.handle_input(prompt);
    let plan = backend.application.plan()?;
    let status = if response.lines.iter().any(|line| line.starts_with("! ")) {
        "failed"
    } else if plan.mission_id.is_some()
        && plan.accepted > 0
        && plan.running == 0
        && plan.pending == 0
        && plan.blocked == 0
    {
        "completed"
    } else {
        "blocked"
    };
    let exit_code = match status {
        "completed" => 0,
        "blocked" => 2,
        _ => 1,
    };
    backend.application.shutdown("exec-complete")?;
    let events = std::iter::from_fn(|| subscription.try_recv().ok()).collect::<Vec<_>>();
    let elapsed_millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let rendered = if options.json {
        let status_view = backend.application.status()?;
        format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "schemaVersion": 1,
                "type": "exec.result",
                "status": status,
                "exitCode": exit_code,
                "elapsedMillis": elapsed_millis,
                "sessionId": status_view.session_id,
                "missionId": plan.mission_id,
                "model": status_view.model,
                "mode": status_view.mode,
                "plan": plan,
                "lines": response.lines,
                "events": if options.quiet {
                    serde_json::Value::Array(Vec::new())
                } else {
                    serde_json::to_value(&events)?
                }
            }))?
        )
    } else {
        let plain = PlainRenderer::new(RenderStyle {
            ascii: options.ascii,
            color: false,
        });
        let mut lines = Vec::new();
        if !options.quiet {
            lines.extend(events.iter().map(|event| plain.render_event(event)));
        }
        lines.extend(response.lines.iter().map(|line| plain.sanitize(line)));
        if !options.quiet {
            lines.push(format!(
                "Exec {status} · exit={exit_code} · elapsed={elapsed_millis}ms"
            ));
        }
        format!("{}\n", lines.join("\n"))
    };

    if let Some(path) = options.output.as_deref() {
        write_exec_output(path, rendered.as_bytes(), options.force)?;
    } else {
        io::stdout().lock().write_all(rendered.as_bytes())?;
    }
    if status != "completed" {
        return Err(format!("Exec {status}; exitCode={exit_code}").into());
    }
    Ok(())
}

fn write_exec_output(
    path: &Path,
    contents: &[u8],
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = target
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or("Exec output 的父目录不存在或不是目录")?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or("Exec output 文件名无效")?;
    let existing = fs::symlink_metadata(&target).ok();
    if let Some(metadata) = &existing {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err("Exec output 只能替换普通文件，不能是 symlink/directory".into());
        }
        if !force {
            return Err(format!(
                "Exec output 已存在：{}；显式传 --force 才允许替换",
                target.display()
            )
            .into());
        }
    }
    let suffix = format!("{}-{}", std::process::id(), unix_millis()?);
    let temporary = parent.join(format!(".{file_name}.kernary-new-{suffix}"));
    let backup = parent.join(format!(".{file_name}.kernary-backup-{suffix}"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    if existing.is_none() {
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        return Ok(());
    }
    fs::rename(&target, &backup)?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::rename(&backup, &target);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn run_interactive(
    mut backend: AppBackend,
    subscription: EventSubscription,
    ui_mode: UiMode,
    force_ascii: bool,
    force_no_color: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let is_tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    let capabilities = TerminalCapabilities::detect(is_tty, force_ascii, force_no_color);
    if matches!(ui_mode, UiMode::Full) || (matches!(ui_mode, UiMode::Auto) && is_tty) {
        let registry = backend.registry;
        run_tui(
            &mut backend,
            &subscription,
            registry,
            TuiOptions {
                ascii: !capabilities.unicode,
                color: capabilities.color,
            },
        )?;
        return Ok(());
    }
    run_plain_loop(&mut backend, &subscription, capabilities.style())
}

fn run_plain_loop(
    backend: &mut AppBackend,
    subscription: &EventSubscription,
    style: RenderStyle,
) -> Result<(), Box<dyn std::error::Error>> {
    let renderer = PlainRenderer::new(style);
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        let mut input = String::new();
        if stdin.read_line(&mut input)? == 0 {
            break;
        }
        let input = input.trim_end_matches(['\r', '\n']).to_owned();
        let mut response = backend.handle_input(&input);
        let background = backend.poll();
        response.lines.extend(background.lines);
        response.clear_view |= background.clear_view;
        response.should_exit |= background.should_exit;
        while let Ok(envelope) = subscription.try_recv() {
            writeln!(stdout, "{}", renderer.render_event(&envelope))?;
        }
        if response.clear_view {
            writeln!(stdout, "[CLEAR]")?;
        }
        for line in response.lines {
            writeln!(stdout, "{}", renderer.sanitize(&line))?;
        }
        if let Some(prompt) = response.secret_prompt {
            if !stdin.is_terminal() {
                writeln!(
                    stdout,
                    "! secure input requires TTY; use `kernary connect <provider>`"
                )?;
                let cancelled = backend.submit_secret(&prompt.request_id, String::new());
                for line in cancelled.lines {
                    writeln!(stdout, "{}", renderer.sanitize(&line))?;
                }
            } else {
                let secret = read_secret(&format!("{}: ", prompt.prompt))?;
                let submitted =
                    backend.submit_secret(&prompt.request_id, secret.expose_secret()?.to_owned());
                for line in submitted.lines {
                    writeln!(stdout, "{}", renderer.sanitize(&line))?;
                }
            }
        }
        if response.should_exit {
            break;
        }
        write!(stdout, "> ")?;
        stdout.flush()?;
    }
    while backend.background_team.is_some() {
        let response = backend.poll();
        for line in response.lines {
            writeln!(stdout, "{}", renderer.sanitize(&line))?;
        }
        while let Ok(envelope) = subscription.try_recv() {
            writeln!(stdout, "{}", renderer.render_event(&envelope))?;
        }
        if backend.background_team.is_some() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

fn doctor(
    project_root: &Path,
    json: bool,
    force_ascii: bool,
    force_no_color: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let capabilities =
        TerminalCapabilities::detect(io::stdout().is_terminal(), force_ascii, force_no_color);
    let store = SqliteKernelStore::open_in_memory()?;
    let embedding_model_configured = compat_env_var("HARNESS_EMBEDDING_MODEL")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let browser_configured = compat_env_var_os("HARNESS_BROWSER_PYTHON").is_some()
        && compat_env_var_os("HARNESS_BROWSER_EXECUTABLE").is_some()
        && compat_env_var("HARNESS_BROWSER_ALLOWED_ORIGINS")
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
    let lsp_config_path = configured_path(project_root, "KERNARY_LSP_CONFIG")
        .unwrap_or_else(|| default_lsp_config_path(project_root));
    let lsp_config_present = lsp_config_path.is_file();
    let lsp_config_valid =
        lsp_config_present && LspManager::load(&lsp_config_path, project_root).is_ok();
    let provider_catalog = load_provider_catalog(project_root)?;
    let provider_config_path = configured_path(project_root, "KERNARY_PROVIDER_CONFIG")
        .unwrap_or_else(|| default_project_catalog_path(project_root));
    let provider_model_cache_path = default_model_cache_path(project_root);
    let provider_model_cache = load_provider_model_cache(project_root);
    let providers = provider_catalog.list();
    let discovery_configured = providers
        .iter()
        .filter(|provider| provider.discovery.is_some())
        .count();
    let cached_providers = providers
        .iter()
        .filter(|provider| provider_model_cache.get(&provider.id).is_some())
        .count();
    let report = serde_json::json!({
        "schemaVersion": 1,
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "product": {
            "name": PRODUCT_NAME,
            "shortName": PRODUCT_SHORT_NAME,
            "mascot": MASCOT_NAME,
            "tagline": TAGLINE
        },
        "command": {
            "primary": "kernary",
            "invokedAs": invoked_command_name(),
            "compatibilityAliases": ["harness"]
        },
        "project": project_root.display().to_string(),
        "storageSchema": store.schema_version()?,
        "compatibility": {
            "stateDirectory": ".harness",
            "credentialService": "dev.openai.harness",
            "environmentPrimaryPrefix": "KERNARY_",
            "environmentLegacyPrefix": "HARNESS_"
        },
        "terminal": {
            "tty": capabilities.is_tty,
            "color": capabilities.color,
            "unicode": capabilities.unicode,
            "name": capabilities.terminal_name,
        },
        "model": "unconfigured",
        "provider": serde_json::Value::Null,
        "providers": {
            "catalogCount": providers.len(),
            "customConfigPresent": provider_config_path.is_file(),
            "protocols": ["openai-responses", "openai-chat", "anthropic-messages"],
            "discovery": {
                "configured": discovery_configured,
                "cached": cached_providers,
                "cachePresent": provider_model_cache_path.is_file(),
                "activationRule": "explicit-single-provider-refresh"
            }
        },
        "auth": {
            "osCredentialStore": OsCredentialStore::available().is_ok(),
            "credentialService": "dev.openai.harness",
            "nativeBrowserOAuth": "disabled",
            "codexDelegated": true
        },
        "extensions": {
            "mcp": "oauth-pkce-streamable-http-legacy-sse-lazy",
            "plugin": "isolated-process-lazy",
            "skill": "metadata-first-lazy",
            "agents": "stage-11-complete-role-dag-tools-approval-recovery",
            "providers": "catalog-protocol-mux-secure-connect-dynamic-discovery",
            "lsp": "safe-workspace-edit-preview-filelease-patchset-3.18",
            "command": "kernary-primary-harness-byte-identical-alias",
            "observability": "bounded-event-log-traceid-profile-why-inspect",
            "automation": "strict-noninteractive-exec-single-json-atomic-output",
            "sessionControl": "list-goal-history-clear-checkpoint-reset-forget",
            "management": "persistent-mcp-crud-permission-modes-and-rules"
        },
        "vector": {
            "activationRule": "requires-non-empty-embedding-model",
            "configured": embedding_model_configured
        },
        "browser": {
            "configured": browser_configured,
            "activationRule": "explicit-open-with-python-browser-origin-config"
        },
        "lsp": {
            "configured": lsp_config_present,
            "valid": lsp_config_valid,
            "protocol": "3.18",
            "activationRule": "explicit-start-query-or-approved-on-demand-tool",
            "toolBridge": "read-only-process-spawn-permission",
            "positionModel": "human-scalar-1-based-to-negotiated-protocol-units",
            "repositoryFusion": "file-hash-bound-symbols-diagnostics-evidence",
            "patchPreview": "rename-codeaction-preview-second-approval-recoverable-set"
        },
        "stage": 20,
        "stageTrack": "24-settings-vector-doctor-depth",
    });
    if json {
        println!("{report}");
    } else {
        println!("{PRODUCT_SHORT_NAME} doctor: OK");
        println!("Product: {PRODUCT_NAME}");
        println!("{TAGLINE}");
        println!("Storage schema: {}", store.schema_version()?);
        println!("Terminal TTY: {}", capabilities.is_tty);
        println!("Model: unconfigured (由 Session 或 --model 在运行时选择)");
    }
    Ok(())
}
