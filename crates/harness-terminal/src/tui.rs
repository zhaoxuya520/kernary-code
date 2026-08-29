use std::io::{self, Stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use harness_event::{EventEnvelope, EventSubscription, HarnessEvent};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border::{self, Set as BorderSet};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    ActivityIcon, CommandRegistry, InputSuggestion, LanguagePack, PRODUCT_SHORT_NAME,
    PlainRenderer, RenderStyle, TAGLINE, UiLanguage, compact_mark,
};

const ASCII_BORDER: BorderSet = BorderSet {
    top_left: " ",
    top_right: " ",
    bottom_left: " ",
    bottom_right: " ",
    vertical_left: " ",
    vertical_right: " ",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

#[derive(Clone, Copy, Debug)]
struct ProductTheme {
    accent: Style,
    secondary: Style,
    success: Style,
    warning: Style,
    danger: Style,
    muted: Style,
    border: Style,
}

impl ProductTheme {
    fn new(color: bool) -> Self {
        if color {
            Self {
                accent: Style::default().fg(Color::Cyan),
                secondary: Style::default().fg(Color::Blue),
                success: Style::default().fg(Color::Green),
                warning: Style::default().fg(Color::Yellow),
                danger: Style::default().fg(Color::Red),
                muted: Style::default().fg(Color::DarkGray),
                border: Style::default().fg(Color::DarkGray),
            }
        } else {
            let default = Style::default();
            Self {
                accent: default,
                secondary: default,
                success: default,
                warning: default,
                danger: default,
                muted: default.add_modifier(Modifier::DIM),
                border: default.add_modifier(Modifier::DIM),
            }
        }
    }
}

fn middle_truncate(value: &str, max_characters: usize) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= max_characters {
        return value.to_owned();
    }
    if max_characters <= 3 {
        return characters.into_iter().take(max_characters).collect();
    }
    let left = (max_characters - 1) / 2;
    let right = max_characters - left - 1;
    characters[..left]
        .iter()
        .copied()
        .chain(std::iter::once('…'))
        .chain(characters[characters.len() - right..].iter().copied())
        .collect()
}

fn project_label(project: &str) -> String {
    Path::new(project)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(project)
        .to_owned()
}

fn progress_bar(percent: u8, width: usize, ascii: bool) -> String {
    let percent = usize::from(percent.min(100));
    let filled = (percent * width).div_ceil(100);
    let (on, off) = if ascii { ('#', '-') } else { ('━', '─') };
    std::iter::repeat_n(on, filled.min(width))
        .chain(std::iter::repeat_n(off, width.saturating_sub(filled)))
        .collect()
}

fn spinner_frame(elapsed: Duration, ascii: bool) -> &'static str {
    let frame = usize::try_from(elapsed.as_millis() / 120).unwrap_or(0) % 4;
    if ascii {
        ["-", "\\", "|", "/"][frame]
    } else {
        ["◒", "◐", "◓", "◑"][frame]
    }
}

fn activity_lines(
    history: &[String],
    theme: ProductTheme,
    pack: &LanguagePack,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(history.len());
    for entry in history {
        if let Some(text) = entry.strip_prefix("You: ") {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::styled(
                    pack.you_label.to_owned(),
                    theme.accent.add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(text.to_owned()),
            ]));
            continue;
        }
        if let Some(text) = entry.strip_prefix("Kernary: ") {
            lines.push(Line::from(vec![
                Span::styled("KERNARY", theme.accent.add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::raw(text.to_owned()),
            ]));
            continue;
        }
        let style = if entry.starts_with("[DONE]") || entry.starts_with('✓') {
            theme.success
        } else if entry.starts_with("[FAIL]") || entry.starts_with('✕') || entry.starts_with("! ")
        {
            theme.danger
        } else if entry.starts_with("[WARN]") {
            theme.warning
        } else if entry.starts_with("[THINK]")
            || entry.starts_with('◇')
            || entry.starts_with("[RUN]")
            || entry.starts_with('◆')
        {
            theme.secondary
        } else if entry.starts_with("Context ")
            || entry.starts_with("Usage ")
            || entry.starts_with("Model ")
        {
            theme.muted
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!("  {entry}"), style));
    }
    lines
}

fn onboarding_lines(
    pack: &LanguagePack,
    theme: ProductTheme,
    ascii: bool,
    model_configured: bool,
    vector_configured: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            compact_mark(ascii).to_owned(),
            theme.accent.add_modifier(Modifier::BOLD),
        )),
        Line::styled(TAGLINE.to_owned(), theme.muted),
        Line::default(),
        Line::from(Span::styled(
            if model_configured {
                pack.new_session_title.to_owned()
            } else {
                pack.onboarding_title.to_owned()
            },
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    if !model_configured {
        lines.extend([
            Line::from(vec![
                Span::styled("/provider add", theme.accent.add_modifier(Modifier::BOLD)),
                Span::styled("   ", theme.muted),
                Span::raw(pack.add_provider_action.to_owned()),
            ]),
            Line::from(vec![
                Span::styled("/connect", theme.accent.add_modifier(Modifier::BOLD)),
                Span::styled("        ", theme.muted),
                Span::raw(pack.connect_provider_action.to_owned()),
            ]),
        ]);
    }
    if !vector_configured {
        lines.push(Line::from(vec![
            Span::styled("/vector setup", theme.accent.add_modifier(Modifier::BOLD)),
            Span::styled("   ", theme.muted),
            Span::raw(pack.vector_setup_action.to_owned()),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("/agentmd status", theme.accent.add_modifier(Modifier::BOLD)),
        Span::styled("  ", theme.muted),
        Span::raw(pack.agent_md_action.to_owned()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("/", theme.accent.add_modifier(Modifier::BOLD)),
        Span::styled("               ", theme.muted),
        Span::raw(pack.browse_commands_action.to_owned()),
    ]));
    lines
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

/// TUI 顶部/状态栏需要的只读快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub session_title: String,
    pub model: String,
    pub model_configured: bool,
    pub language: UiLanguage,
    pub mode: String,
    pub permission_mode: String,
    pub reasoning: String,
    pub context_percent: u8,
    pub cache_percent: Option<u8>,
    pub agents: usize,
    pub vector_configured: bool,
    pub project: String,
    pub branch: Option<String>,
    pub statusbar_visible: bool,
}

/// Backend 处理一行输入后的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendResponse {
    pub lines: Vec<String>,
    pub should_exit: bool,
    pub clear_view: bool,
    pub secret_prompt: Option<SecretPrompt>,
    pub input_prompt: Option<InputPrompt>,
}

/// 独立 secure input lane；request ID 和提示均不包含 secret。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretPrompt {
    pub request_id: String,
    pub prompt: String,
}

/// 普通文本向导输入；与任务输入隔离，不进入命令历史或 Agent Context。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputPrompt {
    pub request_id: String,
    pub prompt: String,
    pub placeholder: Option<String>,
}

