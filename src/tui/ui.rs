//! SuperLightTUI renderer and event loop for GithubClaw.

use std::io::{self, IsTerminal, Stdout, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as CrosstermEvent, KeyCode as CtKeyCode,
    KeyEventKind as CtKeyEventKind, KeyModifiers as CtKeyModifiers, MouseButton as CtMouseButton,
    MouseEventKind as CtMouseEventKind,
};
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{self, BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{cursor, execute, queue};
use slt::event::KeyEvent as SltKeyEvent;
use slt::{
    frame, AppState as SltAppState, Backend, Border, Buffer, Color, ColorDepth, Context, Event,
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseKind, Rect, RunConfig,
    ScrollState, SpinnerState, Style, TabsState, Theme,
};
use unicode_width::UnicodeWidthStr;

use super::app::{App, IssueReviewDecision, PendingIssueReview};
use super::event::AppEvent;
use super::summary::{CardActionState, CardTone, SummaryCard};
use super::tabs::{AgentSessionItem, AgentStatus, IssueRequestItem, Tab};

pub fn run(repo_root: &Path) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Ok(());
    }

    let config = RunConfig {
        tick_rate: Duration::from_millis(50),
        mouse: true,
        kitty_keyboard: false,
        theme: Theme::catppuccin(),
        color_depth: None,
        max_fps: Some(30),
    };

    let color_depth = config.color_depth.unwrap_or_else(ColorDepth::detect);
    let mut backend = TerminalBackend::new(config.mouse, config.kitty_keyboard, color_depth)?;
    if config.theme.bg != Color::Reset {
        backend.theme_bg = Some(config.theme.bg);
    }

    let mut slt_state = SltAppState::new();
    let mut app = App::new();
    let mut slt_events = Vec::new();
    let mut tabs = TabsState::new(vec!["Issue Request", "Monitoring"]);
    let spinner = SpinnerState::dots();

    let mut issue_scroll = ScrollState::new();
    let mut issue_detail_scroll = ScrollState::new();
    let mut session_scroll = ScrollState::new();
    let mut timeline_scroll = ScrollState::new();
    let mut events_scroll = ScrollState::new();
    let mut app_screen_scroll = ScrollState::new();

    app.handle_event(AppEvent::Tick);

    loop {
        let keep_going = frame(
            &mut backend,
            &mut slt_state,
            &config,
            &slt_events,
            &mut |ui| {
                render_dashboard(
                    ui,
                    repo_root,
                    &mut app,
                    &mut tabs,
                    &spinner,
                    &mut issue_scroll,
                    &mut issue_detail_scroll,
                    &mut session_scroll,
                    &mut timeline_scroll,
                    &mut events_scroll,
                    &mut app_screen_scroll,
                );
            },
        )?;

        if !keep_going || app.should_quit {
            break;
        }

        if let Some(issue_number) = app.take_pending_interactive_issue() {
            app.start_interactive_session_for_issue(issue_number, repo_root);
        }

        if let Some(review) = app.take_pending_issue_review() {
            if let Err(err) = app.apply_issue_review(repo_root, review) {
                tracing::warn!(issue = review.issue_number, error = %err, "Failed to apply issue review");
            }
        }

        slt_events.clear();
        let poll_timeout = if app.interactive_session_active {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(250)
        };

        if event::poll(poll_timeout)? {
            let raw = event::read()?;
            handle_terminal_event(&mut app, &mut slt_events, &mut backend, raw)?;

            while event::poll(Duration::ZERO)? {
                let raw = event::read()?;
                handle_terminal_event(&mut app, &mut slt_events, &mut backend, raw)?;
            }
        } else {
            app.handle_event(AppEvent::Tick);
        }

        sleep_for_fps_cap(config.max_fps, Instant::now());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_dashboard(
    ui: &mut Context,
    repo_root: &Path,
    app: &mut App,
    tabs: &mut TabsState,
    spinner: &SpinnerState,
    issue_scroll: &mut ScrollState,
    issue_detail_scroll: &mut ScrollState,
    session_scroll: &mut ScrollState,
    timeline_scroll: &mut ScrollState,
    events_scroll: &mut ScrollState,
    app_screen_scroll: &mut ScrollState,
) {
    tabs.selected = tab_index(app.active_tab);

    ui.bordered(Border::Rounded)
        .title("GithubClaw Control Room")
        .pad(1)
        .grow(1)
        .col(|ui| {
            render_header(ui, repo_root, app, tabs, spinner);
            ui.divider_text("Overview");
            render_metrics(ui, app);
            ui.divider_text(match app.active_tab {
                Tab::IssueRequest => "Issue Request",
                Tab::Monitoring => "Monitoring",
            });

            ui.scrollable(app_screen_scroll)
                .grow(1)
                .col(|ui| match app.active_tab {
                    Tab::IssueRequest => {
                        render_issue_request(ui, app, issue_scroll, issue_detail_scroll, repo_root);
                    }
                    Tab::Monitoring => {
                        render_monitoring(ui, app, session_scroll, timeline_scroll, events_scroll);
                    }
                });

            ui.divider_text("Controls");
            render_help(ui, app);
        });
}

fn render_header(
    ui: &mut Context,
    repo_root: &Path,
    app: &mut App,
    tabs: &mut TabsState,
    spinner: &SpinnerState,
) {
    ui.row(|ui| {
        ui.spinner(spinner);
        ui.text(" GithubClaw").bold().fg(Color::Cyan);
        if app.interactive_session_active {
            ui.badge_colored("SESSION LIVE", Color::Yellow);
        } else {
            ui.badge_colored("READY", Color::Green);
        }
        ui.spacer();
        ui.text(repo_root.to_string_lossy()).dim();
    });

    ui.row(|ui| {
        let old_selected = tabs.selected;
        let _ = ui.tabs(tabs);
        if tabs.selected != old_selected {
            app.active_tab = tab_from_index(tabs.selected);
        }
        ui.spacer();
        if ui.button("Refresh").clicked {
            app.handle_event(AppEvent::Tick);
        }
    });
}

fn render_metrics(ui: &mut Context, app: &App) {
    let waiting = app.issue_requests.len();
    let running = app.worker_count.0;
    let total = app.agent_sessions.len();
    let rate_color = if app.rate_limit_tier.eq_ignore_ascii_case("none") {
        Color::Green
    } else {
        Color::Yellow
    };

    ui.row(|ui| {
        metric_card(ui, "Inbox", &waiting.to_string(), Color::Cyan);
        metric_card(ui, "Running", &running.to_string(), Color::Green);
        metric_card(ui, "Observed", &total.to_string(), Color::Blue);
        metric_card(
            ui,
            "Queue",
            &app.queue_depth.to_string(),
            queue_color(app.queue_depth),
        );
        metric_card(ui, "Rate", &app.rate_limit_tier, rate_color);
    });
}

fn metric_card(ui: &mut Context, label: &str, value: &str, color: Color) {
    ui.bordered(Border::Rounded).pad(1).grow(1).col(|ui| {
        ui.text(label).dim();
        ui.text(value).bold().fg(color);
    });
}

fn render_issue_request(
    ui: &mut Context,
    app: &mut App,
    issue_scroll: &mut ScrollState,
    issue_detail_scroll: &mut ScrollState,
    _repo_root: &Path,
) {
    ui.row(|ui| {
        ui.bordered(Border::Rounded)
            .title("Inbox")
            .pad(1)
            .grow(1)
            .col(|ui| {
                if app.issue_requests.is_empty() {
                    ui.text("No issues are waiting for interactive review.").fg(Color::Green);
                    ui.text("Feature and refactor requests will appear here after analysis.").dim();
                    return;
                }

                ui.scrollable(issue_scroll).grow(1).col(|ui| {
                    for index in 0..app.issue_requests.len() {
                        let issue = app.issue_requests[index].clone();
                        render_issue_row(ui, app, index, issue);
                    }
                });
            });

        ui.bordered(Border::Rounded)
            .title("Interactive Session")
            .pad(1)
            .grow(2)
            .col(|ui| {
                let summary = app.issue_request_summary_card();
                render_summary_card(ui, &summary);
                ui.separator();

                if let Some(issue) = app.selected_issue().cloned() {
                    ui.row(|ui| {
                        ui.text(format!("#{}", issue.issue_number))
                            .bold()
                            .fg(Color::Cyan);
                        ui.badge_colored(&issue.issue_type, issue_type_color(&issue.issue_type));
                        if issue.vision_report_ready {
                            ui.badge_colored("READY", Color::Green);
                        } else {
                            ui.badge_colored("WARMING", Color::Yellow);
                        }
                    });

                    ui.text(&issue.title).bold();
                    ui.separator();

                    if app.interactive_session_active {
                        ui.row(|ui| {
                            if ui.button("Approve").clicked {
                                app.pending_issue_review = Some(PendingIssueReview {
                                    issue_number: issue.issue_number,
                                    decision: IssueReviewDecision::Approve,
                                });
                            }
                            if ui.button("Reject").clicked {
                                app.pending_issue_review = Some(PendingIssueReview {
                                    issue_number: issue.issue_number,
                                    decision: IssueReviewDecision::Reject,
                                });
                            }
                            ui.text("Esc returns to the inbox.").dim();
                        });
                    } else {
                        ui.row(|ui| {
                            if ui.button("Open session").clicked {
                                app.pending_interactive_issue = Some(issue.issue_number);
                            }
                            if ui.button("Approve").clicked {
                                app.pending_issue_review = Some(PendingIssueReview {
                                    issue_number: issue.issue_number,
                                    decision: IssueReviewDecision::Approve,
                                });
                            }
                            if ui.button("Reject").clicked {
                                app.pending_issue_review = Some(PendingIssueReview {
                                    issue_number: issue.issue_number,
                                    decision: IssueReviewDecision::Reject,
                                });
                            }
                        });
                    }

                    ui.separator();
                    if app.interactive_session_active && !app.pty_output.is_empty() {
                        issue_detail_scroll.scroll_down(usize::MAX / 4);
                    }
                    ui.scrollable(issue_detail_scroll).grow(1).col(|ui| {
                        if app.interactive_session_active {
                            if app.pty_output.is_empty() {
                                ui.text("Starting interactive session...").dim();
                            } else {
                                for line in app.pty_output.lines() {
                                    ui.text(line);
                                }
                            }
                        } else {
                            ui.text("Open the session to speak directly with the orchestrator.")
                                .fg(Color::LightBlue);
                            ui.text("Keyboard input is passed through to the embedded PTY once the session starts.")
                                .dim();
                        }
                    });
                } else {
                    ui.text("Select an issue from the inbox to inspect it.").dim();
                }
            });
    });
}

fn render_issue_row(ui: &mut Context, app: &mut App, index: usize, issue: IssueRequestItem) {
    let selected = index == app.selected_issue_index;
    let border = if selected {
        Border::Double
    } else {
        Border::Rounded
    };
    let accent = if selected {
        Color::Yellow
    } else {
        Color::Indexed(246)
    };

    ui.bordered(border).pad(1).mb(1).col(|ui| {
        ui.row(|ui| {
            if ui
                .button(format!("#{} {}", issue.issue_number, issue.title))
                .clicked
            {
                app.selected_issue_index = index;
            }
            ui.spacer();
            ui.badge_colored(&issue.issue_type, issue_type_color(&issue.issue_type));
            if issue.vision_report_ready {
                ui.badge_colored("READY", Color::Green);
            }
        });
        ui.text(format!(
            "{} request is waiting for operator review.",
            issue.issue_type
        ))
        .fg(accent);
    });
}

fn render_monitoring(
    ui: &mut Context,
    app: &mut App,
    session_scroll: &mut ScrollState,
    timeline_scroll: &mut ScrollState,
    events_scroll: &mut ScrollState,
) {
    ui.row(|ui| {
        ui.bordered(Border::Rounded)
            .title("Sessions")
            .pad(1)
            .grow(1)
            .col(|ui| {
                if app.agent_sessions.is_empty() {
                    ui.text("No active or recorded sessions yet.").dim();
                } else {
                    ui.scrollable(session_scroll).grow(1).col(|ui| {
                        for index in 0..app.agent_sessions.len() {
                            let session = app.agent_sessions[index].clone();
                            render_session_row(ui, app, index, session);
                        }
                    });
                }
            });

        ui.bordered(Border::Rounded)
            .title("Timeline")
            .pad(1)
            .grow(2)
            .col(|ui| {
                let summary = app.monitoring_summary_card();
                render_summary_card(ui, &summary);
                ui.separator();

                ui.scrollable(timeline_scroll).grow(1).col(|ui| {
                    if app.agent_timeline.is_empty() {
                        ui.text("No timeline entries yet.").dim();
                    } else {
                        for entry in &app.agent_timeline {
                            ui.row(|ui| {
                                ui.badge_colored(
                                    entry.status.symbol(),
                                    agent_status_color(&entry.status),
                                );
                                ui.text(&entry.agent_type).bold();
                                ui.text(&entry.detail).fg(Color::Indexed(248));
                            });
                        }
                    }
                });

                ui.separator();
                ui.text("Recent Events").bold().fg(Color::Cyan);
                ui.scrollable(events_scroll).grow(1).col(|ui| {
                    if app.recent_events.is_empty() {
                        ui.text("No recent timeline events.").dim();
                    } else {
                        for event in &app.recent_events {
                            ui.text(event);
                        }
                    }
                });
            });
    });
}

fn render_session_row(ui: &mut Context, app: &mut App, index: usize, session: AgentSessionItem) {
    let selected = index == app.selected_agent_index;
    let border = if selected {
        Border::Double
    } else {
        Border::Rounded
    };

    ui.bordered(border).pad(1).mb(1).col(|ui| {
        ui.row(|ui| {
            if ui
                .button(format!("#{} {}", session.issue_number, session.agent_type))
                .clicked
            {
                app.selected_agent_index = index;
            }
            ui.spacer();
            ui.badge_colored(session.status.symbol(), agent_status_color(&session.status));
        });
        ui.text(format!("Started at {}", session.started_at)).dim();
    });
}

fn render_summary_card(ui: &mut Context, card: &SummaryCard) {
    ui.bordered(Border::Rounded)
        .title(&card.title)
        .pad(1)
        .col(|ui| {
            ui.text(&card.status_line)
                .bold()
                .fg(card_tone_color(card.tone));
            for bullet in card.bullets.iter().take(3) {
                ui.text(format!("- {bullet}")).fg(Color::Indexed(248));
            }
            if let Some(next_action) = &card.next_action {
                ui.separator();
                ui.text("Next").bold().fg(Color::Cyan);
                ui.text(next_action);
            }
            ui.row(|ui| {
                ui.spacer();
                ui.badge_colored(
                    action_state_label(card.action_state),
                    card_tone_color(card.tone),
                );
            });
        });
}

fn render_help(ui: &mut Context, app: &App) {
    if app.interactive_session_active {
        ui.help(&[
            ("Esc", "back"),
            ("Ctrl+A", "approve"),
            ("Ctrl+R", "reject"),
            ("q", "stay in PTY"),
        ]);
    } else {
        ui.help(&[
            ("Tab", "switch view"),
            ("j/k", "move"),
            ("Enter", "open session"),
            ("Ctrl+A", "approve"),
            ("Ctrl+R", "reject"),
            ("q", "quit"),
        ]);
    }
}

fn tab_index(tab: Tab) -> usize {
    match tab {
        Tab::IssueRequest => 0,
        Tab::Monitoring => 1,
    }
}

fn tab_from_index(index: usize) -> Tab {
    match index {
        1 => Tab::Monitoring,
        _ => Tab::IssueRequest,
    }
}

fn issue_type_color(issue_type: &str) -> Color {
    match issue_type {
        "Feature" => Color::LightBlue,
        "Refactoring" => Color::Magenta,
        _ => Color::Cyan,
    }
}

fn agent_status_color(status: &AgentStatus) -> Color {
    match status {
        AgentStatus::Running => Color::Green,
        AgentStatus::Queued => Color::Yellow,
        AgentStatus::Completed => Color::Blue,
        AgentStatus::Failed => Color::Red,
        AgentStatus::Idle => Color::Indexed(245),
    }
}

fn queue_color(queue_depth: usize) -> Color {
    match queue_depth {
        0 => Color::Green,
        1..=3 => Color::Yellow,
        _ => Color::Red,
    }
}

fn action_state_label(state: CardActionState) -> &'static str {
    match state {
        CardActionState::Passive => "PASSIVE",
        CardActionState::Watching => "WATCHING",
        CardActionState::NeedsDecision => "DECISION",
        CardActionState::Blocked => "BLOCKED",
        CardActionState::InProgress => "LIVE",
        CardActionState::Done => "DONE",
    }
}

