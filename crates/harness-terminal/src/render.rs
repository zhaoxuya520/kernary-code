use std::env;

use harness_event::{EventEnvelope, HarnessEvent};

use crate::{PRODUCT_SHORT_NAME, compact_mark};

/// Unicode 图标及 ASCII fallback。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityIcon {
    Waiting,
    Thinking,
    Running,
    Tool,
    Editing,
    Searching,
    Done,
    Warning,
    Failed,
    Permission,
    Background,
}

impl ActivityIcon {
    #[must_use]
    pub const fn render(self, ascii: bool) -> &'static str {
        if ascii {
            match self {
                Self::Waiting => "[WAIT]",
                Self::Thinking => "[THINK]",
                Self::Running => "[RUN]",
                Self::Tool => "[TOOL]",
                Self::Editing => "[EDIT]",
                Self::Searching => "[SEARCH]",
                Self::Done => "[DONE]",
                Self::Warning => "[WARN]",
                Self::Failed => "[FAIL]",
                Self::Permission => "[PERM]",
                Self::Background => "[BG]",
            }
        } else {
            match self {
                Self::Waiting => "○",
                Self::Thinking => "◇",
                Self::Running => "◆",
                Self::Tool => "⚡",
                Self::Editing => "✎",
                Self::Searching => "⌕",
                Self::Done => "✓",
                Self::Warning => "!",
                Self::Failed => "✕",
                Self::Permission => "⛨",
                Self::Background => "∞",
            }
        }
    }
}

/// Renderer 样式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderStyle {
    pub ascii: bool,
    pub color: bool,
}

/// 低成本 terminal capability probe。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub is_tty: bool,
    pub color: bool,
    pub unicode: bool,
    pub terminal_name: Option<String>,
}

impl TerminalCapabilities {
    #[must_use]
    pub fn detect(is_tty: bool, force_ascii: bool, force_no_color: bool) -> Self {
        let terminal_name = env::var("TERM").ok().or_else(|| {
            env::var("WT_SESSION")
                .ok()
                .map(|_| "windows-terminal".to_owned())
        });
        let no_color = env::var_os("NO_COLOR").is_some();
        let dumb = terminal_name.as_deref() == Some("dumb");
        Self {
            is_tty,
            color: is_tty && !force_no_color && !no_color && !dumb,
            unicode: !force_ascii && !dumb,
            terminal_name,
        }
    }

    #[must_use]
    pub const fn style(&self) -> RenderStyle {
        RenderStyle {
            ascii: !self.unicode,
            color: self.color,
        }
    }
}

/// 稳定文本 Renderer，可用于 Plain/日志/TUI history。
#[derive(Clone, Copy, Debug)]
pub struct PlainRenderer {
    style: RenderStyle,
}

impl PlainRenderer {
    #[must_use]
    pub const fn new(style: RenderStyle) -> Self {
        Self { style }
    }

