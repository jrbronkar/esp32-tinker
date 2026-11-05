//! Terminal dashboard for the gRPC management service.
mod update;
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Stdout},
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use ota_protocol::{LogSeverity, ReadLogsRequest, ReadLogsResponse, StatusResponse};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Gauge, List, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{discovery, QueryTimings};

const LOG_HISTORY: usize = 1_000;
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

type Screen = Terminal<CrosstermBackend<Stdout>>;

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            ratatui::crossterm::cursor::Show
        );
    }
}

pub fn run(
    direct: Option<SocketAddr>,
    interface: Option<Ipv4Addr>,
    timeout: Duration,
    firmware: Option<(std::path::PathBuf, Option<String>)>,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "espctl tui requires an interactive terminal",
        ));
    }
    terminal::enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut screen = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    screen.hide_cursor()?;
    // A LAN scan may still be finishing when the user exits.
    let runtime = crate::runtime()?;
    let result = runtime.block_on(dashboard(&mut screen, direct, interface, timeout, firmware));
    runtime.shutdown_background();
    result
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    name: String,
    address: SocketAddr,
}

enum Update {
    Status(Box<StatusResponse>),
    Logs(Box<ReadLogsResponse>),
    Retry(String),
}

struct Session {
    task: JoinHandle<()>,
    updates: mpsc::Receiver<Update>,
}

impl Session {
    fn start(address: SocketAddr, timeout: Duration) -> Self {
        let (sender, updates) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let mut cursor = ReadLogsRequest::default();
            loop {
                let Err(error) = monitor(address, timeout, &mut cursor, &sender).await else {
                    break;
                };
                if sender.send(Update::Retry(error.to_string())).await.is_err() {
                    break;
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        });
        Self { task, updates }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn monitor(
    address: SocketAddr,
    timeout: Duration,
    cursor: &mut ReadLogsRequest,
    sender: &mpsc::Sender<Update>,
) -> io::Result<()> {
    let end = crate::deadline(Instant::now(), timeout)?;
    let mut client = crate::connect(address, end, &mut QueryTimings::default(), true).await?;
    let mut logs = crate::open_logs(&mut client, end, cursor).await?;
    let status_updates = async {
        let mut ticks = tokio::time::interval(STATUS_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let end = crate::deadline(Instant::now(), timeout)?;
            let response = crate::query(
                &mut client,
                end,
                &ota_protocol::StatusRequest::default(),
                device_grpc::STATUS_PATH,
                &mut QueryTimings::default(),
            )
            .await?;
            let status = crate::decode_status(&response)?;
            if sender.send(Update::Status(Box::new(status))).await.is_err() {
                return Ok(());
            }
        }
    };
    let log_updates = async {
        let mut first = true;
        loop {
            let message = if first {
                tokio::time::timeout_at(end, logs.message())
                    .await
                    .map_err(|_| crate::timed_out())?
            } else {
                logs.message().await
            };
            let bytes = message
                .map_err(crate::rpc)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "log stream ended"))?;
            // Reserve space before advancing the cursor. A status failure can
            // cancel this future and reconnect while the UI channel is full.
            let Ok(permit) = sender.reserve().await else {
                return Ok(());
            };
            let batch = crate::decode_logs(cursor, &bytes)?;
            permit.send(Update::Logs(Box::new(batch)));
            first = false;
        }
    };
    tokio::select! {
        result = status_updates => result,
        result = log_updates => result,
    }
}

struct LogRow {
    severity: Option<LogSeverity>,
    text: String,
}

#[derive(Clone, Copy, Default)]
enum Filter {
    #[default]
    All,
    Warn,
    Error,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::All,
        }
    }

    fn includes(self, severity: Option<LogSeverity>) -> bool {
        match severity {
            None => true,
            Some(level) => match self {
                Self::All => true,
                Self::Warn => matches!(level, LogSeverity::Warn | LogSeverity::Error),
                Self::Error => level == LogSeverity::Error,
            },
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all levels",
            Self::Warn => "warn and error",
            Self::Error => "errors",
        }
    }
}

