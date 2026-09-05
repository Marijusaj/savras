//! The embedded side panel: Savras owns the window, draws the panel in a column
//! on one side, and runs your shell or Claude Code in a real terminal beside it.
//! No tmux, no dependency — `svr panel` just works.
//!
//! Savras does not implement a terminal emulator. `portable-pty` provides the
//! pseudo-terminal (ConPTY on Windows) and `vt100` interprets the output into a
//! screen; this module is the glue and the layout.
//!
//! Keystrokes are forwarded to the child as raw bytes rather than decoded and
//! re-encoded, so arrow keys, Ctrl chords, paste and anything else the child
//! understands arrive exactly as the terminal sent them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::ping::Ping;
use crate::ui::{self, Hint};
use crate::watch::Watch;

/// Toggles focus between the working pane and the panel. One byte, so it needs
/// no escape-sequence matching, and the panel is read-only enough that most
/// people never reach for it.
const FOCUS_TOGGLE: u8 = 0x07; // Ctrl-G
const ESC: u8 = 0x1b;

const TICK: Duration = Duration::from_millis(16);
/// Repaint at least this often even when nothing changed, so ages keep ticking.
const REDRAW: Duration = Duration::from_millis(500);
const REFRESH: Duration = Duration::from_secs(2);
/// Columns taken by the divider between the panel and the working pane.
const DIVIDER: u16 = 1;
/// Turns every mouse reporting mode back off.
const MOUSE_OFF: &str = "\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l";

/// Which side of the terminal the panel sits on.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Side {
    Left,
    Right,
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Work,
    Panel,
}

/// Where the program in the working pane came from. It decides what happens
/// when that program exits: leaving the shell you started with closes Savras,
/// but a session you opened from the panel exiting must not take the panel with
/// it — that is how you lose both the panel and the reason it failed.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Origin {
    Initial,
    Opened,
}

/// What a keystroke asked for.
enum Action {
    Nothing,
    Focus(Focus),
    /// Open the selected session in the working pane.
    Open,
    /// Run the last opened session again, after it exited.
    Reopen,
}

/// The program running beside the panel, and the pseudo-terminal it lives in.
///
/// This is a whole unit so it can be *replaced*: opening a session from the
/// panel is dropping one of these and spawning the next.
struct Work {
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Receiver<()>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    origin: Origin,
    /// Set once the child is reaped; `try_wait` must not be asked twice.
    exited: Option<u32>,
}

impl Work {
    fn spawn(
        command: &[String],
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
        origin: Origin,
    ) -> Result<Self> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty = NativePtySystem::default()
            .openpty(size)
            .context("opening a pseudo-terminal")?;

        let child = pty
            .slave
            .spawn_command(build_command(command, cwd)?)
            .context("starting the command for the working pane")?;
        // The child holds its own handle; ours would keep the pty open past its exit.
        drop(pty.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let writer = pty.master.take_writer().context("writing to the pty")?;
        let output = read_thread(
            pty.master.try_clone_reader().context("reading the pty")?,
            Arc::clone(&parser),
        );

        Ok(Work {
            parser,
            master: pty.master,
            writer,
            output,
            child,
            origin,
            exited: None,
        })
    }

    /// The child's exit code, once it has one. Remembered, because a reaped
    /// child cannot be waited on again.
    fn exit_code(&mut self) -> Option<u32> {
        if self.exited.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(status.exit_code());
            }
        }
        self.exited
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.lock().unwrap().set_size(rows, cols);
        Ok(())
    }

    /// What the child currently wants from the terminal's mouse.
    fn mouse(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        let parser = self.parser.lock().unwrap();
        (
            parser.screen().mouse_protocol_mode(),
            parser.screen().mouse_protocol_encoding(),
        )
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        // Never leave a program running against a pty nobody is reading.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn run(
    width: u16,
    side: Side,
    command: Vec<String>,
    jobs_dir: PathBuf,
    ping: Ping,
) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().context("reading the terminal size")?;
    let cols_for_work = work_cols(cols, width);
    if cols_for_work < 20 {
        anyhow::bail!(
            "this terminal is {cols} columns wide — too narrow for a {width}-column panel \
             and a usable pane beside it. Try --width {}, or run plain `svr`.",
            width.saturating_sub(10).max(12)
        );
    }

    let work = Work::spawn(&command, None, cols_for_work, rows, Origin::Initial)?;
    let session = Session {
        jobs_dir,
        width,
        side,
        size: (cols, rows),
        input: stdin_thread(),
        work,
        reopen: None,
        open: None,
        ping,
    };

    let mut terminal = crate::setup_terminal()?;
    let result = event_loop(&mut terminal, session);
    // Leave the terminal's mouse handling as we found it.
    let _ = std::io::stdout().write_all(MOUSE_OFF.as_bytes());
    crate::restore_terminal(&mut terminal)?;
    result
}

