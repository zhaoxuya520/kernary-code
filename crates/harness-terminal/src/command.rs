use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_permission::GrantScope;
use harness_types::ReasoningLevel;

use crate::UiLanguage;
use crate::language::command_description;

/// 当前已真实接线的 Slash Commands；未实现的命令不能提前出现在 Registry。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Account,
    AgentShow {
        agent_id: String,
    },
    AgentMd {
        operation: AgentMdCommand,
    },
    Agents {
        mode: AgentDisplayMode,
    },
    Budget {
        operation: BudgetCommand,
    },
    Browser {
        operation: BrowserCommand,
    },
    Approve {
        invocation_id: String,
        scope: GrantScope,
    },
    Cache,
    Checkpoint {
        name: Option<String>,
    },
    Compact {
        mode: CompactCommandMode,
    },
    Config,
    Connect {
        provider: Option<String>,
    },
    Context,
    DenyTool {
        invocation_id: String,
    },
    Debug,
    Diff,
    Doctor,
    Focus {
        value: Option<String>,
    },
    Failover {
        operation: FailoverCommand,
    },
    Forget {
        id: String,
    },
    Fork {
        checkpoint_id: String,
        child_session_id: Option<String>,
    },
    Git {
        operation: GitCommand,
    },
    Help {
        command: Option<String>,
    },
    Inspect {
        target: String,
    },
    GoalShow,
    GoalSet {
        text: String,
    },
    GoalLock {
        locked: bool,
    },
    GoalClear,
    GoalHistory {
        limit: usize,
    },
    Lsp {
        operation: LspCommand,
    },
    Memory {
        operation: MemoryCommand,
    },
    Mcp {
        operation: McpCommand,
    },
    Logout {
        provider: String,
    },
    Logs {
        limit: usize,
    },
    ModelShow,
    ModelSelect {
        provider: String,
        model: String,
    },
    ModelSelectCurrent {
        model: String,
    },
    Mode {
        mode: Option<String>,
    },
    Models {
        refresh: bool,
        provider: Option<String>,
    },
    Plan,
    PatchList,
    Queue {
        operation: QueueCommand,
    },
    Index {
        operation: IndexCommand,
    },
    Pin {
        value: String,
    },
    Plugins {
        operation: PluginCommand,
    },
    Permissions {
        operation: PermissionCommand,
    },
    Rollback {
        checkpoint_id: String,
    },
    Provider {
        operation: ProviderCommand,
    },
    Providers,
    Profile,
    Reasoning {
        level: ReasoningLevel,
    },
    Review {
        staged: bool,
    },
    RetryTool {
        invocation_id: String,
    },
    Resume,
    Reset,
    Sandbox {
        mode: Option<String>,
    },
    Status,
    Steer {
        instruction: String,
    },
    Team {
        operation: TeamCommand,
    },
    Skills {
        operation: SkillCommand,
    },
    Test {
        arguments: Vec<String>,
    },
    Tools,
    Trace {
        operation: TraceCommand,
    },
    Session,
    Sessions,
    SessionSwitch {
        target: String,
    },
    SessionNew,
    SessionRename {
        title: String,
    },
    Settings {
        operation: SettingsCommand,
    },
    Undo {
        patch_id: Option<String>,
    },
    Vector {
        operation: VectorCommand,
    },
    Why,
    Language {
        language: Option<UiLanguage>,
    },
    Clear,
    Exit,
}