// Explicit xterm colors keep the dashboard dark even with a light terminal profile.
const BACKGROUND: Color = Color::Indexed(233);
const PANEL: Color = Color::Indexed(235);
const BORDER: Color = Color::Indexed(60);
const TEXT: Color = Color::Indexed(255);
const MUTED: Color = Color::Indexed(250);
const CYAN: Color = Color::Indexed(87);
const VIOLET: Color = Color::Indexed(147);
const AMBER: Color = Color::Indexed(221);
const RED: Color = Color::Indexed(210);

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { CYAN } else { BORDER }))
        .title_style(Style::new().fg(if focused { CYAN } else { MUTED }))
        .style(Style::new().bg(PANEL).fg(TEXT))
}

fn highlight() -> Style {
    Style::new()
        .bg(CYAN)
        .fg(BACKGROUND)
        .add_modifier(Modifier::BOLD)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Focus {
    #[default]
    Boards,
    Actions,
    Logs,
}

impl Focus {
    fn shift(self, right: bool) -> Self {
        match (self, right) {
            (Self::Boards, true) | (Self::Logs, false) => Self::Actions,
            (Self::Actions, true) | (Self::Boards, false) => Self::Logs,
            (Self::Logs, true) | (Self::Actions, false) => Self::Boards,
        }
    }
}

#[derive(Default)]
struct Dashboard {
    targets: Vec<Target>,
    selected: usize,
    focus: Focus,
    action: usize,
    activation_selected: bool,
    session: Option<Session>,
    status: Option<StatusResponse>,
    connection: String,
    discovery: String,
    logs: VecDeque<LogRow>,
    filter: Filter,
    scroll: usize,
    page: usize,
    workflow: update::Workflow,
    log_boot: u64,
    log_sequence: u64,
}

impl Dashboard {
    fn select(&mut self, index: usize, timeout: Duration) {
        self.workflow.clear();
        self.selected = index;
        self.session = self
            .targets
            .get(index)
            .map(|target| Session::start(target.address, timeout));
        self.status = None;
        self.connection = "Connecting…".into();
        self.logs.clear();
        self.log_boot = 0;
        self.log_sequence = 0;
        self.scroll = 0;
    }

    fn push_log(&mut self, severity: Option<LogSeverity>, text: String) {
        if self.scroll > 0 && self.filter.includes(severity) {
            self.scroll += 1;
        }
        if self.logs.len() == LOG_HISTORY {
            self.logs.pop_front();
        }
        self.logs.push_back(LogRow { severity, text });
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Status(status) => {
                self.status = Some(*status);
                self.connection = "Connected".into();
            }
            Update::Retry(error) => {
                self.status = None;
                self.connection = format!("Reconnecting · {}", error.escape_debug());
            }
            Update::Logs(batch) => {
                let same_boot = self.log_boot == batch.boot_id && !batch.reset;
                if batch.reset || (self.log_boot != 0 && !same_boot) {
                    self.push_log(None, "── Device restarted or log cursor reset ──".into());
                }
                if !same_boot {
                    self.log_boot = batch.boot_id;
                    self.log_sequence = 0;
                }
                if batch.dropped && (!same_boot || batch.next_sequence > self.log_sequence) {
                    self.push_log(None, "── Device overwrote older log entries ──".into());
                }
                for entry in &batch.entries {
                    if entry.sequence <= self.log_sequence {
                        continue;
                    }
                    self.log_sequence = entry.sequence;
                    let severity = match entry.severity {
                        LogSeverity::Info => "INFO",
                        LogSeverity::Warn => "WARN",
                        LogSeverity::Error => "ERROR",
                        _ => "?",
                    };
                    self.push_log(
                        Some(entry.severity),
                        format!(
                            "#{:<6} {:>9}ms  {:<5}  {}",
                            entry.sequence,
                            entry.uptime_ms,
                            severity,
                            entry.message.escape_debug()
                        ),
                    );
                }
            }
        }
    }

    fn actions(&self, direct: bool) -> Vec<(KeyCode, String)> {
        let mut actions = vec![(KeyCode::Char('u'), "Firmware images".into())];
        if self.workflow.can_activate() {
            actions.insert(0, (KeyCode::Char('a'), "Review activation".into()));
        }
        actions.push((
            KeyCode::Char('f'),
            format!("Log filter · {}", self.filter.label()),
        ));
        actions.push((KeyCode::End, "Follow live logs".into()));
        if !direct {
            actions.push((KeyCode::Char('r'), "Scan for boards".into()));
        }
        actions.push((KeyCode::Char('q'), "Quit".into()));
        actions
    }

    fn pause_monitor(&mut self) {
        if self.workflow.busy() {
            self.focus = Focus::Logs;
            self.session = None;
            self.connection = "Update in progress".into();
        }
    }

    fn draw_firmware(&self, frame: &mut Frame<'_>, area: Rect) {
        if !self.workflow.enabled() {
            return;
        }
        if let Some((images, selected)) = self.workflow.images() {
            let shell = panel(
                format!(" Firmware library · {}/{} ", selected + 1, images.len()),
                true,
            );
            let [list, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                .areas(shell.inner(area));
            frame.render_widget(shell, area);
            let rows = images.iter().map(|(path, version)| {
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                Line::from(vec![
                    Span::raw(version.escape_debug().to_string()),
                    Span::raw(format!("  ·  {}", name.escape_debug())),
                ])
            });
            let mut state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_symbol("▸ ")
                    .highlight_style(highlight()),
                list,
                &mut state,
            );
            frame.render_widget(
                Line::styled(
                    "↑↓ Choose   Enter Upload and verify   Esc Back",
                    Style::new().fg(MUTED),
                ),
                hint,
            );
            return;
        }
        let confirming = self.workflow.confirming();
        let shell = panel(
            if confirming {
                " Confirm activation "
            } else {
                " Firmware update "
            },
            confirming || self.workflow.busy(),
        );
        let inner = shell.inner(area);
        frame.render_widget(shell, area);
        if confirming {
            let [message, buttons] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
            frame.render_widget(
                Paragraph::new(self.workflow.text()).wrap(Wrap { trim: true }),
                message,
            );
            let choice = |selected| {
                if selected {
                    highlight()
                } else {
                    Style::new().fg(AMBER)
                }
            };
            frame.render_widget(
                Line::from(vec![
                    Span::styled(" Cancel ", choice(!self.activation_selected)),
                    Span::raw("  "),
                    Span::styled(" Activate & reboot ", choice(self.activation_selected)),
                    Span::styled("   ←→ Choose   Enter Confirm", Style::new().fg(MUTED)),
                ]),
                buttons,
            );
        } else if let Some((written, total)) = self.workflow.write_progress() {
            frame.render_widget(
                Gauge::default()
                    .ratio(f64::from(written) / f64::from(total.max(1)))
                    .label(format!("Writing flash  ·  {written}/{total} bytes"))
                    .gauge_style(Style::new().fg(CYAN).bg(PANEL)),
                inner,
            );
        } else {
            frame.render_widget(
                Paragraph::new(self.workflow.text()).wrap(Wrap { trim: true }),
                inner,
            );
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, direct: bool) {
        frame.render_widget(
            Block::default().style(Style::new().bg(BACKGROUND).fg(TEXT)),
            frame.area(),
        );
        let firmware_height = if self.workflow.picking() {
            (frame.area().height / 2).clamp(5, 14)
        } else if self.workflow.confirming() {
            7
        } else if self.workflow.enabled() {
            4
        } else {
            0
        };
        let [header, body, update, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(firmware_height),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        let [sidebar, board] = Layout::horizontal([
            Constraint::Length((frame.area().width / 3).min(36)),
            Constraint::Min(0),
        ])
        .areas(body);
        let [devices, actions] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(8)]).areas(sidebar);
        let [status, logs] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(board);

        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "◈ ESP32  /  DEVICE CONTROL",
                    Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Line::styled(&self.discovery, Style::new().fg(VIOLET)),
            ]),
            header,
        );
        let names = self.targets.iter().map(|target| target.name.as_str());
        let mut selected =
            ListState::default().with_selected((!self.targets.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(
            List::new(names)
                .block(panel(" Boards ", self.focus == Focus::Boards))
                .highlight_symbol("▸ ")
                .highlight_style(if self.focus == Focus::Boards {
                    highlight()
                } else {
                    Style::new().fg(VIOLET)
                }),
            devices,
            &mut selected,
        );
        let options = self.actions(direct);
        self.action = self.action.min(options.len() - 1);
        let mut selected_action = ListState::default().with_selected(
            (self.focus == Focus::Actions && !self.workflow.busy()).then_some(self.action),
        );
        frame.render_stateful_widget(
            List::new(options.into_iter().map(|(_, label)| label))
                .style(Style::new().fg(if self.workflow.busy() { MUTED } else { TEXT }))
                .block(panel(" Actions ", self.focus == Focus::Actions))
                .highlight_symbol("▸ ")
                .highlight_style(highlight()),
            actions,
            &mut selected_action,
        );

        let mut details = Vec::new();
        if let Some(target) = self.targets.get(self.selected) {
            details.push(format!("{}  ·  {}", target.address, self.connection));
        } else {
            details.push("No board selected".into());
        }
        if let Some(status) = &self.status {
            details.extend([
                format!(
                    "{}  ·  {}",
                    status.device_id.escape_debug(),
                    status.chip.escape_debug()
                ),
                format!(
                    "{}  ·  firmware {}",
                    status.application.escape_debug(),
                    status.firmware_version.escape_debug()
                ),
                format!(
                    "{}  ·  uptime {}s  ·  OTA {}",
                    status.management_state.escape_debug(),
                    status.uptime_ms / 1000,
                    if status.ota_available {
                        "available"
                    } else {
                        "unavailable"
                    }
                ),
            ]);
        }
        frame.render_widget(
            Paragraph::new(details.into_iter().map(Line::raw).collect::<Vec<_>>())
                .block(panel(" Status ", false)),
            status,
        );

        self.page = usize::from(logs.height.saturating_sub(2));
        let visible: Vec<_> = self
            .logs
            .iter()
            .filter(|row| self.filter.includes(row.severity))
            .collect();
        self.scroll = self.scroll.min(visible.len().saturating_sub(self.page));
        let end = visible.len() - self.scroll;
        let start = end.saturating_sub(self.page);
        let lines = visible[start..end]
            .iter()
            .map(|row| {
                let color = match row.severity {
                    Some(LogSeverity::Error) => RED,
                    Some(LogSeverity::Warn) => AMBER,
                    None => CYAN,
                    _ => TEXT,
                };
                Line::styled(&row.text, Style::default().fg(color))
            })
            .collect::<Vec<_>>();
        let mode = if self.scroll == 0 { "live" } else { "scrolled" };
        frame.render_widget(
            Paragraph::new(lines).block(panel(
                format!(" Logs · {} · {mode} ", self.filter.label()),
                self.focus == Focus::Logs,
            )),
            logs,
        );
        self.draw_firmware(frame, update);
        let controls = if self.workflow.busy() {
            "↑↓ Scroll logs   PgUp/PgDn Page   End Live"
        } else {
            match self.focus {
                Focus::Boards => "←→ Switch pane   ↑↓ Select board   Enter Actions",
                Focus::Actions => "←→ Switch pane   ↑↓ Choose action   Enter Run",
                Focus::Logs => "←→ Switch pane   ↑↓ Scroll logs   Enter Live",
            }
        };
        let shortcuts = if self.workflow.busy() {
            "F Filter   Board selection and quit resume after update"
        } else {
            "U Firmware   A Review activation   F Filter   R Scan   Q Quit"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(controls, Style::new().fg(CYAN)),
                Line::styled(shortcuts, Style::new().fg(MUTED)),
            ]),
            footer,
        );
    }
}