/// Terminal 只依赖该 Application interface。
pub trait TerminalBackend {
    fn handle_input(&mut self, input: &str) -> BackendResponse;
    fn snapshot(&self) -> TerminalSnapshot;
    fn cancel_current(&mut self) -> BackendResponse;
    fn submit_secret(&mut self, request_id: &str, secret: String) -> BackendResponse;
    fn submit_input_prompt(&mut self, _request_id: &str, _value: String) -> BackendResponse {
        BackendResponse::default()
    }
    fn complete_input(&self, _input: &str) -> Vec<InputSuggestion> {
        Vec::new()
    }
    fn poll(&mut self) -> BackendResponse {
        BackendResponse::default()
    }
    fn initial_history(&self) -> Vec<String> {
        Vec::new()
    }
    fn cycle_permission_mode(&mut self) -> BackendResponse {
        BackendResponse::default()
    }
}

/// Unicode-safe 单行编辑器；cursor 使用字符边界而不是字节偏移。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LineEditor {
    text: String,
    cursor: usize,
}

impl LineEditor {
    fn text(&self) -> &str {
        &self.text
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn byte_index(&self, character_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(character_index)
            .map_or(self.text.len(), |(index, _)| index)
    }

    fn insert_char(&mut self, character: char) {
        let byte_index = self.byte_index(self.cursor);
        self.text.insert(byte_index, character);
        self.cursor += 1;
    }

    fn insert_paste(&mut self, pasted: &str) {
        for character in pasted.chars() {
            match character {
                '\r' | '\n' | '\t' => {
                    if !self.text[..self.byte_index(self.cursor)].ends_with(' ') {
                        self.insert_char(' ');
                    }
                }
                character if !character.is_control() => self.insert_char(character),
                _ => {}
            }
        }
    }

    fn delete_character_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(end);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_character_range(self.cursor - 1, self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            self.delete_character_range(self.cursor, self.cursor + 1);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.len();
    }

    fn previous_word_boundary(&self) -> usize {
        let characters = self.text.chars().collect::<Vec<_>>();
        let mut index = self.cursor;
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !characters[index - 1].is_whitespace() {
            index -= 1;
        }
        index
    }

    fn next_word_boundary(&self) -> usize {
        let characters = self.text.chars().collect::<Vec<_>>();
        let mut index = self.cursor;
        while index < characters.len() && !characters[index].is_whitespace() {
            index += 1;
        }
        while index < characters.len() && characters[index].is_whitespace() {
            index += 1;
        }
        index
    }

    fn move_word_left(&mut self) {
        self.cursor = self.previous_word_boundary();
    }

    fn move_word_right(&mut self) {
        self.cursor = self.next_word_boundary();
    }

    fn delete_word_backward(&mut self) {
        let start = self.previous_word_boundary();
        self.delete_character_range(start, self.cursor);
    }

    fn delete_to_start(&mut self) {
        self.delete_character_range(0, self.cursor);
    }

    fn delete_to_end(&mut self) {
        let end = self.len();
        self.delete_character_range(self.cursor, end);
    }

    /// 生成始终包含 cursor 的单行 viewport，并返回 cursor 的显示列。
    fn visible_window(&self, max_width: usize, masked: bool) -> (String, usize) {
        if max_width == 0 {
            return (String::new(), 0);
        }
        let characters = if masked {
            vec!['*'; self.len()]
        } else {
            self.text.chars().collect::<Vec<_>>()
        };
        let cursor = self.cursor.min(characters.len());
        let width_of = |character: char| UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        let mut start = cursor;
        let mut before_width = 0usize;
        while start > 0 {
            let character_width = width_of(characters[start - 1]);
            let left_marker = usize::from(start > 1);
            if before_width + character_width + left_marker > max_width {
                break;
            }
            start -= 1;
            before_width += character_width;
        }
        let left_hidden = start > 0;
        let mut visible = String::new();
        if left_hidden {
            visible.push('…');
        }
        for character in &characters[start..cursor] {
            visible.push(*character);
        }
        let cursor_column = UnicodeWidthStr::width(visible.as_str());
        let mut used = cursor_column;
        let mut end = cursor;
        while end < characters.len() {
            let character_width = width_of(characters[end]);
            let right_marker = usize::from(end + 1 < characters.len());
            if used + character_width + right_marker > max_width {
                break;
            }
            visible.push(characters[end]);
            used += character_width;
            end += 1;
        }
        if end < characters.len() && used < max_width {
            visible.push('…');
        }
        (visible, cursor_column.min(max_width.saturating_sub(1)))
    }
}

fn suggestion_window(total: usize, selected: usize, capacity: usize) -> (usize, usize) {
    if total == 0 || capacity == 0 {
        return (0, 0);
    }
    let selected = selected.min(total - 1);
    let start = selected
        .saturating_sub(capacity / 2)
        .min(total.saturating_sub(capacity));
    (start, (start + capacity).min(total))
}

fn reset_edit_navigation(
    history_cursor: &mut Option<usize>,
    history_draft: &mut Option<LineEditor>,
    suggestion_cursor: &mut usize,
    suggestions_dismissed: &mut bool,
) {
    *history_cursor = None;
    *history_draft = None;
    *suggestion_cursor = 0;
    *suggestions_dismissed = false;
}

fn push_activity(history: &mut Vec<String>, line: String) {
    if line.contains('\r') || line.contains('\n') {
        for part in line.replace("\r\n", "\n").replace('\r', "\n").lines() {
            push_activity(history, part.to_owned());
        }
        return;
    }
    if history.last() == Some(&line) {
        return;
    }
    let replaceable =
        line.starts_with("Context ") || line.contains(" Plan ·") || line.contains(" Plan |");
    if replaceable
        && let Some(index) = history.iter().rposition(|candidate| {
            (line.starts_with("Context ") && candidate.starts_with("Context "))
                || ((line.contains(" Plan ·") || line.contains(" Plan |"))
                    && (candidate.contains(" Plan ·") || candidate.contains(" Plan |")))
        })
        && history.len().saturating_sub(index) <= 12
    {
        history[index] = line;
        return;
    }
    history.push(line);
}

fn event_is_transcript_worthy(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::GoalChanged { .. }
            | HarnessEvent::AgentStatus { .. }
            | HarnessEvent::ReasoningSummary { .. }
            | HarnessEvent::TextOutput { .. }
            | HarnessEvent::ToolStatus { .. }
            | HarnessEvent::BrowserStatus { .. }
            | HarnessEvent::McpStatus { .. }
            | HarnessEvent::PluginStatus { .. }
            | HarnessEvent::SkillStatus { .. }
            | HarnessEvent::PermissionRequested { .. }
            | HarnessEvent::Error { .. }
    )
}