fn card_tone_color(tone: CardTone) -> Color {
    match tone {
        CardTone::Info => Color::Cyan,
        CardTone::Success => Color::Green,
        CardTone::Warning => Color::Yellow,
        CardTone::Danger => Color::Red,
    }
}

fn handle_terminal_event(
    app: &mut App,
    slt_events: &mut Vec<Event>,
    backend: &mut TerminalBackend,
    raw: CrosstermEvent,
) -> io::Result<()> {
    match &raw {
        CrosstermEvent::Key(key) => {
            app.handle_event(AppEvent::Key(*key));
        }
        CrosstermEvent::Mouse(mouse) => {
            app.handle_event(AppEvent::Mouse(*mouse));
        }
        CrosstermEvent::Resize(width, height) => {
            backend.handle_resize()?;
            app.handle_event(AppEvent::Resize(*width, *height));
        }
        _ => {}
    }

    if let Some(event) = convert_crossterm_event(raw) {
        if should_forward_to_slt(app, &event) {
            slt_events.push(event);
        }
    }

    Ok(())
}

fn should_forward_to_slt(app: &App, event: &Event) -> bool {
    match event {
        Event::Mouse(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => true,
        Event::Paste(_) => !app.interactive_session_active,
        Event::Key(_) => false,
    }
}

fn convert_crossterm_event(raw: CrosstermEvent) -> Option<Event> {
    match raw {
        CrosstermEvent::Key(key) => Some(Event::Key(SltKeyEvent {
            code: convert_key_code(key.code)?,
            modifiers: convert_key_modifiers(key.modifiers),
            kind: match key.kind {
                CtKeyEventKind::Press => KeyEventKind::Press,
                CtKeyEventKind::Repeat => KeyEventKind::Repeat,
                CtKeyEventKind::Release => KeyEventKind::Release,
            },
        })),
        CrosstermEvent::Mouse(mouse) => Some(Event::Mouse(MouseEvent {
            kind: match mouse.kind {
                CtMouseEventKind::Down(button) => MouseKind::Down(convert_mouse_button(button)),
                CtMouseEventKind::Up(button) => MouseKind::Up(convert_mouse_button(button)),
                CtMouseEventKind::Drag(button) => MouseKind::Drag(convert_mouse_button(button)),
                CtMouseEventKind::Moved => MouseKind::Moved,
                CtMouseEventKind::ScrollUp => MouseKind::ScrollUp,
                CtMouseEventKind::ScrollDown => MouseKind::ScrollDown,
                _ => return None,
            },
            x: mouse.column as u32,
            y: mouse.row as u32,
            modifiers: convert_key_modifiers(mouse.modifiers),
        })),
        CrosstermEvent::Resize(width, height) => Some(Event::Resize(width as u32, height as u32)),
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
        CrosstermEvent::FocusGained => Some(Event::FocusGained),
        CrosstermEvent::FocusLost => Some(Event::FocusLost),
    }
}

fn convert_key_code(code: CtKeyCode) -> Option<KeyCode> {
    match code {
        CtKeyCode::Char(ch) => Some(KeyCode::Char(ch)),
        CtKeyCode::Enter => Some(KeyCode::Enter),
        CtKeyCode::Backspace => Some(KeyCode::Backspace),
        CtKeyCode::Tab => Some(KeyCode::Tab),
        CtKeyCode::BackTab => Some(KeyCode::BackTab),
        CtKeyCode::Esc => Some(KeyCode::Esc),
        CtKeyCode::Up => Some(KeyCode::Up),
        CtKeyCode::Down => Some(KeyCode::Down),
        CtKeyCode::Left => Some(KeyCode::Left),
        CtKeyCode::Right => Some(KeyCode::Right),
        CtKeyCode::Home => Some(KeyCode::Home),
        CtKeyCode::End => Some(KeyCode::End),
        CtKeyCode::PageUp => Some(KeyCode::PageUp),
        CtKeyCode::PageDown => Some(KeyCode::PageDown),
        CtKeyCode::Delete => Some(KeyCode::Delete),
        CtKeyCode::F(number) => Some(KeyCode::F(number)),
        _ => None,
    }
}

fn convert_key_modifiers(modifiers: CtKeyModifiers) -> KeyModifiers {
    let mut converted = KeyModifiers::NONE;
    if modifiers.contains(CtKeyModifiers::SHIFT) {
        converted.0 |= KeyModifiers::SHIFT.0;
    }
    if modifiers.contains(CtKeyModifiers::CONTROL) {
        converted.0 |= KeyModifiers::CONTROL.0;
    }
    if modifiers.contains(CtKeyModifiers::ALT) {
        converted.0 |= KeyModifiers::ALT.0;
    }
    converted
}

fn convert_mouse_button(button: CtMouseButton) -> MouseButton {
    match button {
        CtMouseButton::Left => MouseButton::Left,
        CtMouseButton::Right => MouseButton::Right,
        CtMouseButton::Middle => MouseButton::Middle,
    }
}

fn sleep_for_fps_cap(max_fps: Option<u32>, frame_start: Instant) {
    if let Some(fps) = max_fps.filter(|fps| *fps > 0) {
        let target = Duration::from_secs_f64(1.0 / fps as f64);
        let elapsed = frame_start.elapsed();
        if elapsed < target {
            std::thread::sleep(target - elapsed);
        }
    }
}

struct TerminalBackend {
    stdout: Stdout,
    current: Buffer,
    previous: Buffer,
    mouse_enabled: bool,
    cursor_visible: bool,
    kitty_keyboard: bool,
    color_depth: ColorDepth,
    theme_bg: Option<Color>,
}

impl TerminalBackend {
    fn new(mouse: bool, kitty_keyboard: bool, color_depth: ColorDepth) -> io::Result<Self> {
        let (cols, rows) = terminal::size()?;
        let area = Rect::new(0, 0, cols as u32, rows as u32);

        let mut stdout = io::stdout();
        terminal::enable_raw_mode()?;
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            cursor::Hide,
            EnableBracketedPaste
        )?;
        if mouse {
            execute!(stdout, EnableMouseCapture, EnableFocusChange)?;
        }
        if kitty_keyboard {
            use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            );
        }

        Ok(Self {
            stdout,
            current: Buffer::empty(area),
            previous: Buffer::empty(area),
            mouse_enabled: mouse,
            cursor_visible: false,
            kitty_keyboard,
            color_depth,
            theme_bg: None,
        })
    }

    fn handle_resize(&mut self) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        let area = Rect::new(0, 0, cols as u32, rows as u32);
        self.current.resize(area);
        self.previous.resize(area);
        execute!(
            self.stdout,
            terminal::Clear(terminal::ClearType::All),
            cursor::MoveTo(0, 0)
        )?;
        Ok(())
    }
}

