//! Native Ratatui frontend over the shared owned debug session.

use std::{
    fs,
    io::{self, Stdout},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use samdebug_core::{CancellationToken, ErrorCategory, SamdebugError, SamdebugResult};
use samdebug_debug::{
    Breakpoint, DisassemblyInstruction, OwnedDebugSession, RegisterValue, SessionEvent,
    SessionState, StackFrame, Variable,
};

const REGISTER_NAMES: &[&str] = &[
    "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr",
    "pc", "xpsr",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Source,
    Disassembly,
    Stack,
    Locals,
    Registers,
    Breakpoints,
    Output,
    Logs,
}

impl Pane {
    const ALL: [Self; 8] = [
        Self::Source,
        Self::Disassembly,
        Self::Stack,
        Self::Locals,
        Self::Registers,
        Self::Breakpoints,
        Self::Output,
        Self::Logs,
    ];

    fn next(self) -> Self {
        let index = Self::ALL.iter().position(|pane| *pane == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    Breakpoint(String),
    ConfirmLoad(String),
    Help,
}

#[derive(Debug)]
struct App {
    probe_serial: String,
    state: SessionState,
    generation: u64,
    active: Pane,
    input: InputMode,
    status: String,
    stack: Vec<StackFrame>,
    variables: Vec<Variable>,
    registers: Vec<RegisterValue>,
    breakpoints: Vec<Breakpoint>,
    disassembly: Vec<DisassemblyInstruction>,
    source: Vec<String>,
    source_path: Option<String>,
    source_line: Option<u32>,
    output: Vec<String>,
    logs: Vec<String>,
    should_quit: bool,
}

impl App {
    fn new(probe_serial: String, session: &OwnedDebugSession) -> Self {
        Self {
            probe_serial,
            state: session.state(),
            generation: session.generation(),
            active: Pane::Source,
            input: InputMode::Normal,
            status: "Connected. Press ? for help.".into(),
            stack: Vec::new(),
            variables: Vec::new(),
            registers: Vec::new(),
            breakpoints: Vec::new(),
            disassembly: Vec::new(),
            source: Vec::new(),
            source_path: None,
            source_line: None,
            output: Vec::new(),
            logs: Vec::new(),
            should_quit: false,
        }
    }

    fn process_events(&mut self, events: Vec<SessionEvent>) -> bool {
        let mut stopped = false;
        for event in events {
            match event {
                SessionEvent::State { current, .. } => self.state = current,
                SessionEvent::Stopped { reason, frame, .. } => {
                    self.state = SessionState::Halted;
                    self.status = format!("Stopped: {reason}");
                    if let Some(frame) = frame {
                        self.source_path.clone_from(&frame.file);
                        self.source_line = frame.line;
                    }
                    stopped = true;
                }
                SessionEvent::Running { .. } => {
                    self.state = SessionState::Running;
                    self.status = "Running".into();
                }
                SessionEvent::TargetOutput { text, .. } => push_bounded(&mut self.output, &text),
                SessionEvent::Log { source, text, .. } => {
                    push_bounded(&mut self.logs, &format!("[{source}] {text}"));
                }
                SessionEvent::SessionError { code, message, .. } => {
                    self.status = format!("{code}: {message}");
                }
                SessionEvent::Loaded { verified, .. } => {
                    self.status = if verified {
                        "Firmware loaded and verified".into()
                    } else {
                        "Firmware load finished without verification".into()
                    };
                }
                SessionEvent::Progress {
                    operation,
                    completed,
                    total,
                    ..
                } => self.status = format!("{operation}: {completed}/{total}"),
                SessionEvent::Cancelled { operation, .. } => {
                    self.status = format!("Cancelled {operation}");
                }
                _ => {}
            }
        }
        stopped
    }

    fn refresh(&mut self, session: &mut OwnedDebugSession) {
        if session.state() != SessionState::Halted {
            return;
        }
        match session.stack_frames(0, 32) {
            Ok(frames) => {
                self.stack = frames;
                if let Some(frame) = self.stack.first() {
                    self.source_path = frame.file.clone();
                    self.source_line = frame.line;
                }
            }
            Err(error) => self.status = error.to_string(),
        }
        self.variables = session.variables(0).unwrap_or_else(|error| {
            self.logs.push(format!("[samdebug] {error}"));
            Vec::new()
        });
        let requested = REGISTER_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        self.registers = session.registers(&requested).unwrap_or_else(|error| {
            self.logs.push(format!("[samdebug] {error}"));
            Vec::new()
        });
        if let Some(address) = self.stack.first().and_then(|frame| frame.address) {
            self.disassembly = session.disassemble(address, 96).unwrap_or_else(|error| {
                self.logs.push(format!("[samdebug] {error}"));
                Vec::new()
            });
        }
        self.source = self
            .source_path
            .as_deref()
            .and_then(read_source)
            .unwrap_or_default();
    }

    fn handle_key(&mut self, key: KeyEvent, session: &mut OwnedDebugSession) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match &mut self.input {
            InputMode::Breakpoint(value) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Enter => {
                    let location = std::mem::take(value);
                    self.input = InputMode::Normal;
                    if location.is_empty() {
                        self.status = "Breakpoint location cannot be empty".into();
                    } else {
                        match session.insert_breakpoint(&location, false) {
                            Ok(point) => {
                                self.status = format!("Breakpoint {} inserted", point.id);
                                self.breakpoints.push(point);
                            }
                            Err(error) => self.status = error.to_string(),
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(character) if !character.is_control() => value.push(character),
                _ => {}
            },
            InputMode::ConfirmLoad(value) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Enter => {
                    let authorization = std::mem::take(value);
                    self.input = InputMode::Normal;
                    let expected = format!("firmware.load:{}", self.probe_serial);
                    if firmware_authorization_matches(&self.probe_serial, &authorization) {
                        self.apply(session.load_firmware(&authorization), "Firmware loaded");
                        self.refresh(session);
                    } else {
                        self.status = format!("Authorization rejected; expected {expected}");
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(character) if !character.is_control() => value.push(character),
                _ => {}
            },
            InputMode::Help => self.input = InputMode::Normal,
            InputMode::Normal => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('?') => self.input = InputMode::Help,
                KeyCode::Tab => self.active = self.active.next(),
                KeyCode::Char('c') => self.apply(session.continue_target(), "Running"),
                KeyCode::Char('h') => {
                    let result = session.halt().map(|_| ());
                    self.apply(result, "Halted");
                    self.refresh(session);
                }
                KeyCode::Char('s') => {
                    let result = session.step().map(|_| ());
                    self.apply(result, "Stepped");
                    self.refresh(session);
                }
                KeyCode::Char('n') => {
                    let result = session.next_target().map(|_| ());
                    self.apply(result, "Next complete");
                    self.refresh(session);
                }
                KeyCode::Char('r') => {
                    self.apply(session.reset_halt(), "Reset and halted");
                    self.refresh(session);
                }
                KeyCode::Char('l') => self.input = InputMode::ConfirmLoad(String::new()),
                KeyCode::Char('b') => self.input = InputMode::Breakpoint(String::new()),
                KeyCode::Char('d') => {
                    if let Some(point) = self.breakpoints.pop() {
                        self.apply(session.remove_breakpoint(&point.id), "Breakpoint removed");
                    }
                }
                _ => {}
            },
        }
    }

    fn apply(&mut self, result: SamdebugResult<()>, success: &str) {
        self.status = match result {
            Ok(()) => success.into(),
            Err(error) => error.to_string(),
        };
    }
}

/// Runs the native debugger TUI until the user exits or the session fails.
pub fn run(
    mut session: OwnedDebugSession,
    probe_serial: String,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    let watcher = CancellationWatcher::start(cancellation, session.cancellation_controller());
    let mut stdout = io::stdout();
    if let Err(error) = enable_raw_mode() {
        return Err(terminal_error(error));
    }
    let cleanup = complete_terminal_setup(CrosstermRestorer, || {
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
    })?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(terminal_error)?;
    terminal.clear().map_err(terminal_error)?;
    let result = run_loop(&mut terminal, &mut session, probe_serial, cancellation);
    let _ = session.stop();
    drop(terminal);
    drop(cleanup);
    drop(watcher);
    result
}

#[derive(Debug)]
struct CancellationWatcher {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CancellationWatcher {
    fn start(
        cancellation: &CancellationToken,
        controller: samdebug_debug::DebugCancellationController,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let external = cancellation.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                if external.is_cancelled() {
                    controller.cancel();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for CancellationWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    session: &mut OwnedDebugSession,
    probe_serial: String,
    cancellation: &CancellationToken,
) -> SamdebugResult<()> {
    let mut app = App::new(probe_serial, session);
    app.process_events(session.take_events());
    app.refresh(session);
    while !app.should_quit {
        if cancellation.is_cancelled() {
            session.cancellation_controller().cancel();
            app.process_events(session.take_events());
            return Err(SamdebugError::new(
                ErrorCategory::Interrupted,
                "INTERRUPTED",
                "debug session interrupted",
            ));
        }
        terminal
            .draw(|frame| render(frame, &app))
            .map_err(terminal_error)?;
        if event::poll(Duration::from_millis(50)).map_err(terminal_error)?
            && let Event::Key(key) = event::read().map_err(terminal_error)?
        {
            app.handle_key(key, session);
        }
        let stopped = app.process_events(session.take_events());
        if stopped {
            app.refresh(session);
        }
        if session.state() == SessionState::Idle {
            app.should_quit = true;
        }
    }
    Ok(())
}

trait TerminalRestorer: std::fmt::Debug {
    fn restore(&mut self);
}

#[derive(Debug)]
struct CrosstermRestorer;

impl TerminalRestorer for CrosstermRestorer {
    fn restore(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[derive(Debug)]
struct TerminalCleanup<R: TerminalRestorer> {
    restorer: R,
}

impl<R: TerminalRestorer> TerminalCleanup<R> {
    const fn new(restorer: R) -> Self {
        Self { restorer }
    }
}

fn complete_terminal_setup<R, F>(restorer: R, setup: F) -> SamdebugResult<TerminalCleanup<R>>
where
    R: TerminalRestorer,
    F: FnOnce() -> io::Result<()>,
{
    // The guard must exist before entering the alternate screen. If a compound
    // crossterm setup fails after applying only some commands, dropping it
    // restores raw mode, mouse capture, and the original screen.
    let cleanup = TerminalCleanup::new(restorer);
    setup().map_err(terminal_error)?;
    Ok(cleanup)
}

impl<R: TerminalRestorer> Drop for TerminalCleanup<R> {
    fn drop(&mut self) {
        self.restorer.restore();
    }
}

#[allow(clippy::too_many_lines)]
fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 70 || area.height < 20 {
        frame.render_widget(
            Paragraph::new(format!(
                "samdebug — {:?}\nTerminal is too small ({}x{}). Resize to at least 70x20.\n\nq quit | ? help",
                app.state, area.width, area.height
            ))
            .block(Block::default().title("Small terminal").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(9),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "samdebug ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "probe={} generation={} state={:?} — {}",
                app.probe_serial, app.generation, app.state, app.status
            )),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Status")),
        rows[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(columns[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(26),
            Constraint::Percentage(26),
            Constraint::Percentage(26),
            Constraint::Percentage(22),
        ])
        .split(columns[1]);
    render_source(frame, app, left[0]);
    render_disassembly(frame, app, left[1]);
    render_lines(
        frame,
        "Stack",
        Pane::Stack,
        app,
        right[0],
        &stack_lines(&app.stack),
    );
    render_lines(
        frame,
        "Locals",
        Pane::Locals,
        app,
        right[1],
        &variable_lines(&app.variables),
    );
    render_lines(
        frame,
        "Registers",
        Pane::Registers,
        app,
        right[2],
        &register_lines(&app.registers),
    );
    render_lines(
        frame,
        "Breakpoints",
        Pane::Breakpoints,
        app,
        right[3],
        &app.breakpoints
            .iter()
            .map(|point| format!("{} {}", point.id, point.location))
            .collect::<Vec<_>>(),
    );
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[2]);
    render_lines(
        frame,
        "Target output",
        Pane::Output,
        app,
        bottom[0],
        &app.output,
    );
    render_lines(
        frame,
        "OpenOCD / GDB logs",
        Pane::Logs,
        app,
        bottom[1],
        &app.logs,
    );
    frame.render_widget(
        Paragraph::new("Tab panes | c continue | h halt | s step | n next | r reset | b breakpoint | d delete | l load | ? help | q quit"),
        rows[3],
    );
    render_overlay(frame, app, area);
}

fn render_source(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let current = app.source_line.unwrap_or(0);
    let items = if app.source.is_empty() {
        vec![ListItem::new("Source unavailable; see disassembly below.")]
    } else {
        let visible = usize::from(area.height.saturating_sub(2)).max(1);
        let current_index = usize::try_from(current.saturating_sub(1)).unwrap_or(0);
        let start = current_index
            .saturating_sub(visible / 2)
            .min(app.source.len().saturating_sub(visible));
        app.source
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, text)| {
                let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
                let line = format!("{number:>5}  {text}");
                if number == current {
                    ListItem::new(line).style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    ListItem::new(line)
                }
            })
            .collect()
    };
    let title = app.source_path.as_deref().unwrap_or("Source");
    frame.render_widget(
        List::new(items).block(active_block(title, app.active == Pane::Source)),
        area,
    );
}

fn render_disassembly(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let current = app.stack.first().and_then(|frame| frame.address);
    let items = app
        .disassembly
        .iter()
        .map(|instruction| {
            let text = format!(
                "0x{:08x}  {:<12} {}",
                instruction.address,
                instruction.opcodes.as_deref().unwrap_or(""),
                instruction.instruction
            );
            if Some(instruction.address) == current {
                ListItem::new(text).style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ListItem::new(text)
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(active_block("Disassembly", app.active == Pane::Disassembly)),
        area,
    );
}

fn render_lines(
    frame: &mut Frame<'_>,
    title: &str,
    pane: Pane,
    app: &App,
    area: Rect,
    lines: &[String],
) {
    let text = if lines.is_empty() {
        "—".into()
    } else {
        lines.join("\n")
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(active_block(title, app.active == pane))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (title, text) = match &app.input {
        InputMode::Normal => return,
        InputMode::Breakpoint(value) => ("Insert breakpoint", format!("Location: {value}_")),
        InputMode::ConfirmLoad(value) => (
            "Confirm firmware load",
            format!(
                "Type exactly:\nfirmware.load:{}\n\nAuthorization: {value}_\nEnter confirm | Esc cancel",
                app.probe_serial,
            ),
        ),
        InputMode::Help => (
            "Keyboard help",
            "Tab: next pane\nc/h: continue/halt\ns/n: step/next\nr: reset-halt\nb/d: insert/delete breakpoint\nl: authorized managed firmware load\nq: stop session and exit\n\nAny key closes help."
                .into(),
        ),
    };
    let popup = centered_rect(62, 40, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn active_block(title: &str, active: bool) -> Block<'_> {
    let style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn stack_lines(frames: &[StackFrame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| {
            format!(
                "#{} {} {}:{}",
                frame.index,
                frame.function,
                frame.file.as_deref().unwrap_or("?"),
                frame
                    .line
                    .map_or_else(|| "?".into(), |line| line.to_string())
            )
        })
        .collect()
}

fn variable_lines(values: &[Variable]) -> Vec<String> {
    values
        .iter()
        .map(|value| format!("{} = {}", value.name, value.value))
        .collect()
}

fn register_lines(values: &[RegisterValue]) -> Vec<String> {
    values
        .iter()
        .map(|value| format!("{:<5} {}", value.name, value.value))
        .collect()
}

fn read_source(path: &str) -> Option<Vec<String>> {
    let path = Path::new(path);
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return None;
    }
    Some(
        fs::read_to_string(path)
            .ok()?
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

fn push_bounded(lines: &mut Vec<String>, text: &str) {
    lines.extend(text.lines().map(str::to_owned));
    if lines.len() > 1000 {
        lines.drain(..lines.len() - 1000);
    }
}

fn firmware_authorization_matches(serial: &str, authorization: &str) -> bool {
    authorization == format!("firmware.load:{serial}")
}

fn terminal_error(error: impl std::fmt::Display) -> SamdebugError {
    SamdebugError::new(
        ErrorCategory::Debugger,
        "TERMINAL_FAILED",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn fixture() -> App {
        App {
            probe_serial: "ATML123".into(),
            state: SessionState::Halted,
            generation: 7,
            active: Pane::Source,
            input: InputMode::Normal,
            status: "Stopped: breakpoint".into(),
            stack: vec![StackFrame {
                index: 0,
                function: "main".into(),
                file: Some("src/main.c".into()),
                line: Some(2),
                address: Some(0x0040_0100),
            }],
            variables: vec![Variable {
                name: "counter".into(),
                value: "7".into(),
                type_name: Some("int".into()),
            }],
            registers: vec![RegisterValue {
                name: "pc".into(),
                value: "0x00400100".into(),
            }],
            breakpoints: vec![Breakpoint {
                id: "1".into(),
                location: "main".into(),
                enabled: true,
                temporary: false,
            }],
            disassembly: vec![DisassemblyInstruction {
                address: 0x0040_0100,
                function: Some("main".into()),
                offset: Some(0),
                instruction: "push {r7, lr}".into(),
                opcodes: Some("b580".into()),
            }],
            source: vec!["int main(void) {".into(), "  return 0;".into(), "}".into()],
            source_path: Some("src/main.c".into()),
            source_line: Some(2),
            output: vec!["boot".into()],
            logs: vec!["[openocd] ready".into()],
            should_quit: false,
        }
    }

    #[test]
    fn renders_full_and_small_terminal_snapshots() {
        let app = fixture();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let full = format!("{:?}", terminal.backend().buffer());
        for expected in ["samdebug", "main", "counter", "Disassembly", "boot"] {
            assert!(full.contains(expected), "missing {expected}");
        }

        let mut terminal = Terminal::new(TestBackend::new(50, 12)).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        assert!(format!("{:?}", terminal.backend().buffer()).contains("Terminal is too small"));
    }

    #[test]
    fn source_view_keeps_current_execution_line_visible() {
        let mut app = fixture();
        app.source = (1..=100)
            .map(|line| format!("source line {line}"))
            .collect();
        app.source_line = Some(80);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let snapshot = format!("{:?}", terminal.backend().buffer());
        assert!(snapshot.contains("source line 80"));
        assert!(!snapshot.contains("source line 1 "));
    }

    #[derive(Debug)]
    struct CountingRestorer(Arc<AtomicUsize>);

    impl TerminalRestorer for CountingRestorer {
        fn restore(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn terminal_cleanup_runs_on_normal_return_and_panic_unwind() {
        let normal = Arc::new(AtomicUsize::new(0));
        {
            let _cleanup = TerminalCleanup::new(CountingRestorer(Arc::clone(&normal)));
        }
        assert_eq!(normal.load(Ordering::SeqCst), 1);

        let panicking = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&panicking);
        let result = std::panic::catch_unwind(move || {
            let _cleanup = TerminalCleanup::new(CountingRestorer(observed));
            panic!("terminal failure fixture");
        });
        assert!(result.is_err());
        assert_eq!(panicking.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn terminal_cleanup_runs_when_compound_setup_partially_fails() {
        let restores = Arc::new(AtomicUsize::new(0));
        let result = complete_terminal_setup(
            CountingRestorer(Arc::clone(&restores)),
            || -> io::Result<()> { Err(io::Error::other("mouse capture setup failed")) },
        );
        assert!(result.is_err());
        assert_eq!(restores.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pane_and_event_state_are_deterministic() {
        let mut app = fixture();
        assert_eq!(app.active.next(), Pane::Disassembly);
        let stopped = app.process_events(vec![SessionEvent::Running { generation: 7 }]);
        assert!(!stopped);
        assert_eq!(app.state, SessionState::Running);
        let stopped = app.process_events(vec![SessionEvent::Stopped {
            generation: 7,
            reason: "step".into(),
            frame: app.stack.first().cloned(),
        }]);
        assert!(stopped);
        assert_eq!(app.state, SessionState::Halted);
    }

    #[test]
    fn firmware_authorization_is_operation_and_probe_scoped() {
        assert!(firmware_authorization_matches(
            "ATML123",
            "firmware.load:ATML123"
        ));
        assert!(!firmware_authorization_matches(
            "ATML123",
            "firmware.load:OTHER"
        ));
        assert!(!firmware_authorization_matches("ATML123", "flash:ATML123"));
    }
}