fn render_transcript_event(
    envelope: &EventEnvelope,
    renderer: PlainRenderer,
    ascii: bool,
) -> String {
    let icon = |kind: ActivityIcon| kind.render(ascii);
    let rendered = match &envelope.event {
        HarnessEvent::AgentStatus {
            role,
            status,
            detail,
            ..
        } => {
            let kind = if status.contains("fail") || status.contains("block") {
                ActivityIcon::Failed
            } else if status.contains("complete") || status.contains("sleep") {
                ActivityIcon::Done
            } else {
                ActivityIcon::Running
            };
            format!("{} {role} · {detail}", icon(kind))
        }
        HarnessEvent::ReasoningSummary { summary, .. } => {
            format!("{} {summary}", icon(ActivityIcon::Thinking))
        }
        HarnessEvent::TextOutput { text } => format!("{PRODUCT_SHORT_NAME}: {text}"),
        HarnessEvent::ToolStatus {
            tool,
            status,
            summary,
        } => {
            let kind = if status.contains("fail") {
                ActivityIcon::Failed
            } else if status.contains("complete") || status.contains("success") {
                ActivityIcon::Done
            } else {
                ActivityIcon::Tool
            };
            format!("{} {tool} · {summary}", icon(kind))
        }
        _ => renderer.render_event(envelope),
    };
    renderer.sanitize(&rendered)
}

/// Ctrl+C 的纯状态机结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelAction {
    ClearInput,
    CancelCurrent,
    ArmExit,
    Exit,
}

/// 第一次取消、第二次退出。
#[derive(Clone, Debug, Default)]
pub struct CancelController {
    armed_at_millis: Option<u64>,
}