impl Backend for TerminalBackend {
    fn size(&self) -> (u32, u32) {
        (self.current.area.width, self.current.area.height)
    }

    fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.current
    }

    fn flush(&mut self) -> io::Result<()> {
        queue!(self.stdout, BeginSynchronizedUpdate)?;

        let mut last_style = Style::new();
        let mut first_style = true;
        let mut last_pos: Option<(u32, u32)> = None;
        let mut active_link: Option<&str> = None;
        let mut has_updates = false;

        for y in self.current.area.y..self.current.area.bottom() {
            for x in self.current.area.x..self.current.area.right() {
                let cur = self.current.get(x, y);
                let prev = self.previous.get(x, y);
                if cur == prev || cur.symbol.is_empty() {
                    continue;
                }
                has_updates = true;

                let need_move = last_pos.is_none_or(|(lx, ly)| ly != y || lx != x);
                if need_move {
                    queue!(self.stdout, cursor::MoveTo(x as u16, y as u16))?;
                }

                if cur.style != last_style {
                    if first_style {
                        queue!(self.stdout, ResetColor, SetAttribute(Attribute::Reset))?;
                        apply_style(&mut self.stdout, &cur.style, self.color_depth)?;
                        first_style = false;
                    } else {
                        apply_style_delta(
                            &mut self.stdout,
                            &last_style,
                            &cur.style,
                            self.color_depth,
                        )?;
                    }
                    last_style = cur.style;
                }

                let cell_link = cur.hyperlink.as_deref();
                if cell_link != active_link {
                    if let Some(url) = cell_link {
                        queue!(self.stdout, Print(format!("\x1b]8;;{url}\x07")))?;
                    } else {
                        queue!(self.stdout, Print("\x1b]8;;\x07"))?;
                    }
                    active_link = cell_link;
                }

                queue!(self.stdout, Print(&*cur.symbol))?;
                let char_width = UnicodeWidthStr::width(cur.symbol.as_str()).max(1) as u32;
                last_pos = Some((x + char_width, y));
            }
        }

        if has_updates {
            if active_link.is_some() {
                queue!(self.stdout, Print("\x1b]8;;\x07"))?;
            }
            queue!(self.stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        }

        queue!(self.stdout, EndSynchronizedUpdate)?;

        if let Some((cursor_x, cursor_y)) = find_cursor_marker(&self.current) {
            if !self.cursor_visible {
                queue!(self.stdout, cursor::Show)?;
                self.cursor_visible = true;
            }
            queue!(
                self.stdout,
                cursor::MoveTo(cursor_x as u16, cursor_y as u16)
            )?;
        } else if self.cursor_visible {
            queue!(self.stdout, cursor::Hide)?;
            self.cursor_visible = false;
        }

        self.stdout.flush()?;

        std::mem::swap(&mut self.current, &mut self.previous);
        reset_current_buffer(&mut self.current, self.theme_bg);
        Ok(())
    }
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        if self.kitty_keyboard {
            use crossterm::event::PopKeyboardEnhancementFlags;
            let _ = execute!(self.stdout, PopKeyboardEnhancementFlags);
        }
        if self.mouse_enabled {
            let _ = execute!(self.stdout, DisableMouseCapture, DisableFocusChange);
        }
        let _ = execute!(
            self.stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            cursor::Show,
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn find_cursor_marker(buffer: &Buffer) -> Option<(u32, u32)> {
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            if buffer.get(x, y).symbol == "▎" {
                return Some((x, y));
            }
        }
    }
    None
}