    #[must_use]
    pub fn render_event(&self, envelope: &EventEnvelope) -> String {
        let rendered = match &envelope.event {
            HarnessEvent::SystemStarted { version, mode } => {
                format!("{} {version} · mode {mode}", compact_mark(self.style.ascii))
            }
            HarnessEvent::SystemReady { project_root } => {
                format!(
                    "{} Ready · {project_root}",
                    ActivityIcon::Done.render(self.style.ascii)
                )
            }
            HarnessEvent::SessionChanged { status } => {
                format!(
                    "{} Session {status}",
                    ActivityIcon::Running.render(self.style.ascii)
                )
            }
            HarnessEvent::GoalChanged { text, locked, .. } => format!(
                "{} Goal{}: {}",
                ActivityIcon::Done.render(self.style.ascii),
                if *locked { " [locked]" } else { "" },
                text.as_deref().unwrap_or("<empty>")
            ),
            HarnessEvent::ModelChanged {
                provider,
                model,
                reasoning_requested,
                reasoning_effective,
                reasoning_mapping,
            } => {
                if (provider == "fake" && model == "deterministic")
                    || (provider == "kernary-internal" && model == "unconfigured")
                {
                    "Model 未配置 · 使用 /connect 和 /model 完成首次设置".to_owned()
                } else {
                    format!(
                        "Model {provider}/{model} · reasoning {reasoning_requested} → {} ({reasoning_mapping})",
                        reasoning_effective.as_deref().unwrap_or("unsupported")
                    )
                }
            }
            HarnessEvent::ModelUsage {
                input_tokens,
                cached_input_tokens,
                cache_write_tokens,
                output_tokens,
                reasoning_tokens,
                total_tokens,
            } => format!(
                "Usage in={input_tokens} cached={cached_input_tokens} cache-write={cache_write_tokens} out={output_tokens} reasoning={reasoning_tokens} total={total_tokens}"
            ),
            HarnessEvent::PlanChanged {
                accepted,
                running,
                pending,
                blocked,
            } => format!(
                "{} Plan · {accepted} accepted · {running} running · {pending} pending · {blocked} blocked",
                ActivityIcon::Running.render(self.style.ascii)
            ),
            HarnessEvent::AgentStatus {
                agent_id,
                role,
                status,
                detail,
            } => format!(
                "{} {agent_id} {role} · {status} · {detail}",
                ActivityIcon::Running.render(self.style.ascii)
            ),
            HarnessEvent::ReasoningSummary { agent_id, summary } => format!(
                "{} {agent_id} · {summary}",
                ActivityIcon::Thinking.render(self.style.ascii)
            ),
            HarnessEvent::TextOutput { text } => format!("{PRODUCT_SHORT_NAME}: {text}"),
            HarnessEvent::ToolStatus {
                tool,
                status,
                summary,
            } => format!(
                "{} {tool} · {status} · {summary}",
                ActivityIcon::Tool.render(self.style.ascii)
            ),
            HarnessEvent::BrowserStatus {
                session_id,
                status,
                detail,
            } => format!(
                "{} Browser {session_id} · {status} · {detail}",
                ActivityIcon::Tool.render(self.style.ascii)
            ),
            HarnessEvent::McpStatus {
                server_id,
                status,
                detail,
            } => format!(
                "{} MCP {server_id} · {status} · {detail}",
                ActivityIcon::Tool.render(self.style.ascii)
            ),
            HarnessEvent::PluginStatus {
                plugin_id,
                status,
                detail,
            } => format!(
                "{} Plugin {plugin_id} · {status} · {detail}",
                ActivityIcon::Tool.render(self.style.ascii)
            ),
            HarnessEvent::SkillStatus {
                skill_id,
                status,
                detail,
            } => format!(
                "{} Skill {skill_id} · {status} · {detail}",
                ActivityIcon::Done.render(self.style.ascii)
            ),
            HarnessEvent::PermissionRequested {
                approval_id,
                invocation_id,
                action,
                risk,
                reason,
            } => {
                let commands = invocation_id.as_ref().map_or_else(String::new, |id| {
                    format!(" · /approve {id} once|run|project · /deny {id}")
                });
                format!(
                    "{} {approval_id} · {action} · risk {risk} · {reason}{commands}",
                    ActivityIcon::Permission.render(self.style.ascii)
                )
            }
            HarnessEvent::ContextChanged {
                used_tokens,
                max_tokens,
                cache_percent,
            } => format!(
                "Context {used_tokens}/{max_tokens} · cache {}",
                cache_percent.map_or_else(|| "n/a".to_owned(), |value| format!("{value}%"))
            ),
            HarnessEvent::Error {
                code,
                message,
                action,
            } => format!(
                "{} {code}: {message}{}",
                ActivityIcon::Failed.render(self.style.ascii),
                action
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" · {value}"))
            ),
            HarnessEvent::SystemShutdown { reason } => format!(
                "{} Shutdown · {reason}",
                ActivityIcon::Done.render(self.style.ascii)
            ),
        };
        self.sanitize(&rendered)
    }

    /// 清除装饰性 Unicode；用户正文中的 Unicode 不改写。
    #[must_use]
    pub fn sanitize(&self, line: &str) -> String {
        if !self.style.ascii {
            return line.to_owned();
        }
        line.replace(" · ", " | ")
            .replace('◈', "")
            .replace('❯', ">")
            .replace('│', "|")
    }
}

/// JSONL Renderer；stdout 可直接由 CI 消费。
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonRenderer;

impl JsonRenderer {
    pub fn render_event(&self, envelope: &EventEnvelope) -> Result<String, serde_json::Error> {
        serde_json::to_string(envelope)
    }
}

#[cfg(test)]
mod tests {
    use harness_event::{EventPriority, EventScope, HarnessEvent};

    use super::*;

    fn envelope(event: HarnessEvent) -> EventEnvelope {
        EventEnvelope {
            schema_version: 1,
            sequence: 1,
            recorded_at_millis: 0,
            scope: EventScope::default(),
            priority: EventPriority::Normal,
            event,
        }
    }

    #[test]
    fn ascii_renderer_never_requires_unicode_icons() {
        let renderer = PlainRenderer::new(RenderStyle {
            ascii: true,
            color: false,
        });
        let output = renderer.render_event(&envelope(HarnessEvent::AgentStatus {
            agent_id: harness_types::AgentDefinitionId::from("agent:test"),
            role: "Coder".to_owned(),
            status: "running".to_owned(),
            detail: "file.rs".to_owned(),
        }));
        assert_eq!(output, "[RUN] agent:test Coder | running | file.rs");
        assert!(!output.contains('·'));
    }

    #[test]
    fn json_renderer_preserves_schema_and_sequence() {
        let output = JsonRenderer
            .render_event(&envelope(HarnessEvent::TextOutput {
                text: "ok".to_owned(),
            }))
            .expect("JSON event");
        let value: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["sequence"], 1);
        assert_eq!(value["event"]["type"], "text-output");

        let ready = JsonRenderer
            .render_event(&envelope(HarnessEvent::SystemReady {
                project_root: "C:/project".to_owned(),
            }))
            .expect("ready JSON");
        let ready: serde_json::Value = serde_json::from_str(&ready).expect("valid ready JSON");
        assert_eq!(ready["event"]["projectRoot"], "C:/project");
        assert!(ready["event"].get("project_root").is_none());
    }
}