impl CancelController {
    #[must_use]
    pub fn on_ctrl_c(
        &mut self,
        now_millis: u64,
        input_non_empty: bool,
        active_work: bool,
    ) -> CancelAction {
        if input_non_empty {
            self.armed_at_millis = None;
            return CancelAction::ClearInput;
        }
        if active_work {
            self.armed_at_millis = Some(now_millis);
            return CancelAction::CancelCurrent;
        }
        if self
            .armed_at_millis
            .is_some_and(|armed| now_millis.saturating_sub(armed) <= 2_000)
        {
            self.armed_at_millis = None;
            return CancelAction::Exit;
        }
        self.armed_at_millis = Some(now_millis);
        CancelAction::ArmExit
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TuiOptions {
    pub ascii: bool,
    pub color: bool,
}

struct TuiView<'a> {
    snapshot: &'a TerminalSnapshot,
    pack: &'a LanguagePack,
    history: &'a [String],
    input: &'a LineEditor,
    secret_prompt: Option<&'a SecretPrompt>,
    secret_input: &'a LineEditor,
    input_prompt: Option<&'a InputPrompt>,
    suggestions: &'a [InputSuggestion],
    suggestion_cursor: usize,
    transcript_scroll: usize,
    show_onboarding: bool,
    elapsed: Duration,
    options: TuiOptions,
}

fn render_product_tui(frame: &mut Frame<'_>, view: TuiView<'_>) -> usize {
    let area = frame.area();
    let theme = ProductTheme::new(view.options.color);
    let border_set = if view.options.ascii {
        ASCII_BORDER
    } else {
        border::ROUNDED
    };
    let status_height = u16::from(view.snapshot.statusbar_visible && area.height >= 9);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(status_height),
        ])
        .split(area);

    let project = middle_truncate(&project_label(&view.snapshot.project), 32);
    let mut identity = vec![
        Span::styled(
            compact_mark(view.options.ascii).to_owned(),
            theme.accent.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(project, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("  /  ", theme.muted),
        Span::styled(
            middle_truncate(&view.snapshot.session_title, 32),
            theme.muted,
        ),
    ];
    if let Some(branch) = &view.snapshot.branch {
        identity.extend([
            Span::styled("  /  ", theme.muted),
            Span::styled(middle_truncate(branch, 24), theme.muted),
        ]);
    }

    let is_working = view.snapshot.agents > 0;
    let status_mark = if is_working {
        spinner_frame(view.elapsed, view.options.ascii)
    } else if view.options.ascii {
        "*"
    } else {
        "●"
    };
    let status_style = if is_working {
        theme.accent
    } else {
        theme.success
    };
    let model_width = if area.width >= 110 {
        48
    } else if area.width >= 78 {
        34
    } else {
        22
    };
    let mut runtime = vec![
        Span::styled(format!("{status_mark} "), status_style),
        Span::styled(
            if is_working {
                view.pack.working_label
            } else {
                view.pack.ready_label
            },
            status_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", theme.muted),
        Span::styled(format!("{} ", view.pack.model_label), theme.muted),
        Span::raw(middle_truncate(&view.snapshot.model, model_width)),
    ];
    if area.width >= 78 {
        runtime.extend([
            Span::styled("  ·  ", theme.muted),
            Span::styled(format!("{} ", view.pack.mode_label), theme.muted),
            Span::raw(view.snapshot.mode.clone()),
            Span::styled("  ·  ", theme.muted),
            Span::styled(format!("{} ", view.pack.permission_label), theme.muted),
            Span::raw(view.snapshot.permission_mode.clone()),
        ]);
    }
    if area.width >= 108 {
        runtime.extend([
            Span::styled("  ·  ", theme.muted),
            Span::styled(format!("{} ", view.pack.reasoning_label), theme.muted),
            Span::raw(view.snapshot.reasoning.clone()),
        ]);
    }
    if area.width >= 118 {
        runtime.extend([
            Span::styled("  ·  ", theme.muted),
            Span::styled(format!("{} ", view.pack.vector_label), theme.muted),
            Span::styled(
                if view.snapshot.vector_configured {
                    view.pack.vector_configured_label
                } else {
                    view.pack.vector_unconfigured_label
                },
                if view.snapshot.vector_configured {
                    theme.success
                } else {
                    theme.muted
                },
            ),
        ]);
    }
    let bar_width = if area.width >= 90 { 10 } else { 6 };
    runtime.extend([
        Span::styled("  ·  ", theme.muted),
        Span::styled(format!("{} ", view.pack.context_label), theme.muted),
        Span::styled(
            progress_bar(view.snapshot.context_percent, bar_width, view.options.ascii),
            if view.snapshot.context_percent >= 85 {
                theme.warning
            } else {
                theme.accent
            },
        ),
        Span::styled(
            format!(" {:>3}%", view.snapshot.context_percent),
            theme.muted,
        ),
    ]);
    if is_working && area.width >= 72 {
        runtime.extend([
            Span::styled("  ·  ", theme.muted),
            Span::styled(
                format!("{} {}", view.pack.agents_label, view.snapshot.agents),
                theme.secondary,
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(vec![Line::from(identity), Line::from(runtime)]).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_set(border_set)
                .border_style(theme.border)
                .padding(Padding::horizontal(1)),
        ),
        chunks[0],
    );

    let transcript_lines = if view.show_onboarding {
        onboarding_lines(
            view.pack,
            theme,
            view.options.ascii,
            view.snapshot.model_configured,
            view.snapshot.vector_configured,
        )
    } else {
        activity_lines(view.history, theme, view.pack)
    };
    let transcript_padding: u16 = if area.width >= 72 { 3 } else { 1 };
    let text_width = chunks[1]
        .width
        .saturating_sub(transcript_padding.saturating_mul(2));
    let total_height = wrapped_line_count(&transcript_lines, text_width);
    let transcript = Paragraph::new(Text::from(transcript_lines))
        .wrap(Wrap { trim: false })
        .block(Block::default().padding(Padding::new(
            transcript_padding,
            transcript_padding,
            1,
            0,
        )));
    let visible_height = usize::from(chunks[1].height.saturating_sub(1));
    let max_scroll = total_height.saturating_sub(visible_height);
    let transcript_scroll = view.transcript_scroll.min(max_scroll);
    let top = max_scroll.saturating_sub(transcript_scroll);
    frame.render_widget(
        transcript.scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        chunks[1],
    );
    if transcript_scroll > 0 && chunks[1].width >= 24 {
        let indicator = format!("↑ {transcript_scroll} · {}", view.pack.scroll_hint);
        let indicator_area = Rect::new(
            chunks[1].x,
            chunks[1].y,
            chunks[1].width.saturating_sub(1),
            1,
        );
        frame.render_widget(
            Paragraph::new(indicator)
                .alignment(Alignment::Right)
                .style(theme.muted),
            indicator_area,
        );
    }

    let prefix = if view.options.ascii { "> " } else { "❯ " };
    let prefix_width = UnicodeWidthStr::width(prefix);
    let available_width =
        usize::from(chunks[2].width.saturating_sub(2)).saturating_sub(prefix_width);
    let (visible_input, cursor_column, input_title, placeholder, composer_style) =
        view.secret_prompt.map_or_else(
            || {
                let (visible, cursor) = view.input.visible_window(available_width, false);
                if let Some(prompt) = view.input_prompt {
                    (
                        visible,
                        cursor,
                        format!("{} · {}", view.pack.setup, prompt.prompt),
                        prompt.placeholder.clone(),
                        theme.secondary,
                    )
                } else {
                    (
                        visible,
                        cursor,
                        view.pack.input.to_owned(),
                        Some(view.pack.composer_placeholder.to_owned()),
                        theme.accent,
                    )
                }
            },
            |prompt| {
                let (visible, cursor) = view.secret_input.visible_window(available_width, true);
                (
                    visible,
                    cursor,
                    format!("{} · {}", view.pack.secure_label, prompt.prompt),
                    None,
                    theme.warning,
                )
            },
        );
    let title_width = usize::from(chunks[2].width.saturating_sub(8));
    let input_is_empty = visible_input.is_empty();
    let mut composer_spans = vec![Span::styled(
        prefix,
        composer_style.add_modifier(Modifier::BOLD),
    )];
    if input_is_empty {
        if let Some(placeholder) = placeholder {
            composer_spans.push(Span::styled(
                middle_truncate(&placeholder, available_width),
                theme.muted,
            ));
        }
    } else {
        composer_spans.push(Span::raw(visible_input));
    }
    frame.render_widget(
        Paragraph::new(Line::from(composer_spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(border_set)
                    .border_style(composer_style)
                    .title(Line::from(Span::styled(
                        middle_truncate(&input_title, title_width),
                        composer_style.add_modifier(Modifier::BOLD),
                    ))),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );

    if status_height > 0 {
        let separator = if view.options.ascii { " | " } else { "  ·  " };
        let mut footer = view.pack.send_hint.to_owned();
        if area.width >= 92 {
            footer.push_str(separator);
            footer.push_str(view.pack.scroll_hint);
            footer.push_str(separator);
            footer.push_str(view.pack.permission_cycle_hint);
            if let Some(cache) = view.snapshot.cache_percent {
                footer.push_str(separator);
                footer.push_str(view.pack.cache_label);
                footer.push(' ');
                footer.push_str(&format!("{cache}%"));
            }
        }
        frame.render_widget(
            Paragraph::new(footer)
                .style(theme.muted)
                .block(Block::default().padding(Padding::horizontal(1))),
            chunks[3],
        );
    }

    let suggestion_capacity = view
        .suggestions
        .len()
        .min(if area.height >= 22 { 8 } else { 5 });
    if suggestion_capacity > 0 {
        let popup_height = u16::try_from(suggestion_capacity + 2).unwrap_or(10);
        let margin = u16::from(area.width >= 44) * 2;
        let popup_y = chunks[2].y.saturating_sub(popup_height).max(chunks[1].y);
        let popup = Rect::new(
            area.x.saturating_add(margin),
            popup_y,
            area.width.saturating_sub(margin.saturating_mul(2)),
            popup_height.min(chunks[2].y.saturating_sub(chunks[1].y)),
        );
        if popup.height >= 3 {
            let (window_start, window_end) = suggestion_window(
                view.suggestions.len(),
                view.suggestion_cursor,
                usize::from(popup.height.saturating_sub(2)),
            );
            let items = view.suggestions[window_start..window_end]
                .iter()
                .enumerate()
                .map(|(offset, suggestion)| {
                    let selected = window_start + offset == view.suggestion_cursor;
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            suggestion.label.clone(),
                            if selected {
                                theme.accent.add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().add_modifier(Modifier::BOLD)
                            },
                        ),
                        Span::styled(format!("  {}", suggestion.description), theme.muted),
                    ]))
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default();
            state.select(Some(view.suggestion_cursor.saturating_sub(window_start)));
            let title = format!(
                "{}  {}/{}  ·  {}",
                view.pack.command_palette,
                view.suggestion_cursor + 1,
                view.suggestions.len(),
                view.pack.command_hint,
            );
            frame.render_widget(Clear, popup);
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_set(border_set)
                            .border_style(theme.secondary)
                            .title(Line::from(Span::styled(
                                middle_truncate(&title, usize::from(popup.width.saturating_sub(4))),
                                theme.secondary.add_modifier(Modifier::BOLD),
                            ))),
                    )
                    .highlight_symbol(if view.options.ascii { "> " } else { "› " }),
                popup,
                &mut state,
            );
        }
    }

    let cursor_x = chunks[2]
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(prefix_width).unwrap_or(u16::MAX))
        .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX))
        .min(chunks[2].right().saturating_sub(2));
    frame.set_cursor_position((cursor_x, chunks[2].y.saturating_add(1)));
    transcript_scroll
}