fn reset_current_buffer(buffer: &mut Buffer, theme_bg: Option<Color>) {
    if let Some(bg) = theme_bg {
        buffer.reset_with_bg(bg);
    } else {
        buffer.reset();
    }
}

fn apply_style_delta(
    writer: &mut impl Write,
    old: &Style,
    new: &Style,
    depth: ColorDepth,
) -> io::Result<()> {
    if old.fg != new.fg {
        match new.fg {
            Some(fg) => queue!(writer, SetForegroundColor(to_crossterm_color(fg, depth)))?,
            None => queue!(writer, SetForegroundColor(CtColor::Reset))?,
        }
    }
    if old.bg != new.bg {
        match new.bg {
            Some(bg) => queue!(writer, SetBackgroundColor(to_crossterm_color(bg, depth)))?,
            None => queue!(writer, SetBackgroundColor(CtColor::Reset))?,
        }
    }

    let removed = slt::Modifiers(old.modifiers.0 & !new.modifiers.0);
    let added = slt::Modifiers(new.modifiers.0 & !old.modifiers.0);

    if removed.contains(slt::Modifiers::BOLD) || removed.contains(slt::Modifiers::DIM) {
        queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
        if new.modifiers.contains(slt::Modifiers::BOLD) {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        if new.modifiers.contains(slt::Modifiers::DIM) {
            queue!(writer, SetAttribute(Attribute::Dim))?;
        }
    } else {
        if added.contains(slt::Modifiers::BOLD) {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
        if added.contains(slt::Modifiers::DIM) {
            queue!(writer, SetAttribute(Attribute::Dim))?;
        }
    }
    if removed.contains(slt::Modifiers::ITALIC) {
        queue!(writer, SetAttribute(Attribute::NoItalic))?;
    }
    if added.contains(slt::Modifiers::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if removed.contains(slt::Modifiers::UNDERLINE) {
        queue!(writer, SetAttribute(Attribute::NoUnderline))?;
    }
    if added.contains(slt::Modifiers::UNDERLINE) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if removed.contains(slt::Modifiers::REVERSED) {
        queue!(writer, SetAttribute(Attribute::NoReverse))?;
    }
    if added.contains(slt::Modifiers::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if removed.contains(slt::Modifiers::STRIKETHROUGH) {
        queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
    }
    if added.contains(slt::Modifiers::STRIKETHROUGH) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn apply_style(writer: &mut impl Write, style: &Style, depth: ColorDepth) -> io::Result<()> {
    if let Some(fg) = style.fg {
        queue!(writer, SetForegroundColor(to_crossterm_color(fg, depth)))?;
    }
    if let Some(bg) = style.bg {
        queue!(writer, SetBackgroundColor(to_crossterm_color(bg, depth)))?;
    }

    if style.modifiers.contains(slt::Modifiers::BOLD) {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if style.modifiers.contains(slt::Modifiers::DIM) {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if style.modifiers.contains(slt::Modifiers::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if style.modifiers.contains(slt::Modifiers::UNDERLINE) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if style.modifiers.contains(slt::Modifiers::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if style.modifiers.contains(slt::Modifiers::STRIKETHROUGH) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn to_crossterm_color(color: Color, depth: ColorDepth) -> CtColor {
    let color = color.downsampled(depth);
    match color {
        Color::Reset => CtColor::Reset,
        Color::Black => CtColor::Black,
        Color::Red => CtColor::DarkRed,
        Color::Green => CtColor::DarkGreen,
        Color::Yellow => CtColor::DarkYellow,
        Color::Blue => CtColor::DarkBlue,
        Color::Magenta => CtColor::DarkMagenta,
        Color::Cyan => CtColor::DarkCyan,
        Color::White => CtColor::White,
        Color::DarkGray => CtColor::DarkGrey,
        Color::LightRed => CtColor::Red,
        Color::LightGreen => CtColor::Green,
        Color::LightYellow => CtColor::Yellow,
        Color::LightBlue => CtColor::Blue,
        Color::LightMagenta => CtColor::Magenta,
        Color::LightCyan => CtColor::Cyan,
        Color::LightWhite => CtColor::White,
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        Color::Indexed(index) => CtColor::AnsiValue(index),
    }
}
