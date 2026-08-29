#![forbid(unsafe_code)]

//! Default < Global < Project < Session < Runtime 的严格配置合并。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub code: String,
    pub message: String,
}

impl ConfigError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for ConfigError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    Lite,
    #[default]
    Balanced,
    Full,
    Custom,
}

impl Display for RuntimeMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Lite => "lite",
            Self::Balanced => "balanced",
            Self::Full => "full",
            Self::Custom => "custom",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VectorMode {
    #[default]
    Auto,
    On,
    Off,
}

impl Display for VectorMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Manual,
    AcceptEdits,
    Auto,
    Full,
    Bypass,
    /// 旧版兼容别名：等价于 Manual。
    Safe,
    /// 旧版兼容别名：等价于 Auto。
    Ask,
    /// 旧版自定义规则模式。
    Custom,
}

impl Display for PermissionMode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Full => "full",
            Self::Bypass => "bypass",
            Self::Safe => "safe",
            Self::Ask => "ask",
            Self::Custom => "custom",
        })
    }
}

impl Display for LogLevel {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct UiPatch {
    pub statusbar: Option<bool>,
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct VectorPatch {
    pub mode: Option<VectorMode>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TracePatch {
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LoggingPatch {
    pub level: Option<LogLevel>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PermissionPatch {
    pub mode: Option<PermissionMode>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct FailoverPatch {
    pub enabled: Option<bool>,
    pub cost_confirmed: Option<bool>,
    pub targets: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AgentPatch {
    #[serde(rename = "max")]
    pub max_agents: Option<u64>,
    #[serde(rename = "parallel")]
    pub max_parallel_agents: Option<u64>,
    #[serde(rename = "tokens")]
    pub max_total_tokens: Option<u64>,
    #[serde(rename = "tools")]
    pub max_tool_calls: Option<u64>,
    #[serde(rename = "runtime_ms")]
    pub max_runtime_millis: Option<u64>,
    #[serde(rename = "retries")]
    pub max_retries: Option<u32>,
    #[serde(rename = "cost")]
    pub max_cost_units: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ContextPatch {
    #[serde(rename = "window")]
    pub window_tokens: Option<u32>,
    #[serde(rename = "tool_reserve")]
    pub reserved_tool_tokens: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ToolPatch {
    #[serde(rename = "max_on_demand")]
    pub max_on_demand: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct SettingsPatch {
    pub mode: Option<RuntimeMode>,
    #[serde(default)]
    pub ui: UiPatch,
    #[serde(default)]
    pub vector: VectorPatch,
    #[serde(default)]
    pub trace: TracePatch,
    #[serde(default)]
    pub logging: LoggingPatch,
    #[serde(default)]
    pub permissions: PermissionPatch,
    #[serde(default)]
    pub failover: FailoverPatch,
    #[serde(default)]
    pub agents: AgentPatch,
    #[serde(default)]
    pub context: ContextPatch,
    #[serde(default)]
    pub tools: ToolPatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectiveSettings {
    pub mode: RuntimeMode,
    pub ui_statusbar: bool,
    pub ui_language: String,
    pub vector_mode: VectorMode,
    pub trace_enabled: bool,
    pub log_level: LogLevel,
    pub permission_mode: PermissionMode,
    pub failover_enabled: bool,
    pub failover_cost_confirmed: bool,
    pub failover_targets: String,
    pub agent_overrides: AgentPatch,
    pub context_overrides: ContextPatch,
    pub tool_overrides: ToolPatch,
}

impl Default for EffectiveSettings {
    fn default() -> Self {
        Self {
            mode: RuntimeMode::Balanced,
            ui_statusbar: true,
            ui_language: "zh-CN".to_owned(),
            vector_mode: VectorMode::Auto,
            trace_enabled: false,
            log_level: LogLevel::Info,
            permission_mode: PermissionMode::Manual,
            failover_enabled: false,
            failover_cost_confirmed: false,
            failover_targets: String::new(),
            agent_overrides: AgentPatch::default(),
            context_overrides: ContextPatch::default(),
            tool_overrides: ToolPatch::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModeProfile {
    pub mode: RuntimeMode,
    pub max_agents: usize,
    pub max_parallel_agents: usize,
    pub max_total_tokens: u64,
    pub max_tool_calls: u64,
    pub max_runtime_millis: u64,
    pub max_retries: u32,
    pub max_cost_units: u64,
    pub context_window_tokens: u32,
    pub reserved_tool_tokens: u32,
    pub max_on_demand_tools: usize,
    pub proactive_semantic_retrieval: bool,
}

impl ModeProfile {
    #[must_use]
    pub fn resolve(settings: &EffectiveSettings) -> Self {
        let mut profile = match settings.mode {
            RuntimeMode::Lite => Self {
                mode: RuntimeMode::Lite,
                max_agents: 2,
                max_parallel_agents: 1,
                max_total_tokens: 40_000,
                max_tool_calls: 32,
                max_runtime_millis: 10 * 60 * 1_000,
                max_retries: 1,
                max_cost_units: 8,
                context_window_tokens: 8_192,
                reserved_tool_tokens: 512,
                max_on_demand_tools: 2,
                proactive_semantic_retrieval: false,
            },
            RuntimeMode::Balanced | RuntimeMode::Custom => Self {
                mode: settings.mode,
                max_agents: 8,
                max_parallel_agents: 4,
                max_total_tokens: 100_000,
                max_tool_calls: 128,
                max_runtime_millis: 30 * 60 * 1_000,
                max_retries: 3,
                max_cost_units: 32,
                context_window_tokens: 8_192,
                reserved_tool_tokens: 1_024,
                max_on_demand_tools: 8,
                proactive_semantic_retrieval: true,
            },
            RuntimeMode::Full => Self {
                mode: RuntimeMode::Full,
                max_agents: 12,
                max_parallel_agents: 6,
                max_total_tokens: 250_000,
                max_tool_calls: 256,
                max_runtime_millis: 60 * 60 * 1_000,
                max_retries: 4,
                max_cost_units: 64,
                context_window_tokens: 16_384,
                reserved_tool_tokens: 2_048,
                max_on_demand_tools: 16,
                proactive_semantic_retrieval: true,
            },
        };
        let agents = &settings.agent_overrides;
        profile.max_agents = agents
            .max_agents
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(profile.max_agents);
        profile.max_parallel_agents = agents
            .max_parallel_agents
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(profile.max_parallel_agents)
            .min(profile.max_agents);
        profile.max_total_tokens = agents.max_total_tokens.unwrap_or(profile.max_total_tokens);
        profile.max_tool_calls = agents.max_tool_calls.unwrap_or(profile.max_tool_calls);
        profile.max_runtime_millis = agents
            .max_runtime_millis
            .unwrap_or(profile.max_runtime_millis);
        profile.max_retries = agents.max_retries.unwrap_or(profile.max_retries);
        profile.max_cost_units = agents.max_cost_units.unwrap_or(profile.max_cost_units);
        profile.context_window_tokens = settings
            .context_overrides
            .window_tokens
            .unwrap_or(profile.context_window_tokens);
        profile.reserved_tool_tokens = settings
            .context_overrides
            .reserved_tool_tokens
            .unwrap_or(profile.reserved_tool_tokens);
        profile.max_on_demand_tools = settings
            .tool_overrides
            .max_on_demand
            .unwrap_or(profile.max_on_demand_tools);
        profile
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigLayer {
    Default,
    Global,
    Project,
    Session,
    Runtime,
}

impl Display for ConfigLayer {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Project => "project",
            Self::Session => "session",
            Self::Runtime => "runtime",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectiveConfigView {
    pub settings: EffectiveSettings,
    pub provenance: BTreeMap<String, ConfigLayer>,
    pub global_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
}

impl EffectiveConfigView {
    /// 将有效配置展开成与 `/settings` 一致的稳定 dotted-key 视图。
    #[must_use]
    pub fn values(&self) -> BTreeMap<String, String> {
        let profile = ModeProfile::resolve(&self.settings);
        BTreeMap::from([
            ("mode".to_owned(), self.settings.mode.to_string()),
            (
                "ui.statusbar".to_owned(),
                self.settings.ui_statusbar.to_string(),
            ),
            ("ui.language".to_owned(), self.settings.ui_language.clone()),
            (
                "vector.mode".to_owned(),
                self.settings.vector_mode.to_string(),
            ),
            (
                "trace.enabled".to_owned(),
                self.settings.trace_enabled.to_string(),
            ),
            (
                "logging.level".to_owned(),
                self.settings.log_level.to_string(),
            ),
            (
                "permissions.mode".to_owned(),
                self.settings.permission_mode.to_string(),
            ),
            (
                "failover.enabled".to_owned(),
                self.settings.failover_enabled.to_string(),
            ),
            (
                "failover.cost-confirmed".to_owned(),
                self.settings.failover_cost_confirmed.to_string(),
            ),
            (
                "failover.targets".to_owned(),
                self.settings.failover_targets.clone(),
            ),
            ("agents.max".to_owned(), profile.max_agents.to_string()),
            (
                "agents.parallel".to_owned(),
                profile.max_parallel_agents.to_string(),
            ),
            (
                "agents.tokens".to_owned(),
                profile.max_total_tokens.to_string(),
            ),
            (
                "agents.tools".to_owned(),
                profile.max_tool_calls.to_string(),
            ),
            (
                "agents.runtime-ms".to_owned(),
                profile.max_runtime_millis.to_string(),
            ),
            ("agents.retries".to_owned(), profile.max_retries.to_string()),
            ("agents.cost".to_owned(), profile.max_cost_units.to_string()),
            (
                "context.window".to_owned(),
                profile.context_window_tokens.to_string(),
            ),
            (
                "context.tool-reserve".to_owned(),
                profile.reserved_tool_tokens.to_string(),
            ),
            (
                "tools.max-on-demand".to_owned(),
                profile.max_on_demand_tools.to_string(),
            ),
        ])
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new(None, None, &BTreeMap::new())
            .expect("empty built-in Kernary settings must always be valid")
    }
}

#[derive(Clone, Debug)]
pub struct ConfigManager {
    global: SettingsPatch,
    project: SettingsPatch,
    session: SettingsPatch,
    runtime: SettingsPatch,
    global_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct ConfigFile {
    schema_version: u32,
    #[serde(default)]
    settings: SettingsPatch,
}

impl ConfigManager {
    pub fn new(
        global: Option<(PathBuf, SettingsPatch)>,
        project: Option<(PathBuf, SettingsPatch)>,
        session_values: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let mut manager = Self {
            global_path: global.as_ref().map(|(path, _)| path.clone()),
            project_path: project.as_ref().map(|(path, _)| path.clone()),
            global: global.map_or_else(SettingsPatch::default, |(_, patch)| patch),
            project: project.map_or_else(SettingsPatch::default, |(_, patch)| patch),
            session: SettingsPatch::default(),
            runtime: SettingsPatch::default(),
        };
        for (key, value) in session_values {
            manager.set_session(key, value)?;
        }
        manager.validate_effective()?;
        Ok(manager)
    }

    #[must_use]
    pub fn effective(&self) -> EffectiveConfigView {
        let mut settings = EffectiveSettings::default();
        let mut provenance = default_provenance();
        for (layer, patch) in [
            (ConfigLayer::Global, &self.global),
            (ConfigLayer::Project, &self.project),
            (ConfigLayer::Session, &self.session),
            (ConfigLayer::Runtime, &self.runtime),
        ] {
            apply_patch(&mut settings, &mut provenance, layer, patch);
        }
        let mode_layer = provenance
            .get("mode")
            .copied()
            .unwrap_or(ConfigLayer::Default);
        for (key, overridden) in [
            ("agents.max", settings.agent_overrides.max_agents.is_some()),
            (
                "agents.parallel",
                settings.agent_overrides.max_parallel_agents.is_some(),
            ),
            (
                "agents.tokens",
                settings.agent_overrides.max_total_tokens.is_some(),
            ),
            (
                "agents.tools",
                settings.agent_overrides.max_tool_calls.is_some(),
            ),
            (
                "agents.runtime-ms",
                settings.agent_overrides.max_runtime_millis.is_some(),
            ),
            (
                "agents.retries",
                settings.agent_overrides.max_retries.is_some(),
            ),
            (
                "agents.cost",
                settings.agent_overrides.max_cost_units.is_some(),
            ),
            (
                "context.window",
                settings.context_overrides.window_tokens.is_some(),
            ),
            (
                "context.tool-reserve",
                settings.context_overrides.reserved_tool_tokens.is_some(),
            ),
            (
                "tools.max-on-demand",
                settings.tool_overrides.max_on_demand.is_some(),
            ),
        ] {
            if !overridden {
                provenance.insert(key.to_owned(), mode_layer);
            }
        }
        EffectiveConfigView {
            settings,
            provenance,
            global_path: self.global_path.clone(),
            project_path: self.project_path.clone(),
        }
    }

    pub fn set_session(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let previous = self.session.clone();
        set_patch_value(&mut self.session, key, value)?;
        if let Err(error) = self.validate_effective() {
            self.session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn clear_session(&mut self, key: &str) -> Result<(), ConfigError> {
        let previous = self.session.clone();
        clear_patch_value(&mut self.session, key)?;
        if let Err(error) = self.validate_effective() {
            self.session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn replace_session(
        &mut self,
        values: &BTreeMap<String, String>,
    ) -> Result<(), ConfigError> {
        let previous = self.session.clone();
        self.session = SettingsPatch::default();
        for (key, value) in values {
            if let Err(error) = set_patch_value(&mut self.session, key, value) {
                self.session = previous;
                return Err(error);
            }
        }
        if let Err(error) = self.validate_effective() {
            self.session = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_runtime(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let previous = self.runtime.clone();
        set_patch_value(&mut self.runtime, key, value)?;
        if let Err(error) = self.validate_effective() {
            self.runtime = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_runtime_many(&mut self, values: &[(&str, &str)]) -> Result<(), ConfigError> {
        let previous = self.runtime.clone();
        for (key, value) in values {
            if let Err(error) = set_patch_value(&mut self.runtime, key, value) {
                self.runtime = previous;
                return Err(error);
            }
        }
        if let Err(error) = self.validate_effective() {
            self.runtime = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn clear_runtime(&mut self, key: &str) -> Result<(), ConfigError> {
        let previous = self.runtime.clone();
        clear_patch_value(&mut self.runtime, key)?;
        if let Err(error) = self.validate_effective() {
            self.runtime = previous;
            return Err(error);
        }
        Ok(())
    }

    fn validate_effective(&self) -> Result<(), ConfigError> {
        let settings = self.effective().settings;
        let profile = ModeProfile::resolve(&settings);
        if profile.max_agents == 0
            || profile.max_parallel_agents == 0
            || profile.max_parallel_agents > profile.max_agents
            || profile.max_total_tokens == 0
            || profile.max_runtime_millis == 0
            || profile.max_cost_units == 0
            || profile.context_window_tokens < 2_048
            || u64::from(profile.reserved_tool_tokens).saturating_add(1_536)
                >= u64::from(profile.context_window_tokens)
            || profile.max_on_demand_tools > 64
            || (settings.failover_enabled
                && (!settings.failover_cost_confirmed
                    || !valid_failover_targets(&settings.failover_targets)))
        {
            return Err(ConfigError::new(
                "config-effective-invalid",
                format!("{profile:?}"),
            ));
        }
        Ok(())
    }
}

pub fn load_config_file(path: impl AsRef<Path>) -> Result<SettingsPatch, ConfigError> {
    let path = path.as_ref();
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ConfigError::new("config-io", error.to_string()))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            "config-size-or-type",
            path.display().to_string(),
        ));
    }
    let file: ConfigFile = toml::from_str(
        &fs::read_to_string(path)
            .map_err(|error| ConfigError::new("config-io", error.to_string()))?,
    )
    .map_err(|error| ConfigError::new("config-toml", error.to_string()))?;
    if file.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::new(
            "config-schema-unsupported",
            file.schema_version.to_string(),
        ));
    }
    Ok(file.settings)
}

pub fn supported_setting_keys() -> &'static [&'static str] {
    &[
        "mode",
        "ui.statusbar",
        "ui.language",
        "vector.mode",
        "trace.enabled",
        "logging.level",
        "permissions.mode",
        "failover.enabled",
        "failover.cost-confirmed",
        "failover.targets",
        "agents.max",
        "agents.parallel",
        "agents.tokens",
        "agents.tools",
        "agents.runtime-ms",
        "agents.retries",
        "agents.cost",
        "context.window",
        "context.tool-reserve",
        "tools.max-on-demand",
    ]
}

fn set_patch_value(patch: &mut SettingsPatch, key: &str, value: &str) -> Result<(), ConfigError> {
    match key {
        "mode" => patch.mode = Some(parse_mode(value)?),
        "ui.statusbar" => patch.ui.statusbar = Some(parse_bool(value)?),
        "ui.language" => {
            if !matches!(value, "en" | "zh-CN" | "zh-TW" | "ja") {
                return Err(ConfigError::new("config-language-invalid", value));
            }
            patch.ui.language = Some(value.to_owned());
        }
        "vector.mode" => patch.vector.mode = Some(parse_vector_mode(value)?),
        "trace.enabled" => patch.trace.enabled = Some(parse_bool(value)?),
        "logging.level" => patch.logging.level = Some(parse_log_level(value)?),
        "permissions.mode" => patch.permissions.mode = Some(parse_permission_mode(value)?),
        "failover.enabled" => patch.failover.enabled = Some(parse_bool(value)?),
        "failover.cost-confirmed" => patch.failover.cost_confirmed = Some(parse_bool(value)?),
        "failover.targets" => {
            if !value.is_empty() && !valid_failover_targets(value) {
                return Err(ConfigError::new("config-failover-targets-invalid", value));
            }
            patch.failover.targets = Some(value.to_owned());
        }
        "agents.max" => patch.agents.max_agents = Some(parse_u64(value, key)?),
        "agents.parallel" => patch.agents.max_parallel_agents = Some(parse_u64(value, key)?),
        "agents.tokens" => patch.agents.max_total_tokens = Some(parse_u64(value, key)?),
        "agents.tools" => patch.agents.max_tool_calls = Some(parse_u64(value, key)?),
        "agents.runtime-ms" => patch.agents.max_runtime_millis = Some(parse_u64(value, key)?),
        "agents.retries" => {
            patch.agents.max_retries = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| ConfigError::new("config-value-invalid", key))?,
            )
        }
        "agents.cost" => patch.agents.max_cost_units = Some(parse_u64(value, key)?),
        "context.window" => patch.context.window_tokens = Some(parse_u32(value, key)?),
        "context.tool-reserve" => patch.context.reserved_tool_tokens = Some(parse_u32(value, key)?),
        "tools.max-on-demand" => {
            patch.tools.max_on_demand = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| ConfigError::new("config-value-invalid", key))?,
            )
        }
        _ => return Err(ConfigError::new("config-key-unsupported", key)),
    }
    Ok(())
}

fn clear_patch_value(patch: &mut SettingsPatch, key: &str) -> Result<(), ConfigError> {
    match key {
        "mode" => patch.mode = None,
        "ui.statusbar" => patch.ui.statusbar = None,
        "ui.language" => patch.ui.language = None,
        "vector.mode" => patch.vector.mode = None,
        "trace.enabled" => patch.trace.enabled = None,
        "logging.level" => patch.logging.level = None,
        "permissions.mode" => patch.permissions.mode = None,
        "failover.enabled" => patch.failover.enabled = None,
        "failover.cost-confirmed" => patch.failover.cost_confirmed = None,
        "failover.targets" => patch.failover.targets = None,
        "agents.max" => patch.agents.max_agents = None,
        "agents.parallel" => patch.agents.max_parallel_agents = None,
        "agents.tokens" => patch.agents.max_total_tokens = None,
        "agents.tools" => patch.agents.max_tool_calls = None,
        "agents.runtime-ms" => patch.agents.max_runtime_millis = None,
        "agents.retries" => patch.agents.max_retries = None,
        "agents.cost" => patch.agents.max_cost_units = None,
        "context.window" => patch.context.window_tokens = None,
        "context.tool-reserve" => patch.context.reserved_tool_tokens = None,
        "tools.max-on-demand" => patch.tools.max_on_demand = None,
        _ => return Err(ConfigError::new("config-key-unsupported", key)),
    }
    Ok(())
}

fn apply_patch(
    settings: &mut EffectiveSettings,
    provenance: &mut BTreeMap<String, ConfigLayer>,
    layer: ConfigLayer,
    patch: &SettingsPatch,
) {
    macro_rules! apply {
        ($value:expr, $target:expr, $key:literal) => {
            if let Some(value) = $value {
                $target = value;
                provenance.insert($key.to_owned(), layer);
            }
        };
    }
    macro_rules! apply_option {
        ($value:expr, $target:expr, $key:literal) => {
            if let Some(value) = $value {
                $target = Some(value);
                provenance.insert($key.to_owned(), layer);
            }
        };
    }
    apply!(patch.mode, settings.mode, "mode");
    apply!(patch.ui.statusbar, settings.ui_statusbar, "ui.statusbar");
    if let Some(value) = &patch.ui.language {
        settings.ui_language.clone_from(value);
        provenance.insert("ui.language".to_owned(), layer);
    }
    apply!(patch.vector.mode, settings.vector_mode, "vector.mode");
    apply!(patch.trace.enabled, settings.trace_enabled, "trace.enabled");
    apply!(patch.logging.level, settings.log_level, "logging.level");
    apply!(
        patch.permissions.mode,
        settings.permission_mode,
        "permissions.mode"
    );
    apply!(
        patch.failover.enabled,
        settings.failover_enabled,
        "failover.enabled"
    );
    apply!(
        patch.failover.cost_confirmed,
        settings.failover_cost_confirmed,
        "failover.cost-confirmed"
    );
    if let Some(value) = &patch.failover.targets {
        settings.failover_targets.clone_from(value);
        provenance.insert("failover.targets".to_owned(), layer);
    }
    apply_option!(
        patch.agents.max_agents,
        settings.agent_overrides.max_agents,
        "agents.max"
    );
    apply_option!(
        patch.agents.max_parallel_agents,
        settings.agent_overrides.max_parallel_agents,
        "agents.parallel"
    );
    apply_option!(
        patch.agents.max_total_tokens,
        settings.agent_overrides.max_total_tokens,
        "agents.tokens"
    );
    apply_option!(
        patch.agents.max_tool_calls,
        settings.agent_overrides.max_tool_calls,
        "agents.tools"
    );
    apply_option!(
        patch.agents.max_runtime_millis,
        settings.agent_overrides.max_runtime_millis,
        "agents.runtime-ms"
    );
    apply_option!(
        patch.agents.max_retries,
        settings.agent_overrides.max_retries,
        "agents.retries"
    );
    apply_option!(
        patch.agents.max_cost_units,
        settings.agent_overrides.max_cost_units,
        "agents.cost"
    );
    apply_option!(
        patch.context.window_tokens,
        settings.context_overrides.window_tokens,
        "context.window"
    );
    apply_option!(
        patch.context.reserved_tool_tokens,
        settings.context_overrides.reserved_tool_tokens,
        "context.tool-reserve"
    );
    apply_option!(
        patch.tools.max_on_demand,
        settings.tool_overrides.max_on_demand,
        "tools.max-on-demand"
    );
}

fn default_provenance() -> BTreeMap<String, ConfigLayer> {
    supported_setting_keys()
        .iter()
        .map(|key| ((*key).to_owned(), ConfigLayer::Default))
        .collect()
}

fn parse_mode(value: &str) -> Result<RuntimeMode, ConfigError> {
    match value {
        "lite" => Ok(RuntimeMode::Lite),
        "balanced" => Ok(RuntimeMode::Balanced),
        "full" => Ok(RuntimeMode::Full),
        "custom" => Ok(RuntimeMode::Custom),
        _ => Err(ConfigError::new("config-mode-invalid", value)),
    }
}

fn parse_vector_mode(value: &str) -> Result<VectorMode, ConfigError> {
    match value {
        "auto" => Ok(VectorMode::Auto),
        "on" => Ok(VectorMode::On),
        "off" => Ok(VectorMode::Off),
        _ => Err(ConfigError::new("config-vector-mode-invalid", value)),
    }
}

fn parse_log_level(value: &str) -> Result<LogLevel, ConfigError> {
    match value {
        "error" => Ok(LogLevel::Error),
        "warn" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(ConfigError::new("config-log-level-invalid", value)),
    }
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, ConfigError> {
    match value {
        "manual" | "default" => Ok(PermissionMode::Manual),
        "accept-edits" | "edit" => Ok(PermissionMode::AcceptEdits),
        "bypass" => Ok(PermissionMode::Bypass),
        "safe" => Ok(PermissionMode::Safe),
        "ask" => Ok(PermissionMode::Ask),
        "auto" => Ok(PermissionMode::Auto),
        "full" => Ok(PermissionMode::Full),
        "custom" => Ok(PermissionMode::Custom),
        _ => Err(ConfigError::new("config-permission-mode-invalid", value)),
    }
}

fn valid_failover_targets(value: &str) -> bool {
    let targets = value.split(',').map(str::trim).collect::<Vec<_>>();
    !targets.is_empty()
        && targets.len() <= 16
        && targets.iter().all(|target| {
            target.split_once('/').is_some_and(|(provider, model)| {
                !provider.is_empty()
                    && !model.is_empty()
                    && provider.len() <= 128
                    && model.len() <= 256
                    && !provider.chars().any(char::is_whitespace)
                    && !model.chars().any(char::is_whitespace)
            })
        })
}

fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    match value {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(ConfigError::new("config-bool-invalid", value)),
    }
}

fn parse_u64(value: &str, key: &str) -> Result<u64, ConfigError> {
    value
        .parse::<u64>()
        .map_err(|_| ConfigError::new("config-value-invalid", key))
}

fn parse_u32(value: &str, key: &str) -> Result<u32, ConfigError> {
    value
        .parse::<u32>()
        .map_err(|_| ConfigError::new("config-value-invalid", key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn five_layers_merge_in_order_and_mode_profile_changes_real_budgets() {
        let mut global = SettingsPatch::default();
        set_patch_value(&mut global, "mode", "full").expect("global");
        set_patch_value(&mut global, "ui.statusbar", "false").expect("global");
        let mut project = SettingsPatch::default();
        set_patch_value(&mut project, "mode", "balanced").expect("project");
        let session = BTreeMap::from([("mode".to_owned(), "lite".to_owned())]);
        let mut manager = ConfigManager::new(
            Some((PathBuf::from("global"), global)),
            Some((PathBuf::from("project"), project)),
            &session,
        )
        .expect("manager");
        assert_eq!(manager.effective().settings.mode, RuntimeMode::Lite);
        assert!(!manager.effective().settings.ui_statusbar);
        assert_eq!(
            manager.effective().provenance["agents.max"],
            ConfigLayer::Session
        );
        manager.set_runtime("mode", "full").expect("runtime");
        manager
            .set_runtime("tools.max-on-demand", "12")
            .expect("tools");
        manager
            .set_runtime("permissions.mode", "full")
            .expect("permissions");
        manager.set_runtime("agents.cost", "11").expect("cost");
        manager.set_runtime("ui.language", "ja").expect("language");
        manager
            .set_runtime_many(&[
                ("failover.targets", "fake/deterministic"),
                ("failover.cost-confirmed", "true"),
                ("failover.enabled", "true"),
            ])
            .expect("failover");
        let view = manager.effective();
        assert_eq!(view.settings.mode, RuntimeMode::Full);
        assert_eq!(view.provenance["mode"], ConfigLayer::Runtime);
        assert_eq!(view.settings.permission_mode, PermissionMode::Full);
        assert!(view.settings.failover_enabled);
        assert_eq!(view.settings.ui_language, "ja");
        let profile = ModeProfile::resolve(&view.settings);
        assert_eq!(profile.max_parallel_agents, 6);
        assert_eq!(profile.max_on_demand_tools, 12);
        assert_eq!(profile.max_cost_units, 11);
    }

    #[test]
    fn strict_versioned_toml_and_invalid_effective_values_fail_closed() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("kernary.toml");
        fs::write(
            &path,
            "schema_version = 1\n[settings]\nmode = \"balanced\"\n[settings.vector]\nmode = \"off\"\n[settings.agents]\nmax = 10\nparallel = 5\n",
        )
        .expect("config");
        let patch = load_config_file(&path).expect("load");
        assert_eq!(patch.mode, Some(RuntimeMode::Balanced));
        assert_eq!(patch.vector.mode, Some(VectorMode::Off));
        assert_eq!(patch.agents.max_agents, Some(10));
        assert_eq!(patch.agents.max_parallel_agents, Some(5));
        fs::write(&path, "schema_version = 99\n").expect("future");
        assert_eq!(
            load_config_file(&path).expect_err("future").code,
            "config-schema-unsupported"
        );
        let mut manager = ConfigManager::new(None, None, &BTreeMap::new()).expect("manager");
        assert_eq!(
            manager
                .set_runtime("agents.parallel", "0")
                .expect_err("invalid")
                .code,
            "config-effective-invalid"
        );
        assert_eq!(
            manager.effective().provenance["agents.parallel"],
            ConfigLayer::Default
        );
        assert_eq!(
            manager
                .set_runtime("ui.language", "fr")
                .expect_err("invalid language")
                .code,
            "config-language-invalid"
        );
    }

    #[test]
    fn permission_levels_and_session_replacement_are_strict() {
        let mut manager = ConfigManager::default();
        assert_eq!(
            manager.effective().settings.permission_mode,
            PermissionMode::Manual
        );
        for (value, expected) in [
            ("manual", PermissionMode::Manual),
            ("edit", PermissionMode::AcceptEdits),
            ("auto", PermissionMode::Auto),
            ("full", PermissionMode::Full),
            ("bypass", PermissionMode::Bypass),
        ] {
            manager
                .replace_session(&BTreeMap::from([(
                    "permissions.mode".to_owned(),
                    value.to_owned(),
                )]))
                .expect("replace session");
            assert_eq!(manager.effective().settings.permission_mode, expected);
        }
        assert!(
            manager
                .replace_session(&BTreeMap::from([(
                    "permissions.mode".to_owned(),
                    "danger".to_owned(),
                )]))
                .is_err()
        );
        assert_eq!(
            manager.effective().settings.permission_mode,
            PermissionMode::Bypass
        );
    }
}
