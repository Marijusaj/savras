//! The embedded side panel: Savras owns the window, draws the panel in a column
//! on the left, and runs your shell or Claude Code in a real terminal on the
//! right. No tmux, no dependency — `svr panel` just works.
//!
//! Savras does not implement a terminal emulator. `portable-pty` provides the
//! pseudo-terminal (ConPTY on Windows) and `vt100` interprets the output into a
//! screen; this module is the glue and the layout.
//!
//! Keystrokes are forwarded to the child as raw bytes rather than decoded and
//! re-encoded, so arrow keys, Ctrl chords, paste and anything else the child
//! understands arrive exactly as the terminal sent them.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::ui;
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
/// clicks actually arrive — otherwise the terminal keeps scrolling its own
/// scrollback across both panes, which is not what anyone meant.
fn mouse_sequence(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> String {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    // Clear every mode first; terminals treat these as independent switches.
    let mut out = String::from("\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
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

pub fn run(width: u16, side: Side, command: Vec<String>, jobs_dir: PathBuf) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().context("reading the terminal size")?;
    let work_cols = work_cols(cols, width);
    if work_cols < 20 {
        anyhow::bail!(
            "this terminal is {cols} columns wide — too narrow for a {width}-column panel \
             and a usable pane beside it. Try --width {}, or run plain `svr`.",
            width.saturating_sub(10).max(12)
        );
    }

    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols: work_cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a pseudo-terminal")?;

    let mut child = pty
        .slave
        .spawn_command(build_command(&command)?)
        .context("starting the command for the working pane")?;
    // The child holds its own handle; ours would keep the pty open past its exit.
    drop(pty.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, work_cols, 2000)));
    let mut writer = pty.master.take_writer().context("writing to the pty")?;
    let output = read_thread(
        pty.master.try_clone_reader().context("reading the pty")?,
        Arc::clone(&parser),
    );
    let input = stdin_thread();

    let mut terminal = crate::setup_terminal()?;
    let result = event_loop(
        &mut terminal,
        Session {
            jobs_dir,
            width,
            side,
            parser,
            master: pty.master,
            writer: &mut writer,
            input,
            output,
            size: (cols, rows),
        },
        &mut *child,
    );
    // Leave the terminal's mouse handling as we found it.
    let _ = std::io::stdout()
        .write_all(b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l");
    crate::restore_terminal(&mut terminal)?;

    // Do not leave a shell running against a pty nobody is reading.
    let _ = child.kill();
    let _ = child.wait();
    result
}

struct Session<'a> {
    jobs_dir: PathBuf,
    width: u16,
    side: Side,
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: &'a mut Box<dyn Write + Send>,
    input: Receiver<Vec<u8>>,
    output: Receiver<()>,
    size: (u16, u16),
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut session: Session,
    child: &mut dyn portable_pty::Child,
) -> Result<()> {
    let mut app = App::new(session.jobs_dir.clone());
    let watch = Watch::start(&session.jobs_dir);
    app.watching = watch.live;

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

    loop {
        // Follow the child in and out of mouse mode.
        let wanted = {
            let parser = session.parser.lock().unwrap();
            (
                parser.screen().mouse_protocol_mode(),
                parser.screen().mouse_protocol_encoding(),
            )
        };
        if wanted != mouse {
            mouse = wanted;
            let mut out = std::io::stdout();
            out.write_all(mouse_sequence(mouse.0, mouse.1).as_bytes())?;
            out.flush()?;
        }

        if dirty || last_draw.elapsed() >= REDRAW {
            terminal.draw(|frame| draw(frame, &mut app, &session, focus))?;
            last_draw = Instant::now();
            dirty = false;
        }

        // The child exiting is the signal to close the panel with it.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }

        match session.input.recv_timeout(TICK) {
            Ok(bytes) => {
                let bytes = shift_mouse(&bytes, work_offset(session.width, session.side));
                if let Some(new_focus) = route(&bytes, focus, &mut app, session.writer)? {
                    focus = new_focus;
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
        while session.output.try_recv().is_ok() {
            dirty = true;
        }

        if watch.changed() || last_refresh.elapsed() >= REFRESH {
            app.refresh();
            last_refresh = Instant::now();
            dirty = true;
        }

        if resize_if_needed(&mut session)? {
            dirty = true;
        }
    }
}

/// Send keystrokes where they belong. Returns a new focus if it changed.
fn route(
    bytes: &[u8],
    focus: Focus,
    app: &mut App,
    writer: &mut Box<dyn Write + Send>,
) -> Result<Option<Focus>> {
    if bytes.contains(&FOCUS_TOGGLE) {
        return Ok(Some(match focus {
            Focus::Work => Focus::Panel,
            Focus::Panel => Focus::Work,
        }));
    }

    match focus {
        // Everything reaches the child untouched, which is what makes a full
        // TUI like Claude Code behave normally in the pane.
        Focus::Work => {
            writer.write_all(bytes)?;
            writer.flush()?;
            Ok(None)
        }
        Focus::Panel => Ok(panel_key(bytes, app)),
    }
}

/// The panel is read-only, so it needs only a cursor and a way out.
fn panel_key(bytes: &[u8], app: &mut App) -> Option<Focus> {
    match bytes {
        [b'j'] | [ESC, b'[', b'B'] => app.step(1),
        [b'k'] | [ESC, b'[', b'A'] => app.step(-1),
        [b'g'] => app.jump(false),
        [b'G'] => app.jump(true),
        [b'r'] => app.refresh(),
        // Esc and q give the terminal back rather than quitting: in a side
        // panel, closing the whole window is rarely what was meant.
        [ESC] | [b'q'] => return Some(Focus::Work),
        _ => {}
    }
    None
}

fn draw(frame: &mut Frame, app: &mut App, session: &Session, focus: Focus) {
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

    ui::draw_in(frame, panel_area, app, focus == Focus::Panel);

    let divider = Paragraph::new(
        std::iter::repeat_n(Line::from("│"), chunks[1].height as usize).collect::<Vec<_>>(),
    )
    .style(Style::default().fg(Color::Indexed(238)));
    frame.render_widget(divider, chunks[1]);

    let screen = session.parser.lock().unwrap();
    draw_screen(frame, work_area, screen.screen(), focus == Focus::Work);
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

/// Returns true if the terminal changed size and the child was told about it.
fn resize_if_needed(session: &mut Session) -> Result<bool> {
    let size = crossterm::terminal::size().unwrap_or(session.size);
    if size == session.size {
        return Ok(false);
    }
    session.size = size;

    let (cols, rows) = size;
    let work = work_cols(cols, session.width).max(1);
    session.master.resize(PtySize {
        rows,
        cols: work,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    session.parser.lock().unwrap().set_size(rows, work);
    Ok(true)
}

fn build_command(command: &[String]) -> Result<CommandBuilder> {
    let mut builder = match command.split_first() {
        Some((program, args)) => {
            let mut b = CommandBuilder::new(program);
            b.args(args);
            b
        }
        None => CommandBuilder::new(default_shell()),
    };
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
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
        let built = build_command(&["claude".into(), "--resume".into(), "x".into()]).unwrap();
        assert!(format!("{built:?}").contains("claude"));
    }
}
