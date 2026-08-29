use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::{CommandRegistry, PlainRenderer, RenderStyle, compact_mark};

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
    fn poll(&mut self) -> BackendResponse {
        BackendResponse::default()
    }
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

/// 最小 Ratatui 交互循环。
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
    let mut input = String::new();
    let mut command_history = Vec::<String>::new();
    let mut history_cursor: Option<usize> = None;
    let started = Instant::now();
    let mut cancel = CancelController::default();
    let mut secret_prompt: Option<SecretPrompt> = None;
    let mut secret_input = String::new();

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
        let suggestions = if secret_prompt.is_none() && input.starts_with('/') {
            registry.complete(&input)
        } else {
            Vec::new()
        };
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(6),
                    Constraint::Min(5),
                    Constraint::Length(u16::try_from(suggestions.len().min(4)).unwrap_or(4)),
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

            if !suggestions.is_empty() {
                let items = suggestions
                    .iter()
                    .take(4)
                    .map(|command| ListItem::new(command.clone()))
                    .collect::<Vec<_>>();
                frame.render_widget(List::new(items), chunks[2]);
            }
            let (visible_input, input_title) = secret_prompt.as_ref().map_or_else(
                || {
                    (
                        format!("{} {input}", if options.ascii { ">" } else { "❯" }),
                        "Input".to_owned(),
                    )
                },
                |prompt| {
                    (
                        format!(
                            "{} {}",
                            if options.ascii { ">" } else { "❯" },
                            "*".repeat(secret_input.chars().count())
                        ),
                        format!("Secure · {}", prompt.prompt),
                    )
                },
            );
            frame.render_widget(
                Paragraph::new(visible_input)
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
            let cursor_x = chunks[3].x.saturating_add(2).saturating_add(
                u16::try_from(
                    secret_prompt
                        .as_ref()
                        .map_or_else(|| input.chars().count(), |_| secret_input.chars().count()),
                )
                .unwrap_or(u16::MAX),
            );
            frame.set_cursor_position((cursor_x, chunks[3].y.saturating_add(1)));
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
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
                KeyCode::Char(character) => secret_input.push(character),
                KeyCode::Backspace => {
                    secret_input.pop();
                }
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
                    let secret = std::mem::take(&mut secret_input);
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
            KeyCode::Char(character) => input.push(character),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => input.clear(),
            KeyCode::Tab => {
                if let Some(first) = registry.complete(&input).first() {
                    input.clone_from(first);
                }
            }
            KeyCode::Up => {
                if !command_history.is_empty() {
                    let next = history_cursor
                        .map_or(command_history.len() - 1, |index| index.saturating_sub(1));
                    history_cursor = Some(next);
                    input.clone_from(&command_history[next]);
                }
            }
            KeyCode::Down => {
                if let Some(index) = history_cursor {
                    if index + 1 < command_history.len() {
                        history_cursor = Some(index + 1);
                        input.clone_from(&command_history[index + 1]);
                    } else {
                        history_cursor = None;
                        input.clear();
                    }
                }
            }
            KeyCode::Enter => {
                let submitted = input.trim().to_owned();
                input.clear();
                history_cursor = None;
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
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
}