struct Session {
    jobs_dir: PathBuf,
    width: u16,
    side: Side,
    size: (u16, u16),
    input: Receiver<Vec<u8>>,
    work: Work,
    /// The last session opened from the panel, so a dead pane can be retried.
    reopen: Option<(Vec<String>, PathBuf)>,
    /// Which session the working pane is showing, so it is never pinged for:
    /// it is asking you in person, on the other half of the screen.
    open: Option<String>,
    ping: Ping,
}

impl Session {
    fn work_size(&self) -> (u16, u16) {
        (work_cols(self.size.0, self.width), self.size.1)
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut session: Session,
) -> Result<()> {
    let mut app = App::new(session.jobs_dir.clone());
    let watch = Watch::start(&session.jobs_dir);
    app.watching = watch.live;
    // What is already on screen when Savras opens is not news.
    session.ping.poll(&app.snapshot, None);

    let mut focus = Focus::Work;
    let mut last_refresh = Instant::now();
    let mut last_draw = Instant::now();
    // Redraw when something actually changed. An always-open panel that
    // repaints sixty times a second for nothing is a battery complaint.
    let mut dirty = true;
    let mut mouse = (
        vt100::MouseProtocolMode::None,
        vt100::MouseProtocolEncoding::Default,
    );
    let mut dead = false;
    let mut exit_code = 0;

    loop {
        // Follow the child in and out of mouse mode. Its request to enable
        // reporting only ever reached our parser, so mirror it outward or the
        // terminal keeps scrolling its own scrollback across both panes.
        let wanted = session.work.mouse();
        if wanted != mouse {
            mouse = wanted;
            let mut out = std::io::stdout();
            out.write_all(mouse_sequence(mouse.0, mouse.1).as_bytes())?;
            out.flush()?;
        }

        if dirty || last_draw.elapsed() >= REDRAW {
            let banner = dead.then_some(exit_code);
            terminal.draw(|frame| draw(frame, &mut app, &session, focus, banner))?;
            last_draw = Instant::now();
            dirty = false;
        }

        // Leaving the shell you started with closes Savras. A session opened
        // from the panel exiting leaves its last screen on display, so you can
        // read why, and the panel stays where it was.
        if let Some(code) = session.work.exit_code() {
            if session.work.origin == Origin::Initial {
                return Ok(());
            }
            if !dead {
                dead = true;
                dirty = true;
                exit_code = code;
            }
        }

        match session.input.recv_timeout(TICK) {
            Ok(bytes) => {
                let bytes = shift_mouse(&bytes, work_offset(session.width, session.side));
                let action = if dead && focus == Focus::Work {
                    dead_pane_key(&bytes, &mut app)
                } else {
                    route(&bytes, focus, &mut app, &mut session.work)?
                };
                match action {
                    Action::Nothing => {}
                    Action::Focus(next) => focus = next,
                    Action::Open | Action::Reopen => {
                        let opened = if matches!(action, Action::Reopen) {
                            reopen(&mut session)
                        } else {
                            open_selected(&mut session, &app)
                        };
                        match opened {
                            Ok(true) => {
                                focus = Focus::Work;
                                dead = false;
                            }
                            Ok(false) => {}
                            Err(e) => app.error = Some(format!("could not open: {e}")),
                        }
                    }
                }
                if app.should_quit {
                    return Ok(());
                }
                dirty = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // The parser already holds whatever the child wrote; these are only
        // wake-ups, so drain them and note that the screen moved.
        while session.work.output.try_recv().is_ok() {
            dirty = true;
        }

        if watch.changed() || last_refresh.elapsed() >= REFRESH {
            app.refresh();
            let open = session.open.clone();
            session.ping.poll(&app.snapshot, open.as_deref());
            last_refresh = Instant::now();
            dirty = true;
        }

        if resize_if_needed(&mut session)? {
            dirty = true;
        }
    }
}

/// Replace the working pane with the selected session. The program that was
/// there is killed; the Claude Code session it was showing is not — those live
/// in Claude Code's daemon, which is why reopening one is just a resume.
fn open_selected(session: &mut Session, app: &App) -> Result<bool> {
    let Some(job) = app.selected_job() else {
        return Ok(false);
    };
    session.open = Some(job.short.clone());
    session.reopen = Some(resume(job));
    reopen(session)
}

fn reopen(session: &mut Session) -> Result<bool> {
    let Some((command, cwd)) = session.reopen.clone() else {
        return Ok(false);
    };
    let (cols, rows) = session.work_size();

    // Spawn before dropping the old one, so a failure leaves the pane intact.
    let work = Work::spawn(&command, Some(&cwd), cols, rows, Origin::Opened)?;
    session.work = work;
    Ok(true)
}

/// Keys for a pane whose program has exited. Its last screen is still on
/// display — usually the error that explains the exit — so the keys are about
/// what to do next, not about typing into a dead terminal.
fn dead_pane_key(bytes: &[u8], app: &mut App) -> Action {
    match bytes {
        [b'\r'] | [b'\n'] => Action::Reopen,
        [b'q'] => {
            app.should_quit = true;
            Action::Nothing
        }
        _ => Action::Nothing,
    }
}

/// How to reopen a session: the command, and where to run it.
fn resume(job: &crate::job::Job) -> (Vec<String>, PathBuf) {
    (job.open_command(), job.cwd.clone())
}

/// Send keystrokes where they belong.
fn route(bytes: &[u8], focus: Focus, app: &mut App, work: &mut Work) -> Result<Action> {
    if bytes.contains(&FOCUS_TOGGLE) {
        return Ok(Action::Focus(match focus {
            Focus::Work => Focus::Panel,
            Focus::Panel => Focus::Work,
        }));
    }

    match focus {
        // Everything reaches the child untouched, which is what makes a full
        // TUI like Claude Code behave normally in the pane.
        Focus::Work => {
            work.writer.write_all(bytes)?;
            work.writer.flush()?;
            Ok(Action::Nothing)
        }
        Focus::Panel => Ok(panel_key(bytes, app)),
    }
}

/// The panel is read-only, so it needs a cursor, a way in, and a way out.
fn panel_key(bytes: &[u8], app: &mut App) -> Action {
    match bytes {
        [b'j'] | [ESC, b'[', b'B'] => app.step(1),
        [b'k'] | [ESC, b'[', b'A'] => app.step(-1),
        [b'g'] => app.jump(false),
        [b'G'] => app.jump(true),
        [b'r'] => app.refresh(),
        [b'\r'] | [b'\n'] => return Action::Open,
        // Esc and q give the terminal back rather than quitting: in a side
        // panel, closing the whole window is rarely what was meant.
        [ESC] | [b'q'] => return Action::Focus(Focus::Work),
        _ => {}
    }
    Action::Nothing
}

fn draw(frame: &mut Frame, app: &mut App, session: &Session, focus: Focus, dead: Option<u32>) {
    let area = frame.area();
    let panel_width = session.width.min(area.width.saturating_sub(DIVIDER + 1));
    let widths = match session.side {
        Side::Left => [
            Constraint::Length(panel_width),
            Constraint::Length(DIVIDER),
            Constraint::Min(1),
        ],
        Side::Right => [
            Constraint::Min(1),
            Constraint::Length(DIVIDER),
            Constraint::Length(panel_width),
        ],
    };
    let chunks = Layout::horizontal(widths).split(area);
    let (work_area, panel_area) = match session.side {
        Side::Left => (chunks[2], chunks[0]),
        Side::Right => (chunks[0], chunks[2]),
    };

    let hint = if focus == Focus::Panel {
        Hint::Focused
    } else {
        Hint::Background
    };
    ui::draw_in(frame, panel_area, app, hint);

    let divider = Paragraph::new(
        std::iter::repeat_n(Line::from("│"), chunks[1].height as usize).collect::<Vec<_>>(),
    )
    .style(Style::default().fg(Color::Indexed(238)));
    frame.render_widget(divider, chunks[1]);

    let parser = session.work.parser.lock().unwrap();
    draw_screen(
        frame,
        work_area,
        parser.screen(),
        focus == Focus::Work && dead.is_none(),
    );

    if let Some(code) = dead {
        draw_dead_banner(frame, work_area, code);
    }
}

/// Say what happened, over the bottom of the dead pane, without covering the
/// message that explains it.
fn draw_dead_banner(frame: &mut Frame, area: Rect, code: u32) {
    if area.height == 0 {
        return;
    }
    let bar = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let text = format!(
        " session exited ({code}) · enter to try again · ctrl-g for the panel · q to quit "
    );
    frame.render_widget(
        Paragraph::new(Line::from(truncate_to(&text, bar.width as usize))).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(179))
                .add_modifier(Modifier::BOLD),
        ),
        bar,
    );
}

fn truncate_to(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

/// Paint the child's screen into our buffer, cell by cell.
fn draw_screen(frame: &mut Frame, area: Rect, screen: &vt100::Screen, focused: bool) {
    let buffer = frame.buffer_mut();
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let Some(target) = buffer.cell_mut((area.x + col, area.y + row)) else {
                continue;
            };

            let contents = cell.contents();
            target.set_symbol(if contents.is_empty() { " " } else { &contents });

            let mut style = Style::default()
                .fg(convert(cell.fgcolor()))
                .bg(convert(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
    }

    // Only show the child's cursor while the child has the keyboard.
    if focused && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < area.height && col < area.width {
            frame.set_cursor_position((area.x + col, area.y + row));
        }
    }
}

fn convert(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Columns left for the working pane once the panel and divider are taken.
pub fn work_cols(total: u16, panel: u16) -> u16 {
    total.saturating_sub(panel + DIVIDER)
}

/// The column the working pane starts at, which is what mouse coordinates in
/// the child are measured from.
fn work_offset(width: u16, side: Side) -> u16 {
    match side {
        Side::Left => width + DIVIDER,
        Side::Right => 0,
    }
}

/// A child asking for mouse reporting is asking the *terminal*, but its request
/// only reaches our parser. Mirror it to the real terminal so the wheel and
/// clicks actually arrive.
fn mouse_sequence(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> String {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    // Clear every mode first; terminals treat these as independent switches.
    let mut out = String::from(MOUSE_OFF);
    match mode {
        M::None => return out,
        M::Press => out.push_str("\x1b[?9h"),
        M::PressRelease => out.push_str("\x1b[?1000h"),
        M::ButtonMotion => out.push_str("\x1b[?1002h"),
        M::AnyMotion => out.push_str("\x1b[?1003h"),
    }
    match encoding {
        E::Default => {}
        E::Utf8 => out.push_str("\x1b[?1005h"),
        E::Sgr => out.push_str("\x1b[?1006h"),
    }
    out
}

/// Shift an SGR mouse report left by `offset` columns, so a click at screen
/// column 60 reaches a child whose own column 0 starts at 45. Reports that land
/// on the panel are dropped rather than sent to the wrong place.
///
/// Only SGR (`ESC [ < b ; x ; y M|m`) is rewritten. It is what modern
/// applications ask for, and guessing at the older encodings — where
/// coordinates are raw bytes with their own limits — would corrupt more than it
/// fixed.
pub fn shift_mouse(bytes: &[u8], offset: u16) -> Vec<u8> {
    if offset == 0 || !bytes.starts_with(b"\x1b[<") {
        return bytes.to_vec();
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let mut out = String::new();
    for report in text.split_inclusive(['M', 'm']) {
        match shift_one(report, offset) {
            Some(shifted) => out.push_str(&shifted),
            None => continue, // landed on the panel
        }
    }
    out.into_bytes()
}

fn shift_one(report: &str, offset: u16) -> Option<String> {
    let body = report.strip_prefix("\x1b[<")?;
    let (body, kind) = body.split_at(body.len().checked_sub(1)?);
    let mut parts = body.split(';');
    let button = parts.next()?;
    let x: u16 = parts.next()?.parse().ok()?;
    let y = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // Columns are 1-based, so the first column of the pane is offset + 1.
    let shifted = x.checked_sub(offset)?;
    if shifted == 0 {
        return None;
    }
    Some(format!("\x1b[<{button};{shifted};{y}{kind}"))
}

/// Returns true if the terminal changed size and the child was told about it.
fn resize_if_needed(session: &mut Session) -> Result<bool> {
    let size = crossterm::terminal::size().unwrap_or(session.size);
    if size == session.size {
        return Ok(false);
    }
    session.size = size;
    let (cols, rows) = session.work_size();
    session.work.resize(cols.max(1), rows)?;
    Ok(true)
}

fn build_command(command: &[String], cwd: Option<&Path>) -> Result<CommandBuilder> {
    let mut builder = match command.split_first() {
        Some((program, args)) => {
            let mut b = CommandBuilder::new(program);
            b.args(args);
            b
        }
        None => CommandBuilder::new(default_shell()),
    };
    match cwd {
        Some(dir) if dir.is_dir() => builder.cwd(dir),
        _ => {
            if let Ok(dir) = std::env::current_dir() {
                builder.cwd(dir);
            }
        }
    }
    // Tell the child what we can actually render.
    builder.env("TERM", "xterm-256color");
    Ok(builder)
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// Feed the parser from the pty on its own thread, and ping the loop so it
/// redraws promptly rather than waiting for the next tick.
fn read_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    parser.lock().unwrap().process(&buf[..n]);
                    if tx.send(()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// Read the terminal's bytes verbatim. Decoding them into key events and
/// re-encoding for the child would lose exactly the sequences a TUI cares about.
fn stdin_thread() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    #[test]
    fn the_working_pane_gets_what_is_left_after_the_panel_and_divider() {
        assert_eq!(work_cols(180, 44), 135);
        assert_eq!(work_cols(80, 44), 35);
    }

    #[test]
    fn a_panel_wider_than_the_terminal_does_not_underflow() {
        assert_eq!(work_cols(30, 44), 0);
    }

    #[test]
    fn the_working_pane_starts_at_column_zero_when_the_panel_is_on_the_right() {
        assert_eq!(work_offset(44, Side::Right), 0);
        assert_eq!(work_offset(44, Side::Left), 45);
    }

    #[test]
    fn a_click_is_shifted_into_the_childs_own_coordinates() {
        // Screen column 60, with the pane starting at 45, is the child's 15.
        assert_eq!(shift_mouse(b"\x1b[<0;60;7M", 45), b"\x1b[<0;15;7M".to_vec());
        assert_eq!(shift_mouse(b"\x1b[<0;60;7m", 45), b"\x1b[<0;15;7m".to_vec());
    }

    #[test]
    fn a_click_on_the_panel_is_not_sent_to_the_child() {
        assert!(shift_mouse(b"\x1b[<0;10;7M", 45).is_empty());
    }

    #[test]
    fn nothing_is_rewritten_when_the_pane_starts_at_the_edge() {
        let scroll = b"\x1b[<64;12;3M";
        assert_eq!(shift_mouse(scroll, 0), scroll.to_vec());
    }

    #[test]
    fn keystrokes_are_never_mistaken_for_mouse_reports() {
        for keys in [
            &b"hello"[..],
            &b"\x1b[A"[..],
            &b"\x1b[200~pasted\x1b[201~"[..],
        ] {
            assert_eq!(shift_mouse(keys, 45), keys.to_vec());
        }
    }

    #[test]
    fn mouse_mode_is_mirrored_and_cleared() {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let on = mouse_sequence(M::ButtonMotion, E::Sgr);
        assert!(on.ends_with("\x1b[?1002h\x1b[?1006h"), "{on:?}");
        let off = mouse_sequence(M::None, E::Default);
        assert!(!off.contains('h'), "nothing should be enabled: {off:?}");
        assert!(off.contains("\x1b[?1006l"), "{off:?}");
    }

    #[test]
    fn a_command_runs_as_given() {
        let built = build_command(&["claude".into(), "--resume".into(), "x".into()], None).unwrap();
        assert!(format!("{built:?}").contains("claude"));
    }

    #[test]
    fn enter_on_the_panel_asks_to_open_the_session() {
        let f = Fixture::new("host-open").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(panel_key(b"\r", &mut app), Action::Open));
        assert!(matches!(panel_key(b"\n", &mut app), Action::Open));
    }

    #[test]
    fn a_dead_pane_offers_a_way_out_instead_of_swallowing_keys() {
        let f = Fixture::new("host-dead").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());

        assert!(matches!(dead_pane_key(b"\r", &mut app), Action::Reopen));
        assert!(!app.should_quit);
        dead_pane_key(b"q", &mut app);
        assert!(
            app.should_quit,
            "q must close a pane that cannot be typed into"
        );
    }

    #[test]
    fn esc_hands_the_keyboard_back_instead_of_quitting() {
        let f = Fixture::new("host-esc").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(
            panel_key(b"\x1b", &mut app),
            Action::Focus(Focus::Work)
        ));
        assert!(!app.should_quit, "the panel must not close the window");
    }

    #[test]
    fn opening_a_session_resumes_it_in_its_own_repository() {
        let f = Fixture::new("host-resume").job(
            "aaa",
            r#"{"state":"working","name":"WO","backend":"daemon","daemonShort":"abc12345",
                "sessionId":"abc-123","cwd":"/tmp/some-repo"}"#,
        );
        let app = App::new(f.0.clone());
        let (command, cwd) = resume(app.selected_job().unwrap());

        assert_eq!(command, ["claude", "attach", "abc12345"]);
        // Resuming in the wrong directory gives a session that cannot see its
        // own repository.
        assert_eq!(cwd, PathBuf::from("/tmp/some-repo"));
    }

    #[test]
    fn a_replaced_pane_is_spawned_before_the_old_one_is_dropped() {
        // Opening a session must not leave an empty pane if the spawn fails.
        let work = Work::spawn(
            &["/nonexistent/program".to_string()],
            None,
            40,
            10,
            Origin::Opened,
        );
        assert!(
            work.is_err(),
            "a missing program should be reported, not run"
        );
    }
}
