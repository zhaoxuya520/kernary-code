#![forbid(unsafe_code)]

//! 纯终端输入、命令元数据和 Renderer。

mod brand;
mod command;
mod language;
mod render;
mod tui;

pub use brand::{
    MASCOT_NAME, PRODUCT_NAME, PRODUCT_SHORT_NAME, TAGLINE, compact_mark, mascot_lines,
};
pub use command::{
    AgentDisplayMode, BrowserCommand, BudgetCommand, CommandRegistry, CompactCommandMode,
    FailoverCommand, GitCommand, IndexCommand, InputSuggestion, LspCommand, McpCommand,
    MemoryCommand, ParsedInput, PermissionCommand, PluginCommand, ProviderCommand, QueueCommand,
    SettingLayer, SettingsCommand, SkillCommand, SlashCommand, TeamCommand, TraceCommand,
    VectorCommand,
};
pub use language::{LanguagePack, UiLanguage};
pub use render::{ActivityIcon, JsonRenderer, PlainRenderer, RenderStyle, TerminalCapabilities};
pub use tui::{
    BackendResponse, CancelAction, CancelController, InputPrompt, SecretPrompt, TerminalBackend,
    TerminalSnapshot, TuiOptions, run_tui,
};
