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
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, List, ListState, Paragraph},
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

#[derive(Default)]
struct Dashboard {
    targets: Vec<Target>,
    selected: usize,
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

    fn draw(&mut self, frame: &mut Frame<'_>, direct: bool) {
        let [header, body, update, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(if self.workflow.enabled() { 5 } else { 0 }),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        let [devices, board] =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .areas(body);
        let [status, logs] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(board);

        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "ESP32  /  DEVICE MONITOR",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::raw(&self.discovery),
            ]),
            header,
        );
        let names = self.targets.iter().map(|target| target.name.as_str());
        let mut selected =
            ListState::default().with_selected((!self.targets.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(
            List::new(names)
                .block(Block::bordered().title(" Boards "))
                .highlight_symbol("› ")
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            devices,
            &mut selected,
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
                .block(Block::bordered().title(" Status ")),
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
                    Some(LogSeverity::Error) => Color::Red,
                    Some(LogSeverity::Warn) => Color::Yellow,
                    None => Color::Cyan,
                    _ => Color::Reset,
                };
                Line::styled(&row.text, Style::default().fg(color))
            })
            .collect::<Vec<_>>();
        let mode = if self.scroll == 0 { "live" } else { "scrolled" };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered().title(format!(" Logs · {} · {mode} ", self.filter.label())),
            ),
            logs,
        );
        if self.workflow.enabled() {
            let color = if self.workflow.confirming() {
                Color::Yellow
            } else {
                Color::Cyan
            };
            frame.render_widget(
                Paragraph::new(self.workflow.text())
                    .wrap(ratatui::widgets::Wrap { trim: true })
                    .block(
                        Block::bordered()
                            .title(" Firmware update ")
                            .border_style(Style::default().fg(color)),
                    ),
                update,
            );
        }
        let controls = if self.workflow.busy() || direct {
            "↑/↓ Scroll   PgUp/PgDn Page   End Live"
        } else {
            "←/→ Board   R Scan   ↑/↓ Scroll   PgUp/PgDn Page   End Live"
        };
        let actions = if self.workflow.busy() {
            "F Severity   Board selection and quit resume after update"
        } else {
            "F Severity   U Firmware   Q Quit"
        };
        frame.render_widget(
            Paragraph::new(vec![Line::raw(controls), Line::raw(actions)]),
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
                                if app.workflow.busy() {
                                    app.session = None;
                                    app.connection = "Update in progress".into();
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if app.workflow.confirming() {
                        match key.code {
                            KeyCode::Char('y')
                                if key.kind == KeyEventKind::Press && key.modifiers.is_empty() =>
                            {
                                app.workflow.activate(timeout)
                            }
                            KeyCode::Esc | KeyCode::Char('n') => app.workflow.cancel_confirmation(),
                            _ => {}
                        }
                        if app.workflow.busy() {
                            app.session = None;
                            app.connection = "Update in progress".into();
                        }
                        continue;
                    }
                    let busy = app.workflow.busy();
                    match key.code {
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
                            if app.workflow.busy() {
                                app.session = None;
                                app.connection = "Update in progress".into();
                            }
                        }
                        KeyCode::Char('a')
                            if !busy
                                && scanning.is_none()
                                && key.kind == KeyEventKind::Press
                                && key.modifiers.is_empty() =>
                        {
                            app.workflow.ask_activation()
                        }
                        KeyCode::Left if !busy && app.selected > 0 => {
                            app.select(app.selected - 1, timeout)
                        }
                        KeyCode::Right if !busy && app.selected + 1 < app.targets.len() => {
                            app.select(app.selected + 1, timeout)
                        }
                        KeyCode::Char('r') if !busy && direct.is_none() && scanning.is_none() => {
                            app.discovery = "Scanning the local network…".into();
                            scanning = Some(scan(interface));
                        }
                        KeyCode::Char('f') => {
                            app.filter = app.filter.next();
                            app.scroll = 0;
                        }
                        KeyCode::Up => app.scroll += 1,
                        KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
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
