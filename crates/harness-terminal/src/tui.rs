use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use harness_event::EventSubscription;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border::{self, Set as BorderSet};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{CommandRegistry, InputSuggestion, PlainRenderer, RenderStyle, compact_mark};

const ASCII_BORDER: BorderSet = BorderSet {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// TUI 顶部/状态栏需要的只读快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub model: String,
    pub model_configured: bool,
    pub mode: String,
    pub reasoning: String,
    pub context_percent: u8,
    pub cache_percent: Option<u8>,
    pub agents: usize,
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
}

/// 独立 secure input lane；request ID 和提示均不包含 secret。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretPrompt {
    pub request_id: String,
    pub prompt: String,
}

/// Terminal 只依赖该 Application interface。
pub trait TerminalBackend {
    fn handle_input(&mut self, input: &str) -> BackendResponse;
    fn snapshot(&self) -> TerminalSnapshot;
    fn cancel_current(&mut self) -> BackendResponse;
    fn submit_secret(&mut self, request_id: &str, secret: String) -> BackendResponse;
    fn complete_input(&self, _input: &str) -> Vec<InputSuggestion> {
        Vec::new()
    }
    fn poll(&mut self) -> BackendResponse {
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

/// Ratatui 交互循环：Unicode 行编辑、历史、Bracketed Paste 与可滚动 Slash 面板。
pub fn run_tui<B: TerminalBackend>(
    backend: &mut B,
    subscription: &EventSubscription,
    registry: CommandRegistry,
    options: TuiOptions,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let renderer = PlainRenderer::new(RenderStyle {
        ascii: options.ascii,
        color: options.color,
    });
    let mut history = Vec::<String>::new();
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

    if !backend.snapshot().model_configured {
        history.extend([
            "Kernary 尚未配置真实模型；测试模型不会接管用户输入。".to_owned(),
            "输入 /connect 并选择 Provider，再输入 /model 选择可用模型。".to_owned(),
            "输入 / 可浏览全部命令；↑↓ 选择，Tab 补全，←→ 移动光标。".to_owned(),
        ]);
    }

    loop {
        while let Ok(envelope) = subscription.try_recv() {
            history.push(renderer.render_event(&envelope));
        }
        let background = backend.poll();
        if let Some(prompt) = background.secret_prompt.clone() {
            secret_prompt = Some(prompt);
            secret_input.clear();
        }
        if background.clear_view {
            history.clear();
        } else {
            history.extend(
                background
                    .lines
                    .into_iter()
                    .map(|line| renderer.sanitize(&line)),
            );
        }
        if background.should_exit {
            break;
        }
        if history.len() > 1_000 {
            history.drain(..history.len() - 1_000);
        }

        let snapshot = backend.snapshot();
        let mut suggestions =
            if secret_prompt.is_none() && input.text().starts_with('/') && !suggestions_dismissed {
                let dynamic = backend.complete_input(input.text());
                if dynamic.is_empty() {
                    registry.suggestions(input.text())
                } else {
                    dynamic
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
        terminal.draw(|frame| {
            let suggestion_capacity = suggestions.len().min(8);
            let suggestion_height = if suggestion_capacity == 0 {
                0
            } else {
                u16::try_from(suggestion_capacity + 2).unwrap_or(10)
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(6),
                    Constraint::Min(5),
                    Constraint::Length(suggestion_height),
                    Constraint::Length(3),
                    Constraint::Length(u16::from(snapshot.statusbar_visible)),
                ])
                .split(frame.area());

            let border_style = if options.color {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let border_set = if options.ascii {
                ASCII_BORDER
            } else {
                border::PLAIN
            };
            let header = vec![
                Line::from(vec![
                    Span::styled(
                        compact_mark(options.ascii),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("   {}", snapshot.project)),
                ]),
                Line::from(format!(
                    "Model {}  Mode {}  Reasoning {}",
                    snapshot.model, snapshot.mode, snapshot.reasoning
                )),
                Line::from(format!(
                    "Context {}%  Cache {}  Agents {}",
                    snapshot.context_percent,
                    snapshot
                        .cache_percent
                        .map_or_else(|| "n/a".to_owned(), |value| format!("{value}%")),
                    snapshot.agents
                )),
            ];
            frame.render_widget(
                Paragraph::new(header).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_set(border_set)
                        .border_style(border_style),
                ),
                chunks[0],
            );

            let visible_height = usize::from(chunks[1].height.saturating_sub(2));
            let start = history.len().saturating_sub(visible_height);
            let items = history[start..]
                .iter()
                .map(|line| ListItem::new(line.clone()))
                .collect::<Vec<_>>();
            frame.render_widget(
                List::new(items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_set(border_set)
                        .title("Activity"),
                ),
                chunks[1],
            );

            if suggestion_capacity > 0 {
                let (window_start, window_end) =
                    suggestion_window(suggestions.len(), suggestion_cursor, suggestion_capacity);
                let items = suggestions[window_start..window_end]
                    .iter()
                    .map(|suggestion| {
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                suggestion.label.clone(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!("  {}", suggestion.description)),
                        ]))
                    })
                    .collect::<Vec<_>>();
                let mut state = ListState::default();
                state.select(Some(suggestion_cursor - window_start));
                let highlight = if options.color {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::REVERSED)
                };
                let title = format!(
                    "Commands {}/{} · ↑↓ select · Tab complete · Esc close",
                    suggestion_cursor + 1,
                    suggestions.len()
                );
                frame.render_stateful_widget(
                    List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_set(border_set)
                                .title(title),
                        )
                        .highlight_style(highlight)
                        .highlight_symbol(if options.ascii { "> " } else { "› " }),
                    chunks[2],
                    &mut state,
                );
            }
            let prefix = if options.ascii { "> " } else { "❯ " };
            let prefix_width = UnicodeWidthStr::width(prefix);
            let available_width =
                usize::from(chunks[3].width.saturating_sub(2)).saturating_sub(prefix_width);
            let (visible_input, cursor_column, input_title) = secret_prompt.as_ref().map_or_else(
                || {
                    let (visible, cursor) = input.visible_window(available_width, false);
                    (
                        visible,
                        cursor,
                        "Input · ←→ move · Home/End · ↑↓ history/commands".to_owned(),
                    )
                },
                |prompt| {
                    let (visible, cursor) = secret_input.visible_window(available_width, true);
                    (visible, cursor, format!("Secure · {}", prompt.prompt))
                },
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(visible_input),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_set(border_set)
                        .title(input_title),
                )
                .wrap(Wrap { trim: false }),
                chunks[3],
            );
            let separator = if options.ascii { " | " } else { " │ " };
            let status = format!(
                "{}{}{}{}ctx {}%{}cache {}{}agents {}{}",
                snapshot.model,
                separator,
                snapshot.reasoning,
                separator,
                snapshot.context_percent,
                separator,
                snapshot
                    .cache_percent
                    .map_or_else(|| "n/a".to_owned(), |value| format!("{value}%")),
                separator,
                snapshot.agents,
                snapshot
                    .branch
                    .as_ref()
                    .map_or_else(String::new, |branch| format!("{separator}{branch}"))
            );
            if snapshot.statusbar_visible {
                frame.render_widget(Paragraph::new(status), chunks[4]);
            }
            let cursor_x = chunks[3]
                .x
                .saturating_add(1)
                .saturating_add(u16::try_from(prefix_width).unwrap_or(u16::MAX))
                .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX))
                .min(chunks[3].right().saturating_sub(2));
            frame.set_cursor_position((cursor_x, chunks[3].y.saturating_add(1)));
        })?;

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
                }
                _ => {}
            }
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
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
                    history.push("Press Ctrl+C again within 2s to exit".to_owned());
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
                    continue;
                }
                command_history.push(submitted.clone());
                history.push(format!("You: {submitted}"));
                let response = backend.handle_input(&submitted);
                if response.clear_view {
                    history.clear();
                } else {
                    history.extend(
                        response
                            .lines
                            .into_iter()
                            .map(|line| renderer.sanitize(&line)),
                    );
                }
                if response.should_exit {
                    break;
                }
                secret_prompt = response.secret_prompt;
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
    use super::*;

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
}