/// Terminal 允许的显式压缩模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactCommandMode {
    Auto,
    Safe,
    Aggressive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitCommand {
    Status,
    Diff,
    Log,
    Branch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpCommand {
    List,
    AddStdio {
        server_id: String,
        command: String,
        args: Vec<String>,
    },
    AddHttp {
        server_id: String,
        endpoint: String,
    },
    Remove {
        server_id: String,
    },
    Enable {
        server_id: String,
    },
    Disable {
        server_id: String,
    },
    AuthStart {
        server_id: String,
    },
    AuthFinish {
        server_id: String,
    },
    AuthRefresh {
        server_id: String,
    },
    AuthStatus {
        server_id: String,
    },
    Connect {
        server_id: String,
        force: bool,
    },
    Disconnect {
        server_id: String,
    },
    Tools {
        server_id: String,
    },
    Resources {
        server_id: String,
    },
    Prompts {
        server_id: String,
    },
    Poll {
        server_id: String,
    },
    Read {
        server_id: String,
        uri: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspCommand {
    List,
    Start {
        server_id: String,
    },
    Stop {
        server_id: String,
    },
    Symbols {
        server_id: String,
        path: String,
    },
    Definition {
        server_id: String,
        path: String,
        line: u32,
        character: u32,
    },
    References {
        server_id: String,
        path: String,
        line: u32,
        character: u32,
    },
    Diagnostics {
        server_id: String,
        path: String,
    },
    RenamePreview {
        server_id: String,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    CodeActionPreview {
        server_id: String,
        path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        action_index: usize,
        only: Option<String>,
    },
    ApplyPreview {
        preview_id: String,
    },
    UndoPreview {
        preview_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginCommand {
    List,
    Review {
        plugin_id: String,
    },
    Enable {
        plugin_id: String,
        review_hash: String,
    },
    Disable {
        plugin_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillCommand {
    List,
    Search { query: String },
    Load { skill_id: String },
    Unload { skill_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCommand {
    Stats,
    Search {
        mode: String,
        query: String,
    },
    Add {
        kind: String,
        title: String,
        content: String,
        tags: Vec<String>,
    },
    Forget {
        id: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexCommand {
    Status,
    Update,
    Clear,
    Map,
    Search { query: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VectorCommand {
    Status,
    Purge,
    Mode { mode: String },
    Setup,
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderCommand {
    Show,
    Add,
    Switch,
    Remove { provider_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingLayer {
    Session,
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsCommand {
    Show {
        key: Option<String>,
    },
    Set {
        key: String,
        value: String,
        layer: SettingLayer,
    },
    Reset {
        key: String,
        layer: SettingLayer,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceCommand {
    Status,
    On,
    Off,
    Last { limit: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionCommand {
    Show,
    Mode {
        mode: String,
    },
    RuleList,
    RuleAdd {
        effect: String,
        action: String,
        pattern: String,
    },
    RuleRemove {
        rule_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMdCommand {
    Status,
    Show,
    InitProject,
    InitGlobal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailoverCommand {
    Status,
    Off,
    On { targets: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueCommand {
    Status,
    Cancel { task_id: String },
    Priority { task_id: String, priority: i32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetCommand {
    Show,
    Set { field: String, value: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserCommand {
    Status,
    Open,
    Navigate { url: String },
    Actions,
    Handoff,
    Reclaim,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDisplayMode {
    Normal,
    Verbose,
    Compact,
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamCommand {
    Status,
    Create {
        count: usize,
        objective: Option<String>,
    },
    Workflow {
        workers: usize,
        objective: Option<String>,
    },
    Adaptive {
        workers: usize,
        objective: Option<String>,
    },
}

/// 普通文本或 Slash Command。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedInput {
    Text(String),
    Command(SlashCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandParseError {
    pub code: &'static str,
    pub message: String,
}

impl Display for CommandParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for CommandParseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandSpec {
    name: &'static str,
    synopsis: &'static str,
    description: &'static str,
}

/// 输入候选同时携带替换文本和帮助信息，避免 TUI 只显示裸命令名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSuggestion {
    pub replacement: String,
    pub label: String,
    pub description: String,
}

impl InputSuggestion {
    #[must_use]
    pub fn new(
        replacement: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            replacement: replacement.into(),
            label: label.into(),
            description: description.into(),
        }
    }
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/agent",
        synopsis: "/agent <agent-id>",
        description: "查看一个 Agent 的角色、能力、工具边界与生命周期",
    },
    CommandSpec {
        name: "/agentmd",
        synopsis: "/agentmd [status|show|init-project|init-global]",
        description: "管理全局或项目私有 agent.md；项目文件存在时覆盖全局文件",
    },
    CommandSpec {
        name: "/agents",
        synopsis: "/agents [normal|verbose|compact|tree]",
        description: "以不同密度列出内置 Agent、状态与控制面关系",
    },
    CommandSpec {
        name: "/approve",
        synopsis: "/approve <invocation-id> [once|run|project]",
        description: "批准等待中的 Tool Invocation",
    },
    CommandSpec {
        name: "/account",
        synopsis: "/account",
        description: "显示活跃账户元数据，不显示 secret",
    },
    CommandSpec {
        name: "/cache",
        synopsis: "/cache",
        description: "显示 L1/L2 Cache 命中、写入与淘汰指标",
    },
    CommandSpec {
        name: "/budget",
        synopsis: "/budget [agents|parallel|tokens|tools|runtime-ms|retries|cost <value>]",
        description: "查看或设置当前 Runtime 的 Agent 硬预算",
    },
    CommandSpec {
        name: "/browser",
        synopsis: "/browser [status|open|navigate <url>|actions|handoff|reclaim|close]",
        description: "管理独立 profile 的 Browser Runtime；不会隐式启动",
    },
    CommandSpec {
        name: "/checkpoint",
        synopsis: "/checkpoint [name]",
        description: "创建 durable Context checkpoint",
    },
    CommandSpec {
        name: "/clear",
        synopsis: "/clear",
        description: "清空当前终端 viewport，不删除 Session",
    },
    CommandSpec {
        name: "/compact",
        synopsis: "/compact [auto|now|safe|aggressive]",
        description: "先 checkpoint，再安全或强力压缩 Context",
    },
    CommandSpec {
        name: "/connect",
        synopsis: "/connect [provider]",
        description: "安全输入 Provider API key；Key 不进入 Slash 历史",
    },
    CommandSpec {
        name: "/config",
        synopsis: "/config",
        description: "显示五层有效配置、配置文件路径与每项来源",
    },
    CommandSpec {
        name: "/context",
        synopsis: "/context",
        description: "显示 Context Series、预算、筛选和 Checkpoint",
    },
    CommandSpec {
        name: "/deny",
        synopsis: "/deny <invocation-id>",
        description: "拒绝等待中的 Tool Invocation",
    },
    CommandSpec {
        name: "/debug",
        synopsis: "/debug",
        description: "显示 Kernel/Context/Agent/Tool/Memory/Cache 的只读调试快照",
    },
    CommandSpec {
        name: "/diff",
        synopsis: "/diff",
        description: "显示当前 Git diff",
    },
    CommandSpec {
        name: "/doctor",
        synopsis: "/doctor",
        description: "在当前 Session 内执行只读运行时诊断",
    },
    CommandSpec {
        name: "/exit",
        synopsis: "/exit",
        description: "保存当前状态并退出",
    },
    CommandSpec {
        name: "/focus",
        synopsis: "/focus <path|symbol|clear>",
        description: "设置或清除 durable 检索焦点",
    },
    CommandSpec {
        name: "/failover",
        synopsis: "/failover [status|off|on --confirm-cost <provider/model>...]",
        description: "管理精确模型 Failover allowlist；默认关闭，启用必须确认成本范围",
    },
    CommandSpec {
        name: "/forget",
        synopsis: "/forget <memory-id>",
        description: "从 Project Memory 删除指定记录；/memory forget 的快捷方式",
    },
    CommandSpec {
        name: "/fork",
        synopsis: "/fork <checkpoint-id> [child-session-id]",
        description: "从 Checkpoint 创建独立 Child Session",
    },
    CommandSpec {
        name: "/git",
        synopsis: "/git [status|diff|log|branch]",
        description: "通过 allowlisted git executable 执行只读 Git 操作",
    },
    CommandSpec {
        name: "/goal",
        synopsis: "/goal [set|add|edit <text>|clear|history [1..200]|lock|unlock]",
        description: "查看、修订、清除、追溯或锁定 durable Goal",
    },
    CommandSpec {
        name: "/help",
        synopsis: "/help [command]",
        description: "显示命令帮助",
    },
    CommandSpec {
        name: "/inspect",
        synopsis: "/inspect <session|config|context|memory|vector|repository|cache|tool|agent|plan>",
        description: "统一查看一个 Harness 子系统的当前只读状态",
    },
    CommandSpec {
        name: "/language",
        synopsis: "/language [en|zh-CN|zh-TW|ja]",
        description: "切换高度定制的终端语言包并持久化到当前 Session",
    },
    CommandSpec {
        name: "/lsp",
        synopsis: "/lsp [start|stop|symbols|definition|references|diagnostics|rename|code-action|apply|undo] ...",
        description: "按需查询 LSP；写操作只能 preview 后二次审批应用",
    },
    CommandSpec {
        name: "/memory",
        synopsis: "/memory [stats|search|add|forget] ...",
        description: "项目结构化记忆与 lexical/semantic 检索",
    },
    CommandSpec {
        name: "/index",
        synopsis: "/index [status|build|update|clear|map|search]",
        description: "增量 Repository Index",
    },
    CommandSpec {
        name: "/mcp",
        synopsis: "/mcp [add-stdio|add-http|remove|enable|disable|auth|connect|disconnect|tools|resources|prompts|read] ...",
        description: "管理 lazy MCP Server；add/remove/enable/disable 原子持久化到项目 MCP TOML",
    },
    CommandSpec {
        name: "/logout",
        synopsis: "/logout [openai]",
        description: "从 OS Credential Store 删除 Provider 凭证",
    },
    CommandSpec {
        name: "/logs",
        synopsis: "/logs [1..200]",
        description: "显示当前进程最近的安全 Event 日志，不含 secret/CoT",
    },
    CommandSpec {
        name: "/model",
        synopsis: "/model [provider/model]",
        description: "查看或切换当前 Session Model",
    },
    CommandSpec {
        name: "/models",
        synopsis: "/models [refresh <provider>]",
        description: "列出模型；只有显式 refresh 单个 Provider 才访问网络目录",
    },
    CommandSpec {
        name: "/mode",
        synopsis: "/mode [lite|balanced|full|custom]",
        description: "查看或持久化切换当前 Session 的真实运行档位",
    },
    CommandSpec {
        name: "/plan",
        synopsis: "/plan",
        description: "显示当前 Task Plan 摘要",
    },
    CommandSpec {
        name: "/pin",
        synopsis: "/pin <path|text|ref>",
        description: "添加不可压缩的 Pinned Context",
    },
    CommandSpec {
        name: "/patch",
        synopsis: "/patch list",
        description: "列出 durable PatchQueue 与恢复状态",
    },
    CommandSpec {
        name: "/permissions",
        synopsis: "/permissions [manual|edit|auto|full|bypass|custom|rules|rule add <effect> <action> <pattern>|rule remove <id>]",
        description: "管理 Permission 模式与持久规则；任何规则都不能绕过 Sandbox hard deny",
    },
    CommandSpec {
        name: "/plugins",
        synopsis: "/plugins [review|enable|disable] ...",
        description: "查看、审批并激活隔离进程 Plugin",
    },
    CommandSpec {
        name: "/provider",
        synopsis: "/provider [add|switch|remove <provider-id>]",
        description: "显示、添加、切换或删除文本模型提供商",
    },
    CommandSpec {
        name: "/providers",
        synopsis: "/providers",
        description: "列出内置与项目 Provider Catalog 及 credential 状态",
    },
    CommandSpec {
        name: "/profile",
        synopsis: "/profile",
        description: "显示启动、模型、工具、检索、向量和 Context 的真实延迟采样",
    },
    CommandSpec {
        name: "/queue",
        synopsis: "/queue [cancel <task-id>|priority <task-id> <-100..100>]",
        description: "查看任务队列，或取消节点、调整 durable 优先级",
    },
    CommandSpec {
        name: "/reasoning",
        synopsis: "/reasoning <off|minimal|low|medium|high|xhigh|max>",
        description: "设置 reasoning；不支持时显示 capability clamp",
    },
    CommandSpec {
        name: "/review",
        synopsis: "/review [unstaged|staged]",
        description: "生成受控 Git diff 作为 Review 输入",
    },
    CommandSpec {
        name: "/rollback",
        synopsis: "/rollback <checkpoint-id>",
        description: "从 Checkpoint 创建新的 Context Series",
    },
    CommandSpec {
        name: "/retry",
        synopsis: "/retry <tool-invocation-id>",
        description: "只重试 read-only/idempotent Failed Tool",
    },
    CommandSpec {
        name: "/resume",
        synopsis: "/resume",
        description: "显式恢复租约已过期的 recoverable Agent Team",
    },
    CommandSpec {
        name: "/reset",
        synopsis: "/reset [context]",
        description: "先 checkpoint，再清理 Context；保留 Goal 与 pinned/hard-required 项",
    },
    CommandSpec {
        name: "/sandbox",
        synopsis: "/sandbox [read-only|workspace-write|danger-full-access|network-on|network-off]",
        description: "显示或切换系统级 Sandbox；危险模式需要再次确认",
    },
    CommandSpec {
        name: "/session",
        synopsis: "/session [list|new|switch <id-or-title>|rename <title>]",
        description: "选择、创建、切换或重命名当前项目的 Session",
    },
    CommandSpec {
        name: "/sessions",
        synopsis: "/sessions [list]",
        description: "列出当前项目所有 durable Session 与 fork lineage",
    },
    CommandSpec {
        name: "/settings",
        synopsis: "/settings [key|set <key> <value> [session|runtime]|reset <key> [session|runtime]]",
        description: "查看或修改 Session/Runtime 设置；非法组合会原子回滚",
    },
    CommandSpec {
        name: "/status",
        synopsis: "/status",
        description: "显示 Model/Mode/Goal/Session 状态",
    },
    CommandSpec {
        name: "/steer",
        synopsis: "/steer <instruction>",
        description: "向 Supervisor 投递 durable SteeringMessage",
    },
    CommandSpec {
        name: "/team",
        synopsis: "/team [create <2..8>|workflow|adaptive <1..4> [objective]]",
        description: "显示团队状态，或启动研究、精简工作流与能力路由 Adaptive Evidence DAG",
    },
    CommandSpec {
        name: "/skills",
        synopsis: "/skills [search|load|unload] ...",
        description: "搜索 metadata 并按需加载 Skill 正文",
    },
    CommandSpec {
        name: "/think",
        synopsis: "/think <level>",
        description: "/reasoning 的别名",
    },
    CommandSpec {
        name: "/test",
        synopsis: "/test [args...]",
        description: "运行 HARNESS_TEST_EXECUTABLE",
    },
    CommandSpec {
        name: "/tools",
        synopsis: "/tools",
        description: "列出 Tool Registry 与 effect class",
    },
    CommandSpec {
        name: "/trace",
        synopsis: "/trace [status|on|off|last [1..200]]",
        description: "控制 Runtime Trace，并查看带 TraceId 的最近事件",
    },
    CommandSpec {
        name: "/undo",
        synopsis: "/undo [patch-id|list]",
        description: "列出或安全撤销最近/指定的 Harness Patch",
    },
    CommandSpec {
        name: "/vector",
        synopsis: "/vector [status|setup|clear|on|off|auto|purge]",
        description: "配置并验证单一 Embedding Provider，或管理向量检索偏好",
    },
    CommandSpec {
        name: "/why",
        synopsis: "/why",
        description: "显示 Goal、Context 来源和 Tool 证据摘要，不显示私有思维链",
    },
];

/// 统一 Command Registry；解析和补全不访问网络或 Runtime。
#[derive(Clone, Copy, Debug)]
pub struct CommandRegistry {
    language: UiLanguage,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            language: UiLanguage::ZhCn,
        }
    }

    #[must_use]
    pub const fn with_language(language: UiLanguage) -> Self {
        Self { language }
    }

    pub fn parse(&self, input: &str) -> Result<ParsedInput, CommandParseError> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return Ok(ParsedInput::Text(trimmed.to_owned()));
        }
        let mut parts = trimmed.split_whitespace();
        let command = parts.next().unwrap_or_default();
        let remainder = trimmed
            .strip_prefix(command)
            .map(str::trim)
            .unwrap_or_default();
        let parsed = match command {
            "/account" if remainder.is_empty() => SlashCommand::Account,
            "/agent" if !remainder.is_empty() => SlashCommand::AgentShow {
                agent_id: remainder.to_owned(),
            },
            "/agentmd" => SlashCommand::AgentMd {
                operation: match remainder {
                    "" | "status" => AgentMdCommand::Status,
                    "show" => AgentMdCommand::Show,
                    "init-project" => AgentMdCommand::InitProject,
                    "init-global" => AgentMdCommand::InitGlobal,
                    _ => {
                        return Err(CommandParseError {
                            code: "invalid-agentmd-command",
                            message: "用法：/agentmd [status|show|init-project|init-global]"
                                .to_owned(),
                        });
                    }
                },
            },
            "/agents" if matches!(remainder, "" | "normal") => SlashCommand::Agents {
                mode: AgentDisplayMode::Normal,
            },
            "/agents" if remainder == "verbose" => SlashCommand::Agents {
                mode: AgentDisplayMode::Verbose,
            },
            "/agents" if remainder == "compact" => SlashCommand::Agents {
                mode: AgentDisplayMode::Compact,
            },
            "/agents" if remainder == "tree" => SlashCommand::Agents {
                mode: AgentDisplayMode::Tree,
            },
            "/agents" => SlashCommand::Team {
                operation: parse_team(&format!("create {remainder}"))?,
            },
            "/budget" => SlashCommand::Budget {
                operation: parse_budget(remainder)?,
            },
            "/browser" => SlashCommand::Browser {
                operation: parse_browser(remainder)?,
            },
            "/approve" if !remainder.is_empty() => {
                let arguments = remainder.split_whitespace().collect::<Vec<_>>();
                if arguments.len() > 2 {
                    return Err(CommandParseError {
                        code: "invalid-approve-arguments",
                        message: "用法：/approve <invocation-id> [once|run|project]".to_owned(),
                    });
                }
                SlashCommand::Approve {
                    invocation_id: arguments[0].to_owned(),
                    scope: match arguments.get(1).copied().unwrap_or("once") {
                        "once" => GrantScope::Once,
                        "run" => GrantScope::Run,
                        "project" => GrantScope::Project,
                        _ => {
                            return Err(CommandParseError {
                                code: "invalid-grant-scope",
                                message: "Grant scope 仅支持 once/run/project".to_owned(),
                            });
                        }
                    },
                }
            }
            "/cache" if remainder.is_empty() => SlashCommand::Cache,
            "/checkpoint" => SlashCommand::Checkpoint {
                name: (!remainder.is_empty()).then(|| remainder.to_owned()),
            },
            "/compact" if matches!(remainder, "" | "now" | "safe") => SlashCommand::Compact {
                mode: CompactCommandMode::Safe,
            },
            "/compact" if remainder == "auto" => SlashCommand::Compact {
                mode: CompactCommandMode::Auto,
            },
            "/compact" if remainder == "aggressive" => SlashCommand::Compact {
                mode: CompactCommandMode::Aggressive,
            },
            "/compact" => {
                return Err(CommandParseError {
                    code: "invalid-compact-mode",
                    message: "Compact mode 仅支持 auto、now、safe、aggressive".to_owned(),
                });
            }
            "/connect" => SlashCommand::Connect {
                provider: (!remainder.is_empty()).then(|| remainder.to_owned()),
            },
            "/config" if remainder.is_empty() => SlashCommand::Config,
            "/context" if remainder.is_empty() => SlashCommand::Context,
            "/deny" if !remainder.is_empty() => SlashCommand::DenyTool {
                invocation_id: remainder.to_owned(),
            },
            "/debug" if remainder.is_empty() => SlashCommand::Debug,
            "/diff" if remainder.is_empty() => SlashCommand::Diff,
            "/doctor" if remainder.is_empty() => SlashCommand::Doctor,
            "/focus" if remainder == "clear" => SlashCommand::Focus { value: None },
            "/focus" if !remainder.is_empty() => SlashCommand::Focus {
                value: Some(remainder.to_owned()),
            },
            "/failover" => SlashCommand::Failover {
                operation: parse_failover(remainder)?,
            },
            "/forget" if !remainder.is_empty() && remainder.split_whitespace().count() == 1 => {
                SlashCommand::Forget {
                    id: remainder.to_owned(),
                }
            }
            "/fork" if !remainder.is_empty() => {
                let arguments = remainder.split_whitespace().collect::<Vec<_>>();
                if arguments.len() > 2 {
                    return Err(CommandParseError {
                        code: "invalid-fork-arguments",
                        message: "用法：/fork <checkpoint-id> [child-session-id]".to_owned(),
                    });
                }
                SlashCommand::Fork {
                    checkpoint_id: arguments[0].to_owned(),
                    child_session_id: arguments.get(1).map(|value| (*value).to_owned()),
                }
            }
            "/git" if matches!(remainder, "" | "status") => SlashCommand::Git {
                operation: GitCommand::Status,
            },
            "/git" if remainder == "diff" => SlashCommand::Git {
                operation: GitCommand::Diff,
            },
            "/git" if remainder == "log" => SlashCommand::Git {
                operation: GitCommand::Log,
            },
            "/git" if remainder == "branch" => SlashCommand::Git {
                operation: GitCommand::Branch,
            },
            "/git" => {
                return Err(CommandParseError {
                    code: "invalid-git-command",
                    message: "Git 仅支持 status/diff/log/branch".to_owned(),
                });
            }
            "/help" => SlashCommand::Help {
                command: parts.next().map(str::to_owned),
            },
            "/inspect" => SlashCommand::Inspect {
                target: parse_inspect(remainder)?,
            },
            "/goal" if remainder.is_empty() => SlashCommand::GoalShow,
            "/goal" if remainder == "lock" => SlashCommand::GoalLock { locked: true },
            "/goal" if remainder == "unlock" => SlashCommand::GoalLock { locked: false },
            "/goal" if remainder == "clear" => SlashCommand::GoalClear,
            "/goal" if remainder == "history" => SlashCommand::GoalHistory { limit: 20 },
            "/goal" if remainder.starts_with("history ") => SlashCommand::GoalHistory {
                limit: parse_bounded_limit(&remainder[8..], 20, "goal history")?,
            },
            "/goal"
                if remainder.starts_with("set ")
                    || remainder.starts_with("add ")
                    || remainder.starts_with("edit ") =>
            {
                SlashCommand::GoalSet {
                    text: remainder[4..].trim().to_owned(),
                }
            }
            "/goal" => SlashCommand::GoalSet {
                text: remainder.to_owned(),
            },
            "/lsp" => SlashCommand::Lsp {
                operation: parse_lsp(remainder)?,
            },
            "/memory" => SlashCommand::Memory {
                operation: parse_memory(remainder)?,
            },
            "/index" => SlashCommand::Index {
                operation: parse_index(remainder)?,
            },
            "/mcp" => SlashCommand::Mcp {
                operation: parse_mcp(remainder)?,
            },
            "/logout" => SlashCommand::Logout {
                provider: if remainder.is_empty() {
                    "openai".to_owned()
                } else {
                    remainder.to_owned()
                },
            },
            "/logs" => SlashCommand::Logs {
                limit: parse_bounded_limit(remainder, 50, "logs")?,
            },
            "/model" if remainder.is_empty() => SlashCommand::ModelShow,
            "/model" => {
                if let Some((provider, model)) = remainder.split_once('/') {
                    if provider.trim().is_empty() || model.trim().is_empty() {
                        return Err(CommandParseError {
                            code: "invalid-model-selection",
                            message: "Provider/Model 不能为空".to_owned(),
                        });
                    }
                    SlashCommand::ModelSelect {
                        provider: provider.to_owned(),
                        model: model.to_owned(),
                    }
                } else {
                    SlashCommand::ModelSelectCurrent {
                        model: remainder.to_owned(),
                    }
                }
            }
            "/models" if remainder.is_empty() => SlashCommand::Models {
                refresh: false,
                provider: None,
            },
            "/models" if remainder == "refresh" => {
                return Err(CommandParseError {
                    code: "model-refresh-provider-required",
                    message: "用法：/models refresh <provider>".to_owned(),
                });
            }
            "/models" if remainder.starts_with("refresh ") => {
                let provider = remainder[8..].trim();
                if provider.is_empty() || provider.split_whitespace().count() != 1 {
                    return Err(CommandParseError {
                        code: "model-refresh-provider-invalid",
                        message: "用法：/models refresh <provider>".to_owned(),
                    });
                }
                SlashCommand::Models {
                    refresh: true,
                    provider: Some(provider.to_owned()),
                }
            }
            "/mode" if remainder.is_empty() => SlashCommand::Mode { mode: None },
            "/mode" if matches!(remainder, "lite" | "balanced" | "full" | "custom") => {
                SlashCommand::Mode {
                    mode: Some(remainder.to_owned()),
                }
            }
            "/mode" => {
                return Err(CommandParseError {
                    code: "invalid-runtime-mode",
                    message: "Mode 仅支持 lite/balanced/full/custom".to_owned(),
                });
            }
            "/plan" => SlashCommand::Plan,
            "/patch" if remainder == "list" => SlashCommand::PatchList,
            "/pin" if !remainder.is_empty() => SlashCommand::Pin {
                value: remainder.to_owned(),
            },
            "/permissions" if remainder.is_empty() => SlashCommand::Permissions {
                operation: PermissionCommand::Show,
            },
            "/permissions"
                if matches!(
                    remainder,
                    "manual"
                        | "accept-edits"
                        | "edit"
                        | "auto"
                        | "full"
                        | "bypass"
                        | "safe"
                        | "ask"
                        | "custom"
                ) =>
            {
                SlashCommand::Permissions {
                    operation: PermissionCommand::Mode {
                        mode: remainder.to_owned(),
                    },
                }
            }
            "/permissions" => SlashCommand::Permissions {
                operation: parse_permission(remainder)?,
            },
            "/plugins" => SlashCommand::Plugins {
                operation: parse_plugin(remainder)?,
            },
            "/provider" if remainder.is_empty() => SlashCommand::Provider {
                operation: ProviderCommand::Show,
            },
            "/provider" if remainder == "add" => SlashCommand::Provider {
                operation: ProviderCommand::Add,
            },
            "/provider" if remainder == "switch" => SlashCommand::Provider {
                operation: ProviderCommand::Switch,
            },
            "/provider" if remainder.starts_with("remove ") => SlashCommand::Provider {
                operation: ProviderCommand::Remove {
                    provider_id: remainder[7..].trim().to_owned(),
                },
            },
            "/providers" if remainder.is_empty() => SlashCommand::Providers,
            "/profile" if remainder.is_empty() => SlashCommand::Profile,
            "/queue" => SlashCommand::Queue {
                operation: parse_queue(remainder)?,
            },
            "/reasoning" | "/think" => SlashCommand::Reasoning {
                level: parse_reasoning(remainder)?,
            },
            "/review" if matches!(remainder, "" | "unstaged") => {
                SlashCommand::Review { staged: false }
            }
            "/review" if remainder == "staged" => SlashCommand::Review { staged: true },
            "/rollback" if !remainder.is_empty() => SlashCommand::Rollback {
                checkpoint_id: remainder.to_owned(),
            },
            "/retry" if !remainder.is_empty() => SlashCommand::RetryTool {
                invocation_id: remainder.to_owned(),
            },
            "/resume" if remainder.is_empty() => SlashCommand::Resume,
            "/reset" if matches!(remainder, "" | "context") => SlashCommand::Reset,
            "/sandbox" if remainder.is_empty() => SlashCommand::Sandbox { mode: None },
            "/sandbox"
                if matches!(
                    remainder,
                    "read-only"
                        | "workspace-write"
                        | "danger-full-access"
                        | "network-on"
                        | "network-off"
                ) =>
            {
                SlashCommand::Sandbox {
                    mode: Some(remainder.to_owned()),
                }
            }
            "/sandbox" => {
                return Err(CommandParseError {
                    code: "invalid-sandbox-mode",
                    message: "Sandbox 仅支持 read-only/workspace-write/danger-full-access/network-on/network-off"
                        .to_owned(),
                });
            }
            "/skills" => SlashCommand::Skills {
                operation: parse_skill(remainder)?,
            },
            "/status" => SlashCommand::Status,
            "/steer" if !remainder.is_empty() => SlashCommand::Steer {
                instruction: remainder.to_owned(),
            },
            "/team" => SlashCommand::Team {
                operation: parse_team(remainder)?,
            },
            "/test" => SlashCommand::Test {
                arguments: remainder.split_whitespace().map(str::to_owned).collect(),
            },
            "/tools" if remainder.is_empty() => SlashCommand::Tools,
            "/trace" => SlashCommand::Trace {
                operation: parse_trace(remainder)?,
            },
            "/undo" if remainder == "list" => SlashCommand::PatchList,
            "/undo" => SlashCommand::Undo {
                patch_id: (!remainder.is_empty()).then(|| remainder.to_owned()),
            },
            "/vector" => SlashCommand::Vector {
                operation: parse_vector(remainder)?,
            },
            "/why" if remainder.is_empty() => SlashCommand::Why,
            "/language" if remainder.is_empty() => SlashCommand::Language { language: None },
            "/language" => SlashCommand::Language {
                language: Some(
                    UiLanguage::parse(remainder).ok_or_else(|| CommandParseError {
                        code: "invalid-language",
                        message: "Language 仅支持 en/zh-CN/zh-TW/ja".to_owned(),
                    })?,
                ),
            },
            "/session" if remainder.is_empty() => SlashCommand::Session,
            "/session" if remainder == "list" => SlashCommand::Sessions,
            "/session" if remainder == "new" => SlashCommand::SessionNew,
            "/session" if let Some(title) = remainder.strip_prefix("rename ") => {
                SlashCommand::SessionRename {
                    title: title.trim().to_owned(),
                }
            }
            "/session" => SlashCommand::SessionSwitch {
                target: remainder
                    .strip_prefix("switch ")
                    .unwrap_or(remainder)
                    .trim()
                    .to_owned(),
            },
            "/sessions" if matches!(remainder, "" | "list") => SlashCommand::Sessions,
            "/settings" => SlashCommand::Settings {
                operation: parse_settings(remainder)?,
            },
            "/clear" => SlashCommand::Clear,
            "/exit" => SlashCommand::Exit,
            _ => {
                return Err(CommandParseError {
                    code: "unknown-command",
                    message: format!("未知命令：{command}；使用 /help"),
                });
            }
        };
        if matches!(&parsed, SlashCommand::GoalSet { text } if text.trim().is_empty()) {
            return Err(CommandParseError {
                code: "empty-goal",
                message: "Goal 不能为空".to_owned(),
            });
        }
        Ok(ParsedInput::Command(parsed))
    }

    #[must_use]
    pub fn complete(&self, prefix: &str) -> Vec<String> {
        let normalized = prefix.trim();
        let mut matches = COMMANDS
            .iter()
            .filter(|spec| spec.name.starts_with(normalized))
            .map(|spec| spec.name.to_owned())
            .collect::<Vec<_>>();
        matches.sort();
        matches
    }

    /// 返回可滚动 Slash 面板使用的完整候选。
    ///
    /// 根命令候选包含 synopsis 与说明；常用枚举参数也可以继续补全。
    #[must_use]
    pub fn suggestions(&self, input: &str) -> Vec<InputSuggestion> {
        let normalized = input.trim_start();
        if !normalized.starts_with('/') {
            return Vec::new();
        }
        if normalized.contains(char::is_whitespace) {
            return argument_suggestions(normalized);
        }
        COMMANDS
            .iter()
            .filter(|spec| spec.name.starts_with(normalized))
            .map(|spec| {
                let replacement = if spec.synopsis == spec.name {
                    spec.name.to_owned()
                } else {
                    format!("{} ", spec.name)
                };
                InputSuggestion::new(
                    replacement,
                    spec.synopsis,
                    command_description(self.language, spec.name, spec.description),
                )
            })
            .collect()
    }

    #[must_use]
    pub fn help(&self, command: Option<&str>) -> Vec<String> {
        if let Some(command) = command {
            let normalized = if command.starts_with('/') {
                command.to_owned()
            } else {
                format!("/{command}")
            };
            return COMMANDS
                .iter()
                .find(|spec| spec.name == normalized)
                .map_or_else(
                    || vec![format!("未找到命令：{normalized}")],
                    |spec| {
                        vec![
                            spec.synopsis.to_owned(),
                            command_description(self.language, spec.name, spec.description)
                                .to_owned(),
                        ]
                    },
                );
        }
        let mut lines = vec!["Available commands".to_owned()];
        lines.extend(COMMANDS.iter().map(|spec| {
            format!(
                "{:<12} {}",
                spec.name,
                command_description(self.language, spec.name, spec.description)
            )
        }));
        lines
    }
}

fn argument_suggestions(input: &str) -> Vec<InputSuggestion> {
    let Some((command, remainder)) = input.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let prefix = remainder.trim_start();
    let values: &[(&str, &str)] = match command {
        "/mode" => &[
            ("lite", "最小资源与单 Agent 模式"),
            ("balanced", "默认质量与资源平衡模式"),
            ("full", "完整工具与多 Agent 模式"),
            ("custom", "使用自定义预算与能力设置"),
        ],
        "/reasoning" | "/think" => &[
            ("off", "关闭推理预算"),
            ("minimal", "最小推理"),
            ("low", "低推理"),
            ("medium", "中等推理"),
            ("high", "高推理"),
            ("xhigh", "超高推理（需模型支持）"),
            ("max", "最大推理（需模型支持）"),
        ],
        "/agents" => &[
            ("normal", "标准 Agent 状态"),
            ("verbose", "完整 Agent 能力与边界"),
            ("compact", "紧凑单行状态"),
            ("tree", "控制面与 Worker 树"),
        ],
        "/agentmd" => &[
            ("status", "显示实际加载的 scope、路径和大小"),
            ("show", "显示当前生效的 agent.md 内容"),
            ("init-project", "创建项目私有 .harness/agent.md"),
            ("init-global", "创建全局 ~/.kernary/agent.md"),
        ],
        "/permissions" => &[
            ("manual", "所有 Tool 操作手动确认"),
            ("edit", "文件编辑自动，终端与外部操作确认"),
            ("auto", "Sandbox 内低风险自动，高风险确认"),
            ("full", "Sandbox 内自动，Patch 应用仍确认"),
            ("bypass", "最高权限；需输入确认短语"),
            ("custom", "使用自定义规则"),
            ("rules", "列出持久权限规则"),
            ("rule add ", "添加 allow/ask/deny 规则"),
            ("rule remove ", "删除指定规则"),
        ],
        "/sandbox" => &[
            ("read-only", "所有文件只读，网络默认关闭"),
            (
                "workspace-write",
                "仅项目可写，保护 .git/.harness，网络默认关闭",
            ),
            ("danger-full-access", "关闭系统边界；需输入确认短语"),
            ("network-on", "允许受限子进程联网；需输入确认短语"),
            ("network-off", "关闭受限子进程网络访问"),
        ],
        "/vector" => &[
            ("status", "显示向量硬门与后端状态"),
            ("setup", "配置 URL、Key 与手填 Embedding 模型"),
            ("clear", "移除单一 Embedding Provider 配置"),
            ("on", "有 Embedding Model 时允许语义检索"),
            ("off", "强制 lexical 路径"),
            ("auto", "按能力与模式自动选择"),
            ("purge", "清除向量投影"),
        ],
        "/git" => &[
            ("status", "显示工作树状态"),
            ("diff", "显示未提交差异"),
            ("log", "显示最近提交"),
            ("branch", "显示当前分支"),
        ],
        "/compact" => &[
            ("auto", "按阈值决定是否压缩"),
            ("safe", "保留关键证据的安全压缩"),
            ("aggressive", "先 checkpoint 再强力压缩"),
        ],
        "/trace" => &[
            ("status", "显示 Trace 状态"),
            ("on", "开启有界 Trace"),
            ("off", "关闭 Trace"),
            ("last ", "查看最近 Trace 事件"),
        ],
        "/review" => &[("unstaged", "审查未暂存变更"), ("staged", "审查已暂存变更")],
        "/browser" => &[
            ("status", "显示 Browser Runtime 状态"),
            ("open", "显式打开隔离浏览器"),
            ("navigate ", "导航到 URL"),
            ("actions", "列出可交互元素"),
            ("handoff", "交还用户控制"),
            ("reclaim", "收回 Agent 控制"),
            ("close", "关闭隔离浏览器"),
        ],
        "/index" => &[
            ("status", "显示 Repository Index 状态"),
            ("build", "构建完整索引"),
            ("update", "增量更新索引"),
            ("clear", "清除索引"),
            ("map", "显示代码地图"),
            ("search ", "搜索 Repository Index"),
        ],
        "/provider" => &[
            ("add", "添加 OpenAI-compatible 自定义提供商"),
            ("switch", "切换当前模型提供商及其默认模型"),
            ("remove ", "删除项目级自定义提供商及其凭证"),
        ],
        "/language" => &[
            ("en", "English"),
            ("zh-CN", "简体中文"),
            ("zh-TW", "繁體中文"),
            ("ja", "日本語"),
        ],
        _ => return help_argument_suggestions(command, prefix),
    };
    values
        .iter()
        .filter(|(value, _)| value.starts_with(prefix))
        .map(|(value, description)| {
            let replacement = format!("{command} {value}");
            InputSuggestion::new(replacement.clone(), replacement, *description)
        })
        .collect()
}

fn help_argument_suggestions(command: &str, prefix: &str) -> Vec<InputSuggestion> {
    if command != "/help" {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.name.trim_start_matches('/').starts_with(prefix))
        .map(|spec| {
            let replacement = format!("/help {}", spec.name.trim_start_matches('/'));
            InputSuggestion::new(replacement, spec.synopsis, spec.description)
        })
        .collect()
}

fn parse_reasoning(value: &str) -> Result<ReasoningLevel, CommandParseError> {
    match value {
        "off" | "none" => Ok(ReasoningLevel::Off),
        "minimal" => Ok(ReasoningLevel::Minimal),
        "low" => Ok(ReasoningLevel::Low),
        "medium" => Ok(ReasoningLevel::Medium),
        "high" => Ok(ReasoningLevel::High),
        "xhigh" => Ok(ReasoningLevel::Xhigh),
        "max" => Ok(ReasoningLevel::Max),
        _ => Err(CommandParseError {
            code: "invalid-reasoning-level",
            message: "Reasoning 仅支持 off/minimal/low/medium/high/xhigh/max".to_owned(),
        }),
    }
}

fn parse_permission(value: &str) -> Result<PermissionCommand, CommandParseError> {
    if matches!(value, "rules" | "rule list") {
        return Ok(PermissionCommand::RuleList);
    }
    if let Some(rule_id) = value
        .strip_prefix("rule remove ")
        .map(str::trim)
        .filter(|rule_id| !rule_id.is_empty() && rule_id.split_whitespace().count() == 1)
    {
        return Ok(PermissionCommand::RuleRemove {
            rule_id: rule_id.to_owned(),
        });
    }
    if let Some(rest) = value.strip_prefix("rule add ") {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let effect = parts.next().unwrap_or_default();
        let action = parts.next().unwrap_or_default();
        let pattern = parts.next().map(str::trim).unwrap_or_default();
        if matches!(effect, "allow" | "ask" | "deny")
            && matches!(
                action,
                "read" | "write" | "execute" | "network" | "browser" | "mcp" | "plugin"
            )
            && !pattern.is_empty()
        {
            return Ok(PermissionCommand::RuleAdd {
                effect: effect.to_owned(),
                action: action.to_owned(),
                pattern: pattern.to_owned(),
            });
        }
    }
    Err(CommandParseError {
        code: "invalid-permission-command",
        message: "用法：/permissions [manual|edit|auto|full|bypass|custom|rules|rule add <allow|ask|deny> <read|write|execute|network|browser|mcp|plugin> <pattern>|rule remove <id>]".to_owned(),
    })
}

fn parse_failover(value: &str) -> Result<FailoverCommand, CommandParseError> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        [] | ["status"] => Ok(FailoverCommand::Status),
        ["off"] => Ok(FailoverCommand::Off),
        ["on", "--confirm-cost", targets @ ..]
            if !targets.is_empty()
                && targets.iter().all(|target| {
                    target
                        .split_once('/')
                        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
                }) =>
        {
            Ok(FailoverCommand::On {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
            })
        }
        _ => Err(CommandParseError {
            code: "invalid-failover-command",
            message: "用法：/failover [status|off|on --confirm-cost <provider/model>...]"
                .to_owned(),
        }),
    }
}

fn parse_inspect(value: &str) -> Result<String, CommandParseError> {
    match value {
        "session" | "config" | "context" | "memory" | "vector" | "repository" | "cache"
        | "tool" | "tools" | "agent" | "agents" | "plan" => Ok(value.to_owned()),
        _ => Err(CommandParseError {
            code: "invalid-inspect-target",
            message: "Inspect 仅支持 session/config/context/memory/vector/repository/cache/tool/agent/plan"
                .to_owned(),
        }),
    }
}

fn parse_bounded_limit(
    value: &str,
    default: usize,
    command: &'static str,
) -> Result<usize, CommandParseError> {
    if value.is_empty() {
        return Ok(default);
    }
    if value.split_whitespace().count() != 1 {
        return Err(CommandParseError {
            code: "invalid-log-limit",
            message: format!("用法：/{command} [1..200]"),
        });
    }
    value
        .parse::<usize>()
        .ok()
        .filter(|limit| (1..=200).contains(limit))
        .ok_or_else(|| CommandParseError {
            code: "invalid-log-limit",
            message: format!("用法：/{command} [1..200]"),
        })
}

fn parse_trace(value: &str) -> Result<TraceCommand, CommandParseError> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        [] | ["status"] => Ok(TraceCommand::Status),
        ["on"] => Ok(TraceCommand::On),
        ["off"] => Ok(TraceCommand::Off),
        ["last"] => Ok(TraceCommand::Last { limit: 50 }),
        ["last", limit] => Ok(TraceCommand::Last {
            limit: parse_bounded_limit(limit, 50, "trace last")?,
        }),
        _ => Err(CommandParseError {
            code: "invalid-trace-command",
            message: "用法：/trace [status|on|off|last [1..200]]".to_owned(),
        }),
    }
}

fn parse_settings(value: &str) -> Result<SettingsCommand, CommandParseError> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    let layer = |value: Option<&&str>| -> Result<SettingLayer, CommandParseError> {
        match value.copied().unwrap_or("session") {
            "session" => Ok(SettingLayer::Session),
            "runtime" => Ok(SettingLayer::Runtime),
            _ => Err(CommandParseError {
                code: "invalid-setting-layer",
                message: "设置层仅支持 session/runtime".to_owned(),
            }),
        }
    };
    match parts.as_slice() {
        [] => Ok(SettingsCommand::Show { key: None }),
        [key] if *key != "set" && *key != "reset" => Ok(SettingsCommand::Show {
            key: Some((*key).to_owned()),
        }),
        ["set", key, value] => Ok(SettingsCommand::Set {
            key: (*key).to_owned(),
            value: (*value).to_owned(),
            layer: SettingLayer::Session,
        }),
        ["set", key, value, selected_layer] => Ok(SettingsCommand::Set {
            key: (*key).to_owned(),
            value: (*value).to_owned(),
            layer: layer(Some(selected_layer))?,
        }),
        ["reset", key] => Ok(SettingsCommand::Reset {
            key: (*key).to_owned(),
            layer: SettingLayer::Session,
        }),
        ["reset", key, selected_layer] => Ok(SettingsCommand::Reset {
            key: (*key).to_owned(),
            layer: layer(Some(selected_layer))?,
        }),
        _ => Err(CommandParseError {
            code: "invalid-settings-command",
            message: "用法：/settings [key|set <key> <value> [session|runtime]|reset <key> [session|runtime]]".to_owned(),
        }),
    }
}

fn parse_lsp(value: &str) -> Result<LspCommand, CommandParseError> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    let position = |line: &str, character: &str| -> Result<(u32, u32), CommandParseError> {
        let line = line.parse::<u32>().ok().filter(|value| *value > 0);
        let character = character.parse::<u32>().ok().filter(|value| *value > 0);
        line.zip(character).ok_or_else(|| CommandParseError {
            code: "invalid-lsp-position",
            message: "LSP 行列必须是大于 0 的整数（用户界面使用 1-based）".to_owned(),
        })
    };
    match parts.as_slice() {
        [] | ["list"] => Ok(LspCommand::List),
        ["start", server_id] => Ok(LspCommand::Start {
            server_id: (*server_id).to_owned(),
        }),
        ["stop", server_id] => Ok(LspCommand::Stop {
            server_id: (*server_id).to_owned(),
        }),
        ["symbols", server_id, path] => Ok(LspCommand::Symbols {
            server_id: (*server_id).to_owned(),
            path: (*path).to_owned(),
        }),
        ["definition", server_id, path, line, character] => {
            let (line, character) = position(line, character)?;
            Ok(LspCommand::Definition {
                server_id: (*server_id).to_owned(),
                path: (*path).to_owned(),
                line,
                character,
            })
        }
        ["references", server_id, path, line, character] => {
            let (line, character) = position(line, character)?;
            Ok(LspCommand::References {
                server_id: (*server_id).to_owned(),
                path: (*path).to_owned(),
                line,
                character,
            })
        }
        ["diagnostics", server_id, path] => Ok(LspCommand::Diagnostics {
            server_id: (*server_id).to_owned(),
            path: (*path).to_owned(),
        }),
        ["rename", server_id, path, line, character, new_name] => {
            let (line, character) = position(line, character)?;
            Ok(LspCommand::RenamePreview {
                server_id: (*server_id).to_owned(),
                path: (*path).to_owned(),
                line,
                character,
                new_name: (*new_name).to_owned(),
            })
        }
        [
            "code-action",
            server_id,
            path,
            start_line,
            start_character,
            end_line,
            end_character,
            action_index,
        ]
        | [
            "code-action",
            server_id,
            path,
            start_line,
            start_character,
            end_line,
            end_character,
            action_index,
            _,
        ] => {
            let (start_line, start_character) = position(start_line, start_character)?;
            let (end_line, end_character) = position(end_line, end_character)?;
            let action_index = action_index.parse::<usize>().map_err(|_| CommandParseError {
                code: "invalid-lsp-action-index",
                message: "action-index 必须是非负整数".to_owned(),
            })?;
            Ok(LspCommand::CodeActionPreview {
                server_id: (*server_id).to_owned(),
                path: (*path).to_owned(),
                start_line,
                start_character,
                end_line,
                end_character,
                action_index,
                only: (parts.len() == 9).then(|| parts[8].to_owned()),
            })
        }
        ["apply", preview_id] => Ok(LspCommand::ApplyPreview {
            preview_id: (*preview_id).to_owned(),
        }),
        ["undo", preview_id] => Ok(LspCommand::UndoPreview {
            preview_id: (*preview_id).to_owned(),
        }),
        _ => Err(CommandParseError {
            code: "invalid-lsp-command",
            message: "用法：/lsp [start|stop|symbols|definition|references|diagnostics|rename|code-action|apply|undo] ...".to_owned(),
        }),
    }
}

fn parse_mcp(value: &str) -> Result<McpCommand, CommandParseError> {
    let arguments = value.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => Ok(McpCommand::List),
        ["add-stdio", server_id, command, args @ ..] => Ok(McpCommand::AddStdio {
            server_id: (*server_id).to_owned(),
            command: (*command).to_owned(),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
        }),
        ["add-http", server_id, endpoint] => Ok(McpCommand::AddHttp {
            server_id: (*server_id).to_owned(),
            endpoint: (*endpoint).to_owned(),
        }),
        ["remove", server_id] => Ok(McpCommand::Remove {
            server_id: (*server_id).to_owned(),
        }),
        ["enable", server_id] => Ok(McpCommand::Enable {
            server_id: (*server_id).to_owned(),
        }),
        ["disable", server_id] => Ok(McpCommand::Disable {
            server_id: (*server_id).to_owned(),
        }),
        ["auth", "start", server_id] => Ok(McpCommand::AuthStart {
            server_id: (*server_id).to_owned(),
        }),
        ["auth", "finish", server_id] => Ok(McpCommand::AuthFinish {
            server_id: (*server_id).to_owned(),
        }),
        ["auth", "refresh", server_id] => Ok(McpCommand::AuthRefresh {
            server_id: (*server_id).to_owned(),
        }),
        ["auth", "status", server_id] => Ok(McpCommand::AuthStatus {
            server_id: (*server_id).to_owned(),
        }),
        ["connect", server_id] => Ok(McpCommand::Connect {
            server_id: (*server_id).to_owned(),
            force: false,
        }),
        ["reconnect", server_id] => Ok(McpCommand::Connect {
            server_id: (*server_id).to_owned(),
            force: true,
        }),
        ["disconnect", server_id] => Ok(McpCommand::Disconnect {
            server_id: (*server_id).to_owned(),
        }),
        ["tools", server_id] => Ok(McpCommand::Tools {
            server_id: (*server_id).to_owned(),
        }),
        ["resources", server_id] => Ok(McpCommand::Resources {
            server_id: (*server_id).to_owned(),
        }),
        ["prompts", server_id] => Ok(McpCommand::Prompts {
            server_id: (*server_id).to_owned(),
        }),
        ["poll", server_id] => Ok(McpCommand::Poll {
            server_id: (*server_id).to_owned(),
        }),
        ["read", server_id, uri] => Ok(McpCommand::Read {
            server_id: (*server_id).to_owned(),
            uri: (*uri).to_owned(),
        }),
        _ => Err(CommandParseError {
            code: "invalid-mcp-command",
            message: "用法：/mcp [list|auth start|finish|refresh|status <id>|connect <id>|reconnect <id>|disconnect <id>|tools <id>|resources <id>|prompts <id>|poll <id>|read <id> <uri>]".to_owned(),
        }),
    }
}

fn parse_plugin(value: &str) -> Result<PluginCommand, CommandParseError> {
    let arguments = value.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] => Ok(PluginCommand::List),
        ["review", plugin_id] => Ok(PluginCommand::Review {
            plugin_id: (*plugin_id).to_owned(),
        }),
        ["enable", plugin_id, review_hash] => Ok(PluginCommand::Enable {
            plugin_id: (*plugin_id).to_owned(),
            review_hash: (*review_hash).to_owned(),
        }),
        ["disable", plugin_id] => Ok(PluginCommand::Disable {
            plugin_id: (*plugin_id).to_owned(),
        }),
        _ => Err(CommandParseError {
            code: "invalid-plugin-command",
            message: "用法：/plugins [list|review <id>|enable <id> <review-hash>|disable <id>]"
                .to_owned(),
        }),
    }
}

fn parse_skill(value: &str) -> Result<SkillCommand, CommandParseError> {
    if value.is_empty() || value == "list" {
        return Ok(SkillCommand::List);
    }
    if let Some(query) = value.strip_prefix("search ").map(str::trim)
        && !query.is_empty()
    {
        return Ok(SkillCommand::Search {
            query: query.to_owned(),
        });
    }
    let arguments = value.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        ["load", skill_id] => Ok(SkillCommand::Load {
            skill_id: (*skill_id).to_owned(),
        }),
        ["unload", skill_id] => Ok(SkillCommand::Unload {
            skill_id: (*skill_id).to_owned(),
        }),
        _ => Err(CommandParseError {
            code: "invalid-skill-command",
            message: "用法：/skills [list|search <query>|load <id>|unload <id>]".to_owned(),
        }),
    }
}

fn parse_memory(value: &str) -> Result<MemoryCommand, CommandParseError> {
    if value.is_empty() || value == "stats" {
        return Ok(MemoryCommand::Stats);
    }
    if let Some(rest) = value.strip_prefix("search ") {
        let (mode, query) = rest.split_once(' ').unwrap_or(("auto", rest));
        if !query.trim().is_empty() {
            return Ok(MemoryCommand::Search {
                mode: mode.to_owned(),
                query: query.trim().to_owned(),
            });
        }
    }
    if let Some(id) = value
        .strip_prefix("forget ")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Ok(MemoryCommand::Forget { id: id.to_owned() });
    }
    if let Some(rest) = value.strip_prefix("add ") {
        let parts = rest.split('|').map(str::trim).collect::<Vec<_>>();
        if parts.len() >= 3 {
            return Ok(MemoryCommand::Add {
                kind: parts[0].to_owned(),
                title: parts[1].to_owned(),
                content: parts[2].to_owned(),
                tags: parts.get(3).map_or_else(Vec::new, |tags| {
                    tags.split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                        .collect()
                }),
            });
        }
    }
    Err(CommandParseError{code:"invalid-memory-command",message:"用法：/memory stats | search <metadata|lexical|semantic|hybrid|auto> <query> | add <kind> | <title> | <content> | <tags> | forget <id>".to_owned()})
}
fn parse_index(value: &str) -> Result<IndexCommand, CommandParseError> {
    if value.is_empty() || value == "status" {
        Ok(IndexCommand::Status)
    } else if matches!(value, "build" | "update") {
        Ok(IndexCommand::Update)
    } else if value == "clear" {
        Ok(IndexCommand::Clear)
    } else if value == "map" {
        Ok(IndexCommand::Map)
    } else if let Some(query) = value
        .strip_prefix("search ")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Ok(IndexCommand::Search {
            query: query.to_owned(),
        })
    } else {
        Err(CommandParseError {
            code: "invalid-index-command",
            message: "用法：/index [status|build|update|clear|map|search <query>]".to_owned(),
        })
    }
}
fn parse_vector(value: &str) -> Result<VectorCommand, CommandParseError> {
    match value {
        "" | "status" => Ok(VectorCommand::Status),
        "setup" => Ok(VectorCommand::Setup),
        "clear" => Ok(VectorCommand::Clear),
        "purge" => Ok(VectorCommand::Purge),
        "on" | "off" | "auto" => Ok(VectorCommand::Mode {
            mode: value.to_owned(),
        }),
        _ => Err(CommandParseError {
            code: "invalid-vector-command",
            message: "用法：/vector [status|setup|clear|on|off|auto|purge]".to_owned(),
        }),
    }
}

fn parse_queue(value: &str) -> Result<QueueCommand, CommandParseError> {
    let arguments = value.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["status"] => Ok(QueueCommand::Status),
        ["cancel", task_id] => Ok(QueueCommand::Cancel {
            task_id: (*task_id).to_owned(),
        }),
        ["priority", task_id, priority] => {
            let priority = priority.parse::<i32>().map_err(|_| CommandParseError {
                code: "invalid-queue-priority",
                message: "Queue priority 必须是 -100..100 的整数".to_owned(),
            })?;
            if !(-100..=100).contains(&priority) {
                return Err(CommandParseError {
                    code: "invalid-queue-priority",
                    message: "Queue priority 必须是 -100..100 的整数".to_owned(),
                });
            }
            Ok(QueueCommand::Priority {
                task_id: (*task_id).to_owned(),
                priority,
            })
        }
        _ => Err(CommandParseError {
            code: "invalid-queue-command",
            message: "用法：/queue [status|cancel <task-id>|priority <task-id> <-100..100>]"
                .to_owned(),
        }),
    }
}

fn parse_budget(value: &str) -> Result<BudgetCommand, CommandParseError> {
    let arguments = value.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["show"] => Ok(BudgetCommand::Show),
        [field, value]
            if matches!(
                *field,
                "agents" | "parallel" | "tokens" | "tools" | "runtime-ms" | "retries" | "cost"
            ) =>
        {
            let value = value.parse::<u64>().map_err(|_| CommandParseError {
                code: "invalid-budget-value",
                message: "Budget value 必须是非负整数".to_owned(),
            })?;
            Ok(BudgetCommand::Set {
                field: (*field).to_owned(),
                value,
            })
        }
        _ => Err(CommandParseError {
            code: "invalid-budget-command",
            message: "用法：/budget [agents|parallel|tokens|tools|runtime-ms|retries|cost <value>]"
                .to_owned(),
        }),
    }
}

fn parse_team(value: &str) -> Result<TeamCommand, CommandParseError> {
    if matches!(value, "" | "status") {
        return Ok(TeamCommand::Status);
    }
    let mut parts = value.splitn(3, char::is_whitespace);
    let operation = parts.next();
    if !matches!(operation, Some("create" | "workflow" | "adaptive")) {
        return Err(CommandParseError {
            code: "invalid-team-command",
            message: "用法：/team [create <2..8>|workflow|adaptive <1..4> [objective]]".to_owned(),
        });
    }
    let count = parts
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| match operation {
            Some("create") => (2..=8).contains(count),
            Some("workflow" | "adaptive") => (1..=4).contains(count),
            _ => false,
        })
        .ok_or_else(|| CommandParseError {
            code: "invalid-team-size",
            message: "create size 必须是 2..8；workflow/adaptive workers 必须是 1..4".to_owned(),
        })?;
    let objective = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(match operation {
        Some("adaptive") => TeamCommand::Adaptive {
            workers: count,
            objective,
        },
        Some("workflow") => TeamCommand::Workflow {
            workers: count,
            objective,
        },
        _ => TeamCommand::Create { count, objective },
    })
}

fn parse_browser(value: &str) -> Result<BrowserCommand, CommandParseError> {
    match value {
        "" | "status" => Ok(BrowserCommand::Status),
        "open" => Ok(BrowserCommand::Open),
        "actions" => Ok(BrowserCommand::Actions),
        "handoff" => Ok(BrowserCommand::Handoff),
        "reclaim" => Ok(BrowserCommand::Reclaim),
        "close" => Ok(BrowserCommand::Close),
        value if value.starts_with("navigate ") => {
            let url = value[9..].trim();
            if url.is_empty() {
                return Err(CommandParseError {
                    code: "browser-url-empty",
                    message: "用法：/browser navigate <url>".to_owned(),
                });
            }
            Ok(BrowserCommand::Navigate {
                url: url.to_owned(),
            })
        }
        _ => Err(CommandParseError {
            code: "invalid-browser-command",
            message: "用法：/browser [status|open|navigate <url>|actions|handoff|reclaim|close]"
                .to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_completion_is_stable_and_sorted() {
        let registry = CommandRegistry::new();
        assert_eq!(registry.complete("/g"), vec!["/git", "/goal"]);
        assert_eq!(
            registry.complete("/s"),
            vec![
                "/sandbox",
                "/session",
                "/sessions",
                "/settings",
                "/skills",
                "/status",
                "/steer"
            ]
        );
        let suggestions = registry.suggestions("/");
        assert_eq!(suggestions.len(), COMMANDS.len());
        assert!(suggestions.iter().all(|suggestion| {
            suggestion.label.starts_with('/') && !suggestion.description.is_empty()
        }));
        assert!(
            suggestions
                .iter()
                .all(|suggestion| !suggestion.label.starts_with("/login"))
        );
        for language in [UiLanguage::En, UiLanguage::ZhTw, UiLanguage::Ja] {
            let localized = CommandRegistry::with_language(language).suggestions("/");
            assert_eq!(localized.len(), suggestions.len());
            assert!(
                localized
                    .iter()
                    .zip(&suggestions)
                    .all(|(candidate, zh_cn)| { candidate.description != zh_cn.description })
            );
        }
        assert_eq!(
            registry.suggestions("/mode b")[0].replacement,
            "/mode balanced"
        );
    }

    #[test]
    fn mode_config_and_settings_commands_are_strict() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/mode full").expect("mode"),
            ParsedInput::Command(SlashCommand::Mode {
                mode: Some("full".to_owned())
            })
        );
        assert_eq!(
            registry
                .parse("/settings set ui.statusbar false runtime")
                .expect("settings"),
            ParsedInput::Command(SlashCommand::Settings {
                operation: SettingsCommand::Set {
                    key: "ui.statusbar".to_owned(),
                    value: "false".to_owned(),
                    layer: SettingLayer::Runtime,
                }
            })
        );
        assert_eq!(
            registry.parse("/settings reset mode").expect("reset"),
            ParsedInput::Command(SlashCommand::Settings {
                operation: SettingsCommand::Reset {
                    key: "mode".to_owned(),
                    layer: SettingLayer::Session,
                }
            })
        );
        assert!(registry.parse("/mode enormous").is_err());
        assert!(registry.parse("/settings set mode lite project").is_err());
        assert_eq!(
            registry.parse("/permissions full").expect("permissions"),
            ParsedInput::Command(SlashCommand::Permissions {
                operation: PermissionCommand::Mode {
                    mode: "full".to_owned()
                }
            })
        );
        assert_eq!(
            registry
                .parse("/permissions rule add deny execute rm -rf *")
                .expect("permission rule"),
            ParsedInput::Command(SlashCommand::Permissions {
                operation: PermissionCommand::RuleAdd {
                    effect: "deny".to_owned(),
                    action: "execute".to_owned(),
                    pattern: "rm -rf *".to_owned()
                }
            })
        );
    }

    #[test]
    fn observability_and_explanation_commands_are_bounded_and_read_only() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/trace on").expect("trace"),
            ParsedInput::Command(SlashCommand::Trace {
                operation: TraceCommand::On
            })
        );
        assert_eq!(
            registry.parse("/trace last 12").expect("trace last"),
            ParsedInput::Command(SlashCommand::Trace {
                operation: TraceCommand::Last { limit: 12 }
            })
        );
        assert_eq!(
            registry.parse("/logs 25").expect("logs"),
            ParsedInput::Command(SlashCommand::Logs { limit: 25 })
        );
        assert_eq!(
            registry.parse("/inspect context").expect("inspect"),
            ParsedInput::Command(SlashCommand::Inspect {
                target: "context".to_owned()
            })
        );
        assert!(registry.parse("/logs 201").is_err());
        assert!(registry.parse("/inspect secret").is_err());
    }

    #[test]
    fn goal_shorthand_and_subcommands_parse() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/goal implement auth").expect("goal"),
            ParsedInput::Command(SlashCommand::GoalSet {
                text: "implement auth".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/goal lock").expect("lock"),
            ParsedInput::Command(SlashCommand::GoalLock { locked: true })
        );
        assert_eq!(
            registry.parse("/goal history 12").expect("history"),
            ParsedInput::Command(SlashCommand::GoalHistory { limit: 12 })
        );
        assert_eq!(
            registry.parse("/goal clear").expect("clear"),
            ParsedInput::Command(SlashCommand::GoalClear)
        );
        assert_eq!(
            registry.parse("/goal edit revised goal").expect("edit"),
            ParsedInput::Command(SlashCommand::GoalSet {
                text: "revised goal".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/sessions").expect("sessions"),
            ParsedInput::Command(SlashCommand::Sessions)
        );
        assert_eq!(
            registry.parse("/session new").expect("new session"),
            ParsedInput::Command(SlashCommand::SessionNew)
        );
        assert_eq!(
            registry
                .parse("/session switch session:123")
                .expect("switch session"),
            ParsedInput::Command(SlashCommand::SessionSwitch {
                target: "session:123".to_owned()
            })
        );
        assert_eq!(
            registry
                .parse("/session rename auth refactor")
                .expect("rename session"),
            ParsedInput::Command(SlashCommand::SessionRename {
                title: "auth refactor".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/agentmd init-project").expect("agent md"),
            ParsedInput::Command(SlashCommand::AgentMd {
                operation: AgentMdCommand::InitProject
            })
        );
        assert_eq!(
            registry.parse("/reset context").expect("reset"),
            ParsedInput::Command(SlashCommand::Reset)
        );
        assert_eq!(
            registry.parse("/forget memory:1").expect("forget"),
            ParsedInput::Command(SlashCommand::Forget {
                id: "memory:1".to_owned()
            })
        );
        assert_eq!(
            registry
                .parse("/failover on --confirm-cost openai/gpt-5 anthropic/claude")
                .expect("failover"),
            ParsedInput::Command(SlashCommand::Failover {
                operation: FailoverCommand::On {
                    targets: vec!["openai/gpt-5".to_owned(), "anthropic/claude".to_owned()]
                }
            })
        );
        assert!(registry.parse("/failover on openai/gpt-5").is_err());
    }

    #[test]
    fn agent_inspection_commands_parse_without_loading_capability_docs() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/agent agent:coder").expect("agent"),
            ParsedInput::Command(SlashCommand::AgentShow {
                agent_id: "agent:coder".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/agents").expect("agents"),
            ParsedInput::Command(SlashCommand::Agents {
                mode: AgentDisplayMode::Normal
            })
        );
        assert_eq!(
            registry.parse("/agents tree").expect("agent tree"),
            ParsedInput::Command(SlashCommand::Agents {
                mode: AgentDisplayMode::Tree
            })
        );
        assert_eq!(
            registry.parse("/team").expect("team"),
            ParsedInput::Command(SlashCommand::Team {
                operation: TeamCommand::Status
            })
        );
        assert_eq!(
            registry
                .parse("/team create 4 inspect auth")
                .expect("create team"),
            ParsedInput::Command(SlashCommand::Team {
                operation: TeamCommand::Create {
                    count: 4,
                    objective: Some("inspect auth".to_owned())
                }
            })
        );
        assert_eq!(
            registry
                .parse("/lsp code-action rust src/main.rs 1 1 1 4 0 quickfix")
                .expect("code action preview"),
            ParsedInput::Command(SlashCommand::Lsp {
                operation: LspCommand::CodeActionPreview {
                    server_id: "rust".to_owned(),
                    path: "src/main.rs".to_owned(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 1,
                    end_character: 4,
                    action_index: 0,
                    only: Some("quickfix".to_owned())
                }
            })
        );
        assert_eq!(
            registry
                .parse("/team workflow 2 implement auth")
                .expect("role workflow"),
            ParsedInput::Command(SlashCommand::Team {
                operation: TeamCommand::Workflow {
                    workers: 2,
                    objective: Some("implement auth".to_owned())
                }
            })
        );
        assert_eq!(
            registry
                .parse("/team adaptive 3 release auth service")
                .expect("adaptive workflow"),
            ParsedInput::Command(SlashCommand::Team {
                operation: TeamCommand::Adaptive {
                    workers: 3,
                    objective: Some("release auth service".to_owned())
                }
            })
        );
        assert_eq!(
            registry.parse("/agents 4").expect("agents shorthand"),
            ParsedInput::Command(SlashCommand::Team {
                operation: TeamCommand::Create {
                    count: 4,
                    objective: None
                }
            })
        );
        assert_eq!(
            registry.parse("/queue").expect("queue"),
            ParsedInput::Command(SlashCommand::Queue {
                operation: QueueCommand::Status
            })
        );
        assert_eq!(
            registry
                .parse("/queue priority task:coder 70")
                .expect("priority"),
            ParsedInput::Command(SlashCommand::Queue {
                operation: QueueCommand::Priority {
                    task_id: "task:coder".to_owned(),
                    priority: 70
                }
            })
        );
        assert_eq!(
            registry.parse("/queue cancel task:coder").expect("cancel"),
            ParsedInput::Command(SlashCommand::Queue {
                operation: QueueCommand::Cancel {
                    task_id: "task:coder".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/steer keep schema").expect("steer"),
            ParsedInput::Command(SlashCommand::Steer {
                instruction: "keep schema".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/budget parallel 3").expect("budget"),
            ParsedInput::Command(SlashCommand::Budget {
                operation: BudgetCommand::Set {
                    field: "parallel".to_owned(),
                    value: 3
                }
            })
        );
        assert_eq!(
            registry.parse("/budget cost 12").expect("cost budget"),
            ParsedInput::Command(SlashCommand::Budget {
                operation: BudgetCommand::Set {
                    field: "cost".to_owned(),
                    value: 12
                }
            })
        );
        assert_eq!(
            registry
                .parse("/browser navigate http://127.0.0.1:4173/")
                .expect("browser"),
            ParsedInput::Command(SlashCommand::Browser {
                operation: BrowserCommand::Navigate {
                    url: "http://127.0.0.1:4173/".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/browser reclaim").expect("reclaim"),
            ParsedInput::Command(SlashCommand::Browser {
                operation: BrowserCommand::Reclaim
            })
        );
    }

    #[test]
    fn context_commands_parse_without_touching_runtime() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/compact aggressive").expect("compact"),
            ParsedInput::Command(SlashCommand::Compact {
                mode: CompactCommandMode::Aggressive
            })
        );
        assert_eq!(
            registry.parse("/compact auto").expect("compact auto"),
            ParsedInput::Command(SlashCommand::Compact {
                mode: CompactCommandMode::Auto
            })
        );
        assert_eq!(
            registry
                .parse("/checkpoint before-refactor")
                .expect("checkpoint"),
            ParsedInput::Command(SlashCommand::Checkpoint {
                name: Some("before-refactor".to_owned())
            })
        );
        assert_eq!(
            registry.parse("/focus clear").expect("focus clear"),
            ParsedInput::Command(SlashCommand::Focus { value: None })
        );
        assert_eq!(
            registry
                .parse("/fork checkpoint:7 session:child")
                .expect("fork"),
            ParsedInput::Command(SlashCommand::Fork {
                checkpoint_id: "checkpoint:7".to_owned(),
                child_session_id: Some("session:child".to_owned())
            })
        );
        assert_eq!(
            registry.parse("/rollback checkpoint:7").expect("rollback"),
            ParsedInput::Command(SlashCommand::Rollback {
                checkpoint_id: "checkpoint:7".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/resume").expect("resume"),
            ParsedInput::Command(SlashCommand::Resume)
        );
        assert_eq!(
            registry.complete("/c"),
            vec![
                "/cache",
                "/checkpoint",
                "/clear",
                "/compact",
                "/config",
                "/connect",
                "/context"
            ]
        );
    }

    #[test]
    fn model_auth_and_reasoning_commands_are_strict() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/connect opencode-go").expect("connect"),
            ParsedInput::Command(SlashCommand::Connect {
                provider: Some("opencode-go".to_owned())
            })
        );
        assert_eq!(
            registry.parse("/providers").expect("providers"),
            ParsedInput::Command(SlashCommand::Providers)
        );
        assert_eq!(
            registry.parse("/provider add").expect("provider add"),
            ParsedInput::Command(SlashCommand::Provider {
                operation: ProviderCommand::Add
            })
        );
        assert_eq!(
            registry.parse("/provider switch").expect("provider switch"),
            ParsedInput::Command(SlashCommand::Provider {
                operation: ProviderCommand::Switch
            })
        );
        assert_eq!(
            registry
                .parse("/provider remove custom-relay")
                .expect("provider remove"),
            ParsedInput::Command(SlashCommand::Provider {
                operation: ProviderCommand::Remove {
                    provider_id: "custom-relay".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/model openai/gpt-test").expect("model"),
            ParsedInput::Command(SlashCommand::ModelSelect {
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned()
            })
        );
        assert_eq!(
            registry.parse("/model gpt-test").expect("current model"),
            ParsedInput::Command(SlashCommand::ModelSelectCurrent {
                model: "gpt-test".to_owned()
            })
        );
        assert_eq!(
            registry
                .parse("/models refresh opencode-go")
                .expect("refresh"),
            ParsedInput::Command(SlashCommand::Models {
                refresh: true,
                provider: Some("opencode-go".to_owned())
            })
        );
        assert_eq!(
            registry
                .parse("/models refresh")
                .expect_err("provider required")
                .code,
            "model-refresh-provider-required"
        );
        assert_eq!(
            registry
                .parse("/lsp definition rust src/main.rs 12 7")
                .expect("lsp"),
            ParsedInput::Command(SlashCommand::Lsp {
                operation: LspCommand::Definition {
                    server_id: "rust".to_owned(),
                    path: "src/main.rs".to_owned(),
                    line: 12,
                    character: 7
                }
            })
        );
        assert_eq!(
            registry
                .parse("/lsp references rust src/main.rs 0 1")
                .expect_err("zero position")
                .code,
            "invalid-lsp-position"
        );
        assert_eq!(
            registry
                .parse("/lsp rename rust src/main.rs 12 7 NewName")
                .expect("rename preview"),
            ParsedInput::Command(SlashCommand::Lsp {
                operation: LspCommand::RenamePreview {
                    server_id: "rust".to_owned(),
                    path: "src/main.rs".to_owned(),
                    line: 12,
                    character: 7,
                    new_name: "NewName".to_owned()
                }
            })
        );
        assert_eq!(
            registry
                .parse("/lsp apply lsp-preview:abc")
                .expect("apply preview"),
            ParsedInput::Command(SlashCommand::Lsp {
                operation: LspCommand::ApplyPreview {
                    preview_id: "lsp-preview:abc".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/think xhigh").expect("reasoning"),
            ParsedInput::Command(SlashCommand::Reasoning {
                level: ReasoningLevel::Xhigh
            })
        );
        assert_eq!(
            registry.parse("/logout").expect("logout"),
            ParsedInput::Command(SlashCommand::Logout {
                provider: "openai".to_owned()
            })
        );
        assert_eq!(
            registry
                .parse("/login")
                .expect_err("slash login removed")
                .code,
            "unknown-command"
        );
        assert_eq!(
            registry
                .parse("/reasoning extreme")
                .expect_err("invalid")
                .code,
            "invalid-reasoning-level"
        );
        assert_eq!(
            registry.parse("/language ja").expect("language"),
            ParsedInput::Command(SlashCommand::Language {
                language: Some(UiLanguage::Ja)
            })
        );
        assert_eq!(
            registry.parse("/vector setup").expect("vector setup"),
            ParsedInput::Command(SlashCommand::Vector {
                operation: VectorCommand::Setup
            })
        );
        assert_eq!(
            registry
                .parse("/approve invocation:1 project")
                .expect("approve"),
            ParsedInput::Command(SlashCommand::Approve {
                invocation_id: "invocation:1".to_owned(),
                scope: GrantScope::Project
            })
        );
    }

    #[test]
    fn patch_review_and_undo_commands_are_explicit() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.parse("/undo").expect("undo latest"),
            ParsedInput::Command(SlashCommand::Undo { patch_id: None })
        );
        assert_eq!(
            registry.parse("/undo list").expect("undo list"),
            ParsedInput::Command(SlashCommand::PatchList)
        );
        assert_eq!(
            registry.parse("/review staged").expect("review staged"),
            ParsedInput::Command(SlashCommand::Review { staged: true })
        );
    }

    #[test]
    fn mcp_plugin_and_skill_commands_are_strict() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry
                .parse("/mcp add-http docs https://example.com/mcp")
                .expect("add http"),
            ParsedInput::Command(SlashCommand::Mcp {
                operation: McpCommand::AddHttp {
                    server_id: "docs".to_owned(),
                    endpoint: "https://example.com/mcp".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/mcp disable docs").expect("disable"),
            ParsedInput::Command(SlashCommand::Mcp {
                operation: McpCommand::Disable {
                    server_id: "docs".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/mcp reconnect github").expect("mcp"),
            ParsedInput::Command(SlashCommand::Mcp {
                operation: McpCommand::Connect {
                    server_id: "github".to_owned(),
                    force: true
                }
            })
        );
        assert_eq!(
            registry.parse("/mcp auth start remote").expect("mcp oauth"),
            ParsedInput::Command(SlashCommand::Mcp {
                operation: McpCommand::AuthStart {
                    server_id: "remote".to_owned()
                }
            })
        );
        assert_eq!(
            registry
                .parse("/plugins enable demo abc123")
                .expect("plugin"),
            ParsedInput::Command(SlashCommand::Plugins {
                operation: PluginCommand::Enable {
                    plugin_id: "demo".to_owned(),
                    review_hash: "abc123".to_owned()
                }
            })
        );
        assert_eq!(
            registry.parse("/skills search rust review").expect("skill"),
            ParsedInput::Command(SlashCommand::Skills {
                operation: SkillCommand::Search {
                    query: "rust review".to_owned()
                }
            })
        );
        assert_eq!(
            registry
                .parse("/mcp read only-one-arg")
                .expect_err("invalid mcp")
                .code,
            "invalid-mcp-command"
        );
    }

    #[test]
    fn unknown_command_is_friendly_error() {
        let error = CommandRegistry::new()
            .parse("/unknown")
            .expect_err("unknown command");
        assert_eq!(error.code, "unknown-command");
        assert!(error.message.contains("/help"));
    }
}