/// Ratatui 交互循环：Unicode 行编辑、历史、Bracketed Paste 与可滚动 Slash 面板。
pub fn run_tui<B: TerminalBackend>(
    backend: &mut B,
    subscription: &EventSubscription,
    _registry: CommandRegistry,
    options: TuiOptions,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let renderer = PlainRenderer::new(RenderStyle {
        ascii: options.ascii,
        color: options.color,
    });
    let mut history = backend.initial_history();
    let mut input = LineEditor::default();
    let mut command_history = Vec::<String>::new();
    let mut history_cursor: Option<usize> = None;
    let mut history_draft: Option<LineEditor> = None;
    let mut suggestion_cursor = 0usize;
    let mut suggestions_dismissed = false;
    let started = Instant::now();
    let mut cancel = CancelController::default();
    let mut secret_prompt: Option<SecretPrompt> = None;
    let mut secret_input = LineEditor::default();
    let mut input_prompt: Option<InputPrompt> = None;

    let mut show_onboarding = history.is_empty();
    let mut transcript_scroll = 0usize;

    loop {
        while let Ok(envelope) = subscription.try_recv() {
            if event_is_transcript_worthy(&envelope.event) {
                push_activity(
                    &mut history,
                    render_transcript_event(&envelope, renderer, options.ascii),
                );
            }
        }
        let background = backend.poll();
        if let Some(prompt) = background.secret_prompt.clone() {
            secret_prompt = Some(prompt);
            secret_input.clear();
        }
        if let Some(prompt) = background.input_prompt.clone() {
            input_prompt = Some(prompt);
            input.clear();
        }
        if background.clear_view {
            history.clear();
        }
        for line in background.lines {
            push_activity(&mut history, renderer.sanitize(&line));
        }
        if background.should_exit {
            break;
        }
        if history.len() > 1_000 {
            history.drain(..history.len() - 1_000);
        }

        let snapshot = backend.snapshot();
        let pack = snapshot.language.pack();
        let registry = CommandRegistry::with_language(snapshot.language);
        let mut suggestions = if secret_prompt.is_none() && !suggestions_dismissed {
            let dynamic = backend.complete_input(input.text());
            if !dynamic.is_empty() {
                dynamic
            } else if input_prompt.is_none() && input.text().starts_with('/') {
                registry.suggestions(input.text())
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        suggestions.sort_by(|left, right| left.label.cmp(&right.label));
        suggestions.dedup_by(|left, right| left.replacement == right.replacement);
        if suggestions.is_empty() {
            suggestion_cursor = 0;
        } else {
            suggestion_cursor = suggestion_cursor.min(suggestions.len() - 1);
        }
        let mut rendered_scroll = transcript_scroll;
        terminal.draw(|frame| {
            rendered_scroll = render_product_tui(
                frame,
                TuiView {
                    snapshot: &snapshot,
                    pack,
                    history: &history,
                    input: &input,
                    secret_prompt: secret_prompt.as_ref(),
                    secret_input: &secret_input,
                    input_prompt: input_prompt.as_ref(),
                    suggestions: &suggestions,
                    suggestion_cursor,
                    transcript_scroll,
                    show_onboarding,
                    elapsed: started.elapsed(),
                    options,
                },
            );
        })?;
        transcript_scroll = rendered_scroll;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let key = match event::read()? {
            Event::Paste(pasted) => {
                if secret_prompt.is_some() {
                    secret_input.insert_paste(&pasted);
                } else {
                    input.insert_paste(&pasted);
                    reset_edit_navigation(
                        &mut history_cursor,
                        &mut history_draft,
                        &mut suggestion_cursor,
                        &mut suggestions_dismissed,
                    );
                }
                continue;
            }
            Event::Key(key) => key,
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(prompt) = secret_prompt.clone() {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let response = backend.submit_secret(&prompt.request_id, String::new());
                    history.extend(
                        response
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                    secret_prompt = None;
                    input_prompt = response.input_prompt;
                    secret_input.clear();
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.move_home();
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.move_end();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.delete_to_start();
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.delete_to_end();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.delete_word_backward();
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.insert_char(character);
                }
                KeyCode::Backspace => secret_input.backspace(),
                KeyCode::Delete => secret_input.delete(),
                KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.move_word_left();
                }
                KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    secret_input.move_word_right();
                }
                KeyCode::Left => secret_input.move_left(),
                KeyCode::Right => secret_input.move_right(),
                KeyCode::Home => secret_input.move_home(),
                KeyCode::End => secret_input.move_end(),
                KeyCode::Esc => {
                    let response = backend.submit_secret(&prompt.request_id, String::new());
                    history.extend(
                        response
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                    secret_prompt = None;
                    input_prompt = response.input_prompt;
                    secret_input.clear();
                }
                KeyCode::Enter => {
                    let secret = secret_input.take();
                    let response = backend.submit_secret(&prompt.request_id, secret);
                    history.extend(
                        response
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                    secret_prompt = response.secret_prompt;
                    input_prompt = response.input_prompt;
                }
                _ => {}
            }
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(prompt) = input_prompt.take() {
                let response = backend.submit_input_prompt(&prompt.request_id, String::new());
                history.extend(
                    response
                        .lines
                        .into_iter()
                        .map(|line| renderer.sanitize(&line)),
                );
                input.clear();
                input_prompt = response.input_prompt;
                secret_prompt = response.secret_prompt;
                continue;
            }
            let now = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match cancel.on_ctrl_c(now, !input.is_empty(), snapshot.agents > 0) {
                CancelAction::ClearInput => input.clear(),
                CancelAction::CancelCurrent => {
                    history.extend(
                        backend
                            .cancel_current()
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                }
                CancelAction::ArmExit => {
                    push_activity(&mut history, pack.exit_hint.to_owned());
                }
                CancelAction::Exit => {
                    history.extend(
                        backend
                            .handle_input("/exit")
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                    break;
                }
            }
            continue;
        }
        match key.code {
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_home();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_end();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_to_start();
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_to_end();
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete_word_backward();
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.insert_char(character);
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Backspace => {
                input.backspace();
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Delete => {
                input.delete();
                reset_edit_navigation(
                    &mut history_cursor,
                    &mut history_draft,
                    &mut suggestion_cursor,
                    &mut suggestions_dismissed,
                );
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_word_left();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_word_right();
            }
            KeyCode::Left => input.move_left(),
            KeyCode::Right => input.move_right(),
            KeyCode::Home => input.move_home(),
            KeyCode::End => input.move_end(),
            KeyCode::Esc if input_prompt.is_some() => {
                let prompt = input_prompt.take().expect("checked");
                let response = backend.submit_input_prompt(&prompt.request_id, String::new());
                history.extend(
                    response
                        .lines
                        .into_iter()
                        .map(|line| renderer.sanitize(&line)),
                );
                input.clear();
                input_prompt = response.input_prompt;
                secret_prompt = response.secret_prompt;
            }
            KeyCode::Esc if !suggestions.is_empty() => {
                suggestions_dismissed = true;
            }
            KeyCode::Esc => {
                input.clear();
                history_cursor = None;
                history_draft = None;
            }
            KeyCode::Tab => {
                if let Some(suggestion) = suggestions.get(suggestion_cursor) {
                    input.set_text(suggestion.replacement.clone());
                    suggestion_cursor = 0;
                    suggestions_dismissed = false;
                }
            }
            KeyCode::BackTab if !suggestions.is_empty() => {
                suggestion_cursor = suggestion_cursor
                    .checked_sub(1)
                    .unwrap_or(suggestions.len() - 1);
            }
            KeyCode::BackTab => {
                let response = backend.cycle_permission_mode();
                for line in response.lines {
                    push_activity(&mut history, renderer.sanitize(&line));
                }
            }
            KeyCode::Up => {
                if !suggestions.is_empty() {
                    suggestion_cursor = suggestion_cursor
                        .checked_sub(1)
                        .unwrap_or(suggestions.len() - 1);
                } else if !command_history.is_empty() {
                    if history_cursor.is_none() {
                        history_draft = Some(input.clone());
                    }
                    let next = history_cursor
                        .map_or(command_history.len() - 1, |index| index.saturating_sub(1));
                    history_cursor = Some(next);
                    input.set_text(command_history[next].clone());
                }
            }
            KeyCode::Down => {
                if !suggestions.is_empty() {
                    suggestion_cursor = (suggestion_cursor + 1) % suggestions.len();
                } else if let Some(index) = history_cursor {
                    if index + 1 < command_history.len() {
                        history_cursor = Some(index + 1);
                        input.set_text(command_history[index + 1].clone());
                    } else {
                        history_cursor = None;
                        input = history_draft.take().unwrap_or_default();
                    }
                }
            }
            KeyCode::PageUp if !suggestions.is_empty() => {
                suggestion_cursor = suggestion_cursor.saturating_sub(8);
            }
            KeyCode::PageDown if !suggestions.is_empty() => {
                suggestion_cursor = (suggestion_cursor + 8).min(suggestions.len() - 1);
            }
            KeyCode::PageUp => {
                transcript_scroll = transcript_scroll.saturating_add(8);
            }
            KeyCode::PageDown => {
                transcript_scroll = transcript_scroll.saturating_sub(8);
            }
            KeyCode::Enter => {
                if let Some(suggestion) = suggestions.get(suggestion_cursor)
                    && input.text() != suggestion.replacement
                {
                    input.set_text(suggestion.replacement.clone());
                    suggestion_cursor = 0;
                    suggestions_dismissed = false;
                    continue;
                }
                let submitted = input.text().trim().to_owned();
                input.clear();
                history_cursor = None;
                history_draft = None;
                suggestion_cursor = 0;
                suggestions_dismissed = false;
                if submitted.is_empty() {
                    if let Some(prompt) = input_prompt.take() {
                        let response =
                            backend.submit_input_prompt(&prompt.request_id, String::new());
                        history.extend(
                            response
                                .lines
                                .into_iter()
                                .map(|line| renderer.sanitize(&line)),
                        );
                        input_prompt = response.input_prompt;
                        secret_prompt = response.secret_prompt;
                    }
                    continue;
                }
                show_onboarding = false;
                transcript_scroll = 0;
                if let Some(prompt) = input_prompt.take() {
                    let response = backend.submit_input_prompt(&prompt.request_id, submitted);
                    history.extend(
                        response
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                    input_prompt = response.input_prompt;
                    secret_prompt = response.secret_prompt;
                    secret_input.clear();
                    continue;
                }
                command_history.push(submitted.clone());
                history.push(format!("You: {submitted}"));
                let response = backend.handle_input(&submitted);
                if response.clear_view {
                    history.clear();
                }
                for line in response.lines {
                    push_activity(&mut history, renderer.sanitize(&line));
                }
                if response.should_exit {
                    break;
                }
                secret_prompt = response.secret_prompt;
                input_prompt = response.input_prompt;
                secret_input.clear();
            }
            _ => {}
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use harness_event::{EventPriority, EventScope};
    use ratatui::backend::TestBackend;

    use super::*;

    fn snapshot(model_configured: bool) -> TerminalSnapshot {
        TerminalSnapshot {
            session_title: "Product session".to_owned(),
            model: if model_configured {
                "openai/gpt-product".to_owned()
            } else {
                "未配置".to_owned()
            },
            model_configured,
            language: UiLanguage::ZhCn,
            mode: "balanced".to_owned(),
            permission_mode: "manual".to_owned(),
            reasoning: "medium".to_owned(),
            context_percent: 42,
            cache_percent: Some(75),
            agents: usize::from(model_configured),
            vector_configured: false,
            project: "C:/workspace/kernary-demo".to_owned(),
            branch: Some("feature/product-ui".to_owned()),
            statusbar_visible: true,
        }
    }

    fn screen_text(backend: &TestBackend) -> String {
        let width = usize::from(backend.buffer().area.width);
        backend
            .buffer()
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn ctrl_c_clears_input_then_arms_and_exits() {
        let mut controller = CancelController::default();
        assert_eq!(
            controller.on_ctrl_c(0, true, false),
            CancelAction::ClearInput
        );
        assert_eq!(
            controller.on_ctrl_c(10, false, false),
            CancelAction::ArmExit
        );
        assert_eq!(
            controller.on_ctrl_c(1_000, false, false),
            CancelAction::Exit
        );
    }

    #[test]
    fn active_work_gets_cancel_before_exit() {
        let mut controller = CancelController::default();
        assert_eq!(
            controller.on_ctrl_c(0, false, true),
            CancelAction::CancelCurrent
        );
    }

    #[test]
    fn line_editor_inserts_and_deletes_at_unicode_cursor() {
        let mut editor = LineEditor::default();
        editor.set_text("你ab");
        editor.move_left();
        editor.insert_char('好');
        assert_eq!(editor.text(), "你a好b");
        assert_eq!(editor.cursor, 3);
        editor.backspace();
        assert_eq!(editor.text(), "你ab");
        assert_eq!(editor.cursor, 2);
        editor.move_home();
        editor.delete();
        assert_eq!(editor.text(), "ab");
    }

    #[test]
    fn line_editor_supports_shell_word_and_kill_actions() {
        let mut editor = LineEditor::default();
        editor.set_text("/model openai/gpt-test");
        editor.move_word_left();
        assert_eq!(editor.cursor, 7);
        editor.delete_to_end();
        assert_eq!(editor.text(), "/model ");
        editor.set_text("hello world");
        editor.delete_word_backward();
        assert_eq!(editor.text(), "hello ");
        editor.delete_to_start();
        assert!(editor.is_empty());
    }

    #[test]
    fn paste_is_single_line_and_cursor_view_handles_wide_text() {
        let mut editor = LineEditor::default();
        editor.insert_paste("你\n好\tworld");
        assert_eq!(editor.text(), "你 好 world");
        let (visible, cursor) = editor.visible_window(8, false);
        assert!(UnicodeWidthStr::width(visible.as_str()) <= 8);
        assert!(cursor < 8);
    }

    #[test]
    fn suggestion_window_tracks_selection_across_long_catalog() {
        assert_eq!(suggestion_window(60, 0, 8), (0, 8));
        assert_eq!(suggestion_window(60, 30, 8), (26, 34));
        assert_eq!(suggestion_window(60, 59, 8), (52, 60));
    }

    #[test]
    fn product_helpers_are_width_safe() {
        assert_eq!(middle_truncate("abcdefghij", 7), "abc…hij");
        assert_eq!(middle_truncate("你好世界", 3), "你好世");
        assert_eq!(progress_bar(0, 6, true), "------");
        assert_eq!(progress_bar(42, 6, true), "###---");
        assert_eq!(progress_bar(100, 6, false), "━━━━━━");
    }

    #[test]
    fn transcript_hides_telemetry_and_simplifies_agent_activity() {
        assert!(!event_is_transcript_worthy(&HarnessEvent::ModelUsage {
            input_tokens: 1,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
            total_tokens: 2,
        }));
        let envelope = EventEnvelope {
            schema_version: 1,
            sequence: 1,
            recorded_at_millis: 0,
            scope: EventScope::default(),
            priority: EventPriority::Normal,
            event: HarnessEvent::AgentStatus {
                agent_id: harness_types::AgentDefinitionId::from("agent:coder"),
                role: "Coder".to_owned(),
                status: "running".to_owned(),
                detail: "task:main".to_owned(),
            },
        };
        let rendered = render_transcript_event(
            &envelope,
            PlainRenderer::new(RenderStyle {
                ascii: false,
                color: true,
            }),
            false,
        );
        assert_eq!(rendered, "◆ Coder · task:main");
        assert!(!rendered.contains("agent:coder"));

        let mut history = Vec::new();
        push_activity(&mut history, "Kernary: first\nsecond".to_owned());
        assert_eq!(history, ["Kernary: first", "second"]);
    }

    #[test]
    fn product_tui_renders_onboarding_without_debug_dashboard_boxes() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let snapshot = snapshot(false);
        let input = LineEditor::default();
        let secret_input = LineEditor::default();
        terminal
            .draw(|frame| {
                render_product_tui(
                    frame,
                    TuiView {
                        snapshot: &snapshot,
                        pack: snapshot.language.pack(),
                        history: &["[DONE] Ready".to_owned()],
                        input: &input,
                        secret_prompt: None,
                        secret_input: &secret_input,
                        input_prompt: None,
                        suggestions: &[],
                        suggestion_cursor: 0,
                        transcript_scroll: 0,
                        show_onboarding: true,
                        elapsed: Duration::ZERO,
                        options: TuiOptions {
                            ascii: false,
                            color: true,
                        },
                    },
                );
            })
            .expect("render product TUI");
        let screen = screen_text(terminal.backend());
        let compact = screen.replace(' ', "");
        assert!(screen.contains("Kernary"), "screen={screen}");
        assert!(compact.contains("尚未配置模型"), "screen={screen}");
        assert!(compact.contains("向Kernary提问"), "screen={screen}");
        assert!(!screen.contains("┌Activity"), "screen={screen}");
    }

    #[test]
    fn product_tui_keeps_command_palette_usable_on_narrow_terminal() {
        let backend = TestBackend::new(52, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let snapshot = snapshot(true);
        let mut input = LineEditor::default();
        input.set_text("/pro");
        let secret_input = LineEditor::default();
        let suggestions = vec![InputSuggestion::new(
            "/provider",
            "/provider ",
            "配置或切换模型提供商",
        )];
        terminal
            .draw(|frame| {
                render_product_tui(
                    frame,
                    TuiView {
                        snapshot: &snapshot,
                        pack: snapshot.language.pack(),
                        history: &[
                            "You: build a release".to_owned(),
                            "Kernary: working".to_owned(),
                        ],
                        input: &input,
                        secret_prompt: None,
                        secret_input: &secret_input,
                        input_prompt: None,
                        suggestions: &suggestions,
                        suggestion_cursor: 0,
                        transcript_scroll: 0,
                        show_onboarding: false,
                        elapsed: Duration::from_millis(240),
                        options: TuiOptions {
                            ascii: false,
                            color: true,
                        },
                    },
                );
            })
            .expect("render narrow product TUI");
        let screen = screen_text(terminal.backend());
        assert!(screen.contains("/provider"), "screen={screen}");
        assert!(screen.contains("/pro"), "screen={screen}");
        assert!(screen.contains("kernary-demo"), "screen={screen}");
    }
}