fn scan(interface: Option<Ipv4Addr>) -> JoinHandle<io::Result<discovery::Discovery>> {
    tokio::task::spawn_blocking(move || {
        discovery::discover(interface, Duration::from_millis(discovery::DEFAULT_SCAN_MS))
    })
}

async fn dashboard(
    screen: &mut Screen,
    direct: Option<SocketAddr>,
    interface: Option<Ipv4Addr>,
    timeout: Duration,
    firmware: Option<(std::path::PathBuf, Option<String>)>,
) -> io::Result<()> {
    let mut app = Dashboard {
        workflow: update::Workflow::new(firmware),
        ..Dashboard::default()
    };
    let mut scanning = if let Some(address) = direct {
        app.discovery = "Direct connection".into();
        app.targets.push(Target {
            name: address.to_string(),
            address,
        });
        app.select(0, timeout);
        None
    } else {
        app.discovery = "Scanning the local network…".into();
        Some(scan(interface))
    };
    let mut redraw = true;
    loop {
        for _ in 0..32 {
            match app.workflow.next_log() {
                Some(Ok(batch)) => app.update(Update::Logs(Box::new(batch))),
                Some(Err(error)) => app.push_log(
                    None,
                    format!("Upload logs unavailable · {}", error.escape_debug()),
                ),
                None => break,
            }
            redraw = true;
        }
        let updating = app.workflow.busy();
        redraw |= app.workflow.poll().await;
        if updating && !app.workflow.busy() {
            if app.workflow.can_activate() {
                app.focus = Focus::Actions;
                app.action = 0;
            }
            if let Some(target) = app.targets.get(app.selected) {
                app.session = Some(Session::start(target.address, timeout));
                app.connection = "Reconnecting after update…".into();
            }
        }
        if scanning.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(task) = scanning.take() {
                redraw = true;
                match task.await? {
                    Ok(found) => {
                        let previous = app.targets.get(app.selected);
                        let targets: Vec<_> = found
                            .devices
                            .into_iter()
                            .map(|device| Target {
                                name: device.device_id.escape_debug().to_string(),
                                address: device.address,
                            })
                            .collect();
                        let selection = previous.and_then(|target| {
                            targets.iter().position(|candidate| candidate == target)
                        });
                        app.targets = targets;
                        app.discovery = match app.targets.len() {
                            0 => "No boards found · R to scan again".into(),
                            count if found.limit_reached => {
                                format!("{count} boards found · discovery limit reached")
                            }
                            count => format!("{count} board(s) found"),
                        };
                        match selection {
                            Some(index) => app.selected = index,
                            None => app.select(0, timeout),
                        }
                    }
                    Err(error) => {
                        app.discovery = format!(
                            "Scan failed · {} · R to retry",
                            error.to_string().escape_debug()
                        )
                    }
                }
            }
        }
        // Keep keyboard and rendering responsive even under a sustained log flood.
        for _ in 0..32 {
            let update = app
                .session
                .as_mut()
                .and_then(|session| session.updates.try_recv().ok());
            let Some(update) = update else { break };
            app.update(update);
            redraw = true;
        }
        if redraw {
            screen.draw(|frame| app.draw(frame, direct.is_some()))?;
            redraw = false;
        }
        if event::poll(Duration::from_millis(1))? {
            let event = event::read()?;
            redraw = true;
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Release {
                    if app.workflow.picking() {
                        match key.code {
                            KeyCode::Up => app.workflow.previous_image(),
                            KeyCode::Down => app.workflow.next_image(),
                            KeyCode::Esc => app.workflow.cancel_picker(),
                            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                                app.workflow.verify_selected(
                                    app.targets.get(app.selected).map(|target| target.address),
                                    app.status.as_ref(),
                                    timeout,
                                );
                                app.pause_monitor();
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if app.workflow.confirming() {
                        match key.code {
                            KeyCode::Left => app.activation_selected = false,
                            KeyCode::Right => app.activation_selected = true,
                            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                                if app.activation_selected {
                                    app.workflow.activate(timeout);
                                } else {
                                    app.workflow.cancel_confirmation();
                                }
                            }
                            KeyCode::Char('y')
                                if key.kind == KeyEventKind::Press && key.modifiers.is_empty() =>
                            {
                                app.workflow.activate(timeout);
                            }
                            KeyCode::Esc | KeyCode::Char('n') => app.workflow.cancel_confirmation(),
                            _ => {}
                        }
                        app.pause_monitor();
                        continue;
                    }
                    let busy = app.workflow.busy();
                    let command =
                        if !busy && key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                            match app.focus {
                                Focus::Boards => {
                                    app.focus = Focus::Actions;
                                    continue;
                                }
                                Focus::Actions => app.actions(direct.is_some())[app.action].0,
                                Focus::Logs => KeyCode::End,
                            }
                        } else {
                            key.code
                        };
                    match command {
                        KeyCode::Char('q') | KeyCode::Esc if !busy => return Ok(()),
                        KeyCode::Char('c')
                            if !busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            return Ok(())
                        }
                        KeyCode::Char('u')
                            if !busy
                                && scanning.is_none()
                                && key.kind == KeyEventKind::Press
                                && key.modifiers.is_empty() =>
                        {
                            app.workflow.verify(
                                app.targets.get(app.selected).map(|target| target.address),
                                app.status.as_ref(),
                                timeout,
                            );
                            app.pause_monitor();
                        }
                        KeyCode::Char('a')
                            if !busy
                                && scanning.is_none()
                                && key.kind == KeyEventKind::Press
                                && key.modifiers.is_empty() =>
                        {
                            app.activation_selected = false;
                            app.workflow.ask_activation();
                        }
                        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab
                            if !busy =>
                        {
                            app.focus = app
                                .focus
                                .shift(matches!(key.code, KeyCode::Right | KeyCode::Tab));
                        }
                        KeyCode::Up if !busy && app.focus == Focus::Boards && app.selected > 0 => {
                            app.select(app.selected - 1, timeout);
                        }
                        KeyCode::Down
                            if !busy
                                && app.focus == Focus::Boards
                                && app.selected + 1 < app.targets.len() =>
                        {
                            app.select(app.selected + 1, timeout);
                        }
                        KeyCode::Up if !busy && app.focus == Focus::Actions => {
                            app.action = app.action.saturating_sub(1);
                        }
                        KeyCode::Down if !busy && app.focus == Focus::Actions => {
                            app.action =
                                (app.action + 1).min(app.actions(direct.is_some()).len() - 1);
                        }
                        KeyCode::Char('r') if !busy && direct.is_none() && scanning.is_none() => {
                            app.discovery = "Scanning the local network…".into();
                            scanning = Some(scan(interface));
                        }
                        KeyCode::Char('f') => {
                            app.filter = app.filter.next();
                            app.scroll = 0;
                        }
                        KeyCode::Up if busy || app.focus == Focus::Logs => app.scroll += 1,
                        KeyCode::Down if busy || app.focus == Focus::Logs => {
                            app.scroll = app.scroll.saturating_sub(1)
                        }
                        KeyCode::PageUp => app.scroll += app.page,
                        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(app.page),
                        KeyCode::End => app.scroll = 0,
                        _ => {}
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
}
