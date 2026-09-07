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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, Front, GroupBy};
use crate::focus;
use crate::job::Snapshot;
use crate::ping::Ping;
use crate::ui::{self, Hint};
use crate::watch::Watch;

/// Toggles focus between the working pane and the panel. One byte, so it needs
/// no escape-sequence matching, and the panel is read-only enough that most
/// people never reach for it.
const FOCUS_TOGGLE: u8 = 0x07; // Ctrl-G
const ESC: u8 = 0x1b;
/// Redraw everything from scratch. Terminal.app lets you scroll the view of an
/// alternate-screen application, and a scrollbar drag sends no bytes at all —
/// Savras cannot see it happen, so it needs a way to be told to repaint.
const REPAINT: u8 = 0x0c; // Ctrl-L

/// A new tab, from either side of the divider.
///
/// Ctrl-T because cmd-T is the reflex and no terminal can pass it on: Command
/// is not in the terminal's modifier encoding at all, so the terminal keeps
/// every Command chord for its own tabs and the program inside never sees one.
/// A control byte, on the other hand, arrives unchanged everywhere.
const NEW_TAB: u8 = 0x14; // Ctrl-T

/// Set in every pane Savras opens, so a Savras started inside one can tell
/// that it would be the second panel in the same terminal.
const NESTED: &str = "SAVRAS_PANE";

/// Print what the terminal sends for each key, and what Savras makes of it.
///
/// "The chord does nothing in my terminal" has exactly two causes and no way
/// to tell them apart by staring: either the terminal never sent it — which is
/// most of them, since Command chords and unmodified arrows are indistinguish-
/// able from plain arrows in the byte stream — or it sent something Savras
/// does not read. This says which, in the terminal you are actually using.
pub fn keys(switch: Switch) -> Result<()> {
    println!("Press keys to see what this terminal sends. Ctrl-C to stop.\n");
    // Best effort: raw mode is what makes a bare `esc` or a ctrl chord reach
    // us at all, but a pipe has no terminal to put into it and reading the
    // bytes still works — which is how this is tested.
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    let mut input = std::io::stdin();
    let mut buffer = [0u8; 64];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let bytes = &buffer[..n];
        // Raw mode means nothing moves the cursor for us.
        print!("{:<24} {}\r\n", escaped(bytes), meaning(bytes, switch));
        let _ = std::io::stdout().flush();
        // Said after it is shown, so the key that stops this is reported like
        // any other — and a pipe, which arrives all at once, still says
        // everything it was given.
        if bytes.contains(&0x03) {
            break;
        }
    }
    if raw {
        crossterm::terminal::disable_raw_mode()?;
    }
    Ok(())
}

/// Bytes as you would write them in a string, which is how they appear in
/// every other document about terminals.
fn escaped(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            0x1b => "\\e".to_string(),
            0x20..=0x7e => (*b as char).to_string(),
            other => format!("\\x{other:02x}"),
        })
        .collect()
}

/// What Savras would do with those bytes, said in the words the footer uses.
fn meaning(bytes: &[u8], switch: Switch) -> &'static str {
    if bytes.contains(&FOCUS_TOGGLE) {
        "ctrl-g — move the keyboard between the panel and your work"
    } else if bytes.contains(&NEW_TAB) {
        "ctrl-t — open a tab of your own"
    } else if switch.back.is_some_and(|k| bytes.contains(&k)) {
        "the switch key — flip back a tab"
    } else if switch.forward.is_some_and(|k| bytes.contains(&k)) {
        "the switch key — flip forward a tab"
    } else if let Some(delta) = tab_chord(bytes) {
        if delta < 0 {
            "the chord — flip back a tab"
        } else {
            "the chord — flip forward a tab"
        }
    } else if bytes.contains(&REPAINT) {
        "ctrl-l — paint the screen again"
    } else {
        "passed to the program in the pane"
    }
}

/// What the working pane starts on.
///
/// A shell by default, because that is what you get when you open a terminal
/// and Savras has no business deciding otherwise. But an empty shell is rarely
/// what you came for when there are sessions waiting, so `top` opens the first
/// row in the panel — Needs input before Working before Completed, so it is
/// the session most likely to be the reason you started Savras at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Open {
    Shell,
    Top,
    /// A session by name, for the one you always come back to.
    Named(String),
}

impl Open {
    /// Anything that is not `shell` or `top` is a session name. Names are what
    /// you see in the panel, so a name is the obvious thing to type, and there
    /// is nothing else a bare word here could sensibly mean.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "shell" | "none" => Open::Shell,
            "top" | "first" => Open::Top,
            name => Open::Named(name.to_string()),
        }
    }
}

/// Whether this process is running inside a pane of another Savras.
///
/// An environment variable rather than anything cleverer because it is what a
/// pane *is*: a child process, and its children after it. It survives shells,
/// `sudo -E`, ssh into the same box and anything else that keeps the
/// environment, and unsetting it is the escape hatch for whoever really means
/// to nest one.
pub fn nested() -> bool {
    std::env::var_os(NESTED).is_some()
}

/// Flipping tabs, one key each way.
///
/// Control bytes, because they are the only keys *every* terminal delivers
/// unchanged. Terminal.app does not encode modifiers on arrow keys at all —
/// `ctrl-shift-←` arrives there as a bare `ESC [ D`, identical to the arrow
/// the session in the pane wants — so a chord cannot be the only way in.
///
/// W and S because they sit under the left hand where the ctrl key already
/// is, and up/down reads the way the panel's list runs.
///
/// The cost, stated plainly: Ctrl-W is delete-previous-word in a shell and in
/// Claude Code's input, and Savras takes it whenever the panel has a session
/// to flip to — which is nearly always. `--switch` moves both keys, and
/// `--switch off` gives them back. Ctrl-S is free despite its reputation: the
/// flow control that freezes a terminal is turned off by raw mode, which
/// Savras is already in.
pub const SWITCH_BACK: u8 = 0x17; // Ctrl-W
pub const SWITCH_FORWARD: u8 = 0x13; // Ctrl-S

/// The two keys that flip tabs, either of which may be given back to the
/// child with `--switch off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    pub back: Option<u8>,
    pub forward: Option<u8>,
}

impl Default for Switch {
    fn default() -> Self {
        Switch {
            back: Some(SWITCH_BACK),
            forward: Some(SWITCH_FORWARD),
        }
    }
}

impl Switch {
    pub const OFF: Switch = Switch {
        back: None,
        forward: None,
    };

    /// How to name these keys in the footer — `ctrl-w/s`, or `ctrl-o` when
    /// only one of them is bound.
    fn label(&self) -> Option<String> {
        let name = |key: u8| ((key | 0x60) as char).to_string();
        match (self.back, self.forward) {
            (Some(back), Some(forward)) => Some(format!("ctrl-{}/{}", name(back), name(forward))),
            (Some(only), None) | (None, Some(only)) => Some(format!("ctrl-{}", name(only))),
            (None, None) => None,
        }
    }
}

const TICK: Duration = Duration::from_millis(16);
/// Repaint at least this often even when nothing changed, so ages keep ticking.
const REDRAW: Duration = Duration::from_millis(500);
const REFRESH: Duration = Duration::from_secs(2);

/// How long to wait before jogging a new pane's size to make it repaint.
///
/// Long enough for the program to have started and to be handling signals —
/// `claude attach` takes a moment to come up — and short enough that a screen
/// which came up wrong is put right before you have read it.
const REDRAW_AFTER: Duration = Duration::from_millis(1200);
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
    /// Throw the drawn screen away and paint it again.
    Repaint,
    /// `Q` in the panel: quitting takes the working pane with it, so the
    /// second press is the one that does it.
    ConfirmQuit,
    /// Open the selected session in the working pane.
    Open,
    /// Run the last opened session again, after it exited.
    Reopen,
    /// Flip this many tabs along, wrapping.
    Cycle(isize),
    /// Close the tab in front.
    CloseFront,
    /// Close the tab holding the session the panel cursor is on.
    CloseSelected,
    /// Start a parallel agent under the selected session's lead.
    AddAgent,
    /// Open another pane of your own, running what Savras was started with.
    NewTab,
    /// `s` in the panel: group by repository instead of status, or back.
    Regroup,
    /// `d` in the panel: ask whether the selected session should be deleted.
    ConfirmDelete,
    /// The second `d`: delete it.
    Delete,
}

/// A question the panel is holding open, waiting for the same key again.
///
/// Both of these close something for good — the working pane and everything
/// beside it, or a session and its conversation — and a single keystroke is
/// too little to say so with.
#[derive(Clone)]
enum Confirm {
    Quit,
    Delete { short: String, name: String },
}

impl Confirm {
    /// What the footer asks while the question is open. It names the session,
    /// because "delete it" is only safe to answer if you can see which one.
    fn question(&self) -> String {
        match self {
            Confirm::Quit => "Q again to quit Savras · any key stays".to_string(),
            Confirm::Delete { name, .. } => {
                format!("d again to delete {name} for good · any key keeps it")
            }
        }
    }
}

/// The program running beside the panel, and the pseudo-terminal it lives in.
///
/// This is a whole unit so it can be *replaced*: opening a session from the
/// panel is dropping one of these and spawning the next.
struct Work {
    parser: Arc<Mutex<vt100::Parser>>,
    /// Whether the program in the pane asked for focus reporting itself. vt100
    /// does not track mode 1004, so the read thread watches for it.
    wants_focus: Arc<AtomicBool>,
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
        let wants_focus = Arc::new(AtomicBool::new(false));
        let output = read_thread(
            pty.master.try_clone_reader().context("reading the pty")?,
            Arc::clone(&parser),
            Arc::clone(&wants_focus),
        );

        Ok(Work {
            parser,
            wants_focus,
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

/// One open session: its pane, and how to bring it back if it exits.
struct Pane {
    /// The session it is showing, so opening that session again finds this tab
    /// rather than starting a second copy of it. `None` is the shell Savras
    /// started with, which is a tab like any other.
    short: Option<String>,
    reopen: Option<(Vec<String>, PathBuf)>,
    work: Work,
    /// When to jog this pane's size, once, so its program repaints.
    ///
    /// `claude attach` replays a session's transcript as it was *drawn* — at
    /// whatever width the terminal had when the lines were written. Replayed
    /// into a pane of a different width, the old wrapping and the new overlap
    /// and the screen comes up scrambled. Nothing in the byte stream says so,
    /// and the pane is the right size already, so there is no resize to
    /// notice. A size jog a moment after the child starts sends a SIGWINCH,
    /// and Claude Code answers it by drawing the whole screen again — this
    /// time at the size it is actually being shown at.
    redraw: Option<Instant>,
}

/// The sessions you have open, and which one is in front.
///
/// The whole of "tabs" is that a hidden session is *not* stopped: it keeps its
/// pseudo-terminal, its reader thread and its screen, so coming back to it
/// shows the screen you left — scrollback, half-typed message and all — rather
/// than a fresh attach. Everything else here is bookkeeping around that.
///
/// The cost is honest and worth stating: an open tab is a live `claude attach`
/// and a 2000-line scrollback buffer, so tabs are opened deliberately and can
/// be closed. Sessions you have never opened cost nothing.
struct Tabs {
    open: Vec<Pane>,
    current: usize,
}

impl Tabs {
    fn new(work: Work) -> Self {
        Tabs {
            open: vec![Pane {
                short: None,
                reopen: None,
                work,
                redraw: None,
            }],
            current: 0,
        }
    }

    fn front(&self) -> &Pane {
        &self.open[self.current]
    }

    fn front_mut(&mut self) -> &mut Pane {
        &mut self.open[self.current]
    }

    fn work(&self) -> &Work {
        &self.front().work
    }

    fn work_mut(&mut self) -> &mut Work {
        &mut self.front_mut().work
    }

    /// Which session is in front, if it is a session and not the shell.
    fn short(&self) -> Option<&str> {
        self.front().short.as_deref()
    }

    /// Every session with a tab open, the shell excepted — what the panel
    /// marks.
    fn shorts(&self) -> Vec<String> {
        self.open.iter().filter_map(|t| t.short.clone()).collect()
    }

    /// How many panes of your own are open. Never zero: Savras starts with
    /// one and the last tab cannot be closed.
    fn shells(&self) -> usize {
        self.open.iter().filter(|t| t.short.is_none()).count()
    }

    /// Where the nth pane of your own sits in the tab list.
    fn shell_at(&self, n: usize) -> Option<usize> {
        self.open
            .iter()
            .enumerate()
            .filter(|(_, t)| t.short.is_none())
            .map(|(i, _)| i)
            .nth(n)
    }

    /// What the panel should mark as the pane you are in.
    fn front_ref(&self) -> Front {
        match self.short() {
            Some(short) => Front::Session(short.to_string()),
            // Counted in tab order, which is the order they were opened —
            // the same order the panel lists them in.
            None => Front::Shell(
                self.open[..self.current]
                    .iter()
                    .filter(|t| t.short.is_none())
                    .count(),
            ),
        }
    }

    fn position(&self, short: &str) -> Option<usize> {
        self.open
            .iter()
            .position(|t| t.short.as_deref() == Some(short))
    }

    fn go_to(&mut self, index: usize) {
        if index < self.open.len() {
            self.current = index;
        }
    }

    fn push(&mut self, tab: Pane) {
        self.open.push(tab);
        self.current = self.open.len() - 1;
    }

    /// Close a tab and land on a neighbour. The last tab cannot be closed —
    /// that is quitting, and quitting asks first.
    fn close(&mut self, index: usize) -> bool {
        if self.open.len() < 2 || index >= self.open.len() {
            return false;
        }
        self.open.remove(index);
        if self.current >= index && self.current > 0 {
            self.current -= 1;
        }
        true
    }

    /// A hidden pane must be resized too, or it draws at the old size the
    /// moment you flip to it — and a full-screen TUI in it never finds out.
    fn resize_all(&mut self, cols: u16, rows: u16) -> Result<()> {
        for tab in &mut self.open {
            tab.work.resize(cols, rows)?;
        }
        Ok(())
    }
}

/// Everything the side panel needs to start. One struct rather than eight
/// arguments: they are all "how this run was asked for", and a caller passing
/// eight positional values gets two of them the wrong way round eventually.
pub struct Setup {
    pub width: u16,
    pub side: Side,
    /// What runs in the pane, and what a new tab runs. Empty is your shell.
    pub command: Vec<String>,
    pub jobs_dir: PathBuf,
    pub switch: Switch,
    pub open: Open,
    pub group_by: GroupBy,
    pub ping: Ping,
}

pub fn run(setup: Setup) -> Result<()> {
    let Setup {
        width,
        side,
        command,
        jobs_dir,
        switch,
        open,
        group_by,
        ping,
    } = setup;
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
        tabs: Tabs::new(work),
        command,
        switch,
        ping,
    };

    let mut terminal = crate::setup_terminal()?;
    // Ask the terminal to say when it gains and loses focus, so the panel can
    // tell "you are looking at that session" from "you are in another app".
    let _ = std::io::stdout().write_all(focus::ENABLE.as_bytes());
    let result = event_loop(&mut terminal, session, open, group_by);
    // Leave the terminal's mouse and focus handling as we found it.
    let _ = std::io::stdout().write_all(MOUSE_OFF.as_bytes());
    let _ = std::io::stdout().write_all(focus::DISABLE.as_bytes());
    crate::restore_terminal(&mut terminal)?;
    result
}

struct Session {
    jobs_dir: PathBuf,
    width: u16,
    side: Side,
    size: (u16, u16),
    input: Receiver<Vec<u8>>,
    /// The sessions you have open, and which of them is in front. The one in
    /// front is never pinged for: it is asking you in person, on the other
    /// half of the screen.
    tabs: Tabs,
    /// What Savras was started with, and so what a new tab runs: your shell
    /// for plain `svr`, and the command after `--` for anything else. One
    /// rule, and in the default case it is what cmd-T gives you anyway.
    command: Vec<String>,
    /// The keys that flip tabs, or given back to the child by `--switch off`.
    switch: Switch,
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
    open: Open,
    group_by: GroupBy,
) -> Result<()> {
    let mut app = App::new(session.jobs_dir.clone());
    app.set_group_by(group_by);
    // The shell Savras opened with stays as the first tab either way, so
    // whatever this lands on, ctrl-w takes you back to a prompt.
    open_at_startup(&mut session, &mut app, &open);
    // Ctrl-O renders as "ctrl-o": the byte is the letter with its top three
    // bits cleared, so putting them back names the key again.
    app.set_switch(session.switch.label());
    app.set_shell_label(pane_label(&session.command));
    // Starting an agent runs `claude --bg`, which takes long enough that doing
    // it on this thread would visibly stall the panel. The outcome comes back
    // here, since a session that failed to start must say so rather than
    // silently never appearing.
    let (starting, started) = mpsc::channel::<Result<String>>();
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
    // Whether the terminal *window* has you — distinct from `focus` above,
    // which is only about which pane the keyboard is typing into. Unknown
    // counts as away: missing a question because we assumed you were watching
    // is worse than one ping you did not need.
    let mut window_focused = false;
    // A key has been pressed once and the panel is asking whether it was
    // meant. Both questions here close something you cannot get back.
    let mut confirming: Option<Confirm> = None;

    loop {
        // Follow the child in and out of mouse mode. Its request to enable
        // reporting only ever reached our parser, so mirror it outward or the
        // terminal keeps scrolling its own scrollback across both panes.
        let wanted = session.tabs.work().mouse();
        if wanted != mouse {
            mouse = wanted;
            let mut out = std::io::stdout();
            out.write_all(mouse_sequence(mouse.0, mouse.1).as_bytes())?;
            out.flush()?;
        }

        // Whether the tab in front has finished. Asked of the front tab each
        // time round rather than latched, because flipping tabs changes the
        // answer — a dead tab you flip away from must stop being the banner.
        let dead = session.tabs.work_mut().exit_code();

        // Leaving the shell you started with closes Savras — but only while you
        // are looking at it. Exiting it in a background tab leaves a dead tab
        // like any other, rather than taking the panel and your other sessions
        // down from somewhere you cannot see.
        if dead.is_some() && session.tabs.work().origin == Origin::Initial {
            return Ok(());
        }

        if dirty || last_draw.elapsed() >= REDRAW {
            // Which tabs exist, and which one you are in, before it is drawn:
            // opening or closing one has to show up in the same breath as the
            // key that did it, not on the next two-second refresh.
            app.set_tabs(
                session.tabs.front_ref(),
                session.tabs.shorts(),
                session.tabs.shells(),
            );
            terminal
                .draw(|frame| draw(frame, &mut app, &session, focus, dead, confirming.as_ref()))?;
            last_draw = Instant::now();
            dirty = false;
        }

        match session.input.recv_timeout(TICK) {
            Ok(bytes) => {
                if let Some(gained) = focus::event(&bytes) {
                    // Coming back to the window is when a stale idea of the
                    // pane's width shows itself: the program repaints, wraps
                    // its lines where the pane does not, and lands the tail on
                    // top of the row it just wrote. Jog the size and it
                    // repaints against the truth instead. See `Pane::redraw`.
                    if gained && !window_focused {
                        session.tabs.front_mut().redraw = Some(Instant::now());
                    }
                    window_focused = gained;
                }
                let bytes = shift_mouse(&bytes, work_offset(session.width, session.side));
                // A program that never asked for focus reporting would print
                // these as stray characters, so they go no further.
                let bytes = if session.tabs.work().wants_focus.load(Ordering::Relaxed) {
                    bytes
                } else {
                    focus::strip(&bytes)
                };
                let was_confirming = confirming.clone();
                let action = if bytes.is_empty() {
                    Action::Nothing // nothing but a focus event
                } else if bytes.contains(&NEW_TAB) {
                    // From either side of the divider, and without going
                    // through the panel: this is the key you reach for
                    // *because* the terminal's own cmd-T is the wrong tab.
                    Action::NewTab
                } else if let Some(delta) = switch(
                    &bytes,
                    session.switch,
                    !app.snapshot.is_empty() || session.tabs.shells() > 1,
                ) {
                    // Flipping tabs works from either side, and without going
                    // through the panel: it is the one thing you do often
                    // enough that three keystrokes is two too many.
                    Action::Cycle(delta)
                } else if dead.is_some() && focus == Focus::Work {
                    dead_pane_key(&bytes)
                } else {
                    route(
                        &bytes,
                        focus,
                        &mut app,
                        session.tabs.work_mut(),
                        confirming.as_ref(),
                    )?
                };
                // Any key answers the question, so the prompt never outlives
                // the keystroke that followed it.
                if was_confirming.is_some() {
                    confirming = None;
                }
                match action {
                    Action::Nothing => {}
                    Action::Repaint => terminal.clear()?,
                    Action::ConfirmQuit => confirming = Some(Confirm::Quit),
                    Action::ConfirmDelete => {
                        if let Some(job) = app.selected_job() {
                            confirming = Some(Confirm::Delete {
                                short: job.short.clone(),
                                name: job.name.clone(),
                            });
                        }
                    }
                    Action::Delete => {
                        if let Some(Confirm::Delete { short, name }) = &was_confirming {
                            delete_session(
                                &mut session,
                                &mut app,
                                &starting,
                                short.clone(),
                                name.clone(),
                            );
                        }
                    }
                    Action::Focus(next) => focus = next,
                    Action::Cycle(delta) => {
                        let stop = neighbour(
                            &app.snapshot,
                            session.tabs.shells(),
                            &session.tabs.front_ref(),
                            delta,
                        );
                        let moved = match stop {
                            // Back to a pane of your own, in the order you
                            // opened them.
                            Some(Stop::Shell(n)) => match session.tabs.shell_at(n) {
                                Some(index) => {
                                    session.tabs.go_to(index);
                                    app.select_shell(n);
                                    Ok(true)
                                }
                                None => Ok(false),
                            },
                            Some(Stop::Session(short)) => {
                                open_short(&mut session, &mut app, &short)
                            }
                            None => Ok(false),
                        };
                        match moved {
                            Ok(true) => {
                                // A session you have flipped to is one you have
                                // been to, whatever it was asking.
                                attend_front(&mut session, &mut app);
                                focus = Focus::Work;
                                terminal.clear()?;
                            }
                            Ok(false) => {}
                            Err(e) => app.error = Some(format!("could not open: {e}")),
                        }
                    }
                    Action::AddAgent => start_agent(&mut app, &starting),
                    Action::Regroup => {
                        // Say which it is now: the headings change, but the
                        // panel may be showing one repository and one status
                        // group, which look alike.
                        let how = app.regroup();
                        app.error = Some(match how {
                            GroupBy::Repo => "grouped by repository".to_string(),
                            GroupBy::Status => "grouped by status".to_string(),
                        });
                    }
                    Action::NewTab => match new_tab(&mut session) {
                        Ok(()) => {
                            focus = Focus::Work;
                            terminal.clear()?;
                        }
                        Err(e) => app.error = Some(format!("could not open a tab: {e}")),
                    },
                    Action::CloseFront | Action::CloseSelected => {
                        let target = match action {
                            Action::CloseFront => Some(session.tabs.current),
                            // `x` closes whatever the cursor is on, which is a
                            // pane of your own as readily as a session's.
                            _ => match app.selected_shell() {
                                Some(n) => session.tabs.shell_at(n),
                                None => app
                                    .selected_job()
                                    .and_then(|job| session.tabs.position(&job.short)),
                            },
                        };
                        match target {
                            Some(index) if session.tabs.close(index) => {
                                terminal.clear()?;
                            }
                            // The last tab is the working pane itself, and
                            // closing that is quitting — which asks first.
                            Some(_) => confirming = Some(Confirm::Quit),
                            None => {}
                        }
                    }
                    Action::Open | Action::Reopen => {
                        let opened = if matches!(action, Action::Reopen) {
                            reopen(&mut session)
                        } else {
                            open_selected(&mut session, &mut app)
                        };
                        match opened {
                            Ok(true) => {
                                focus = Focus::Work;
                                terminal.clear()?;
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

        // The parsers already hold whatever the children wrote; these are only
        // wake-ups. Every tab is drained, or a hidden one's queue grows for as
        // long as you leave it — but only the tab in front makes the screen
        // dirty, since the others are not on it.
        let front = session.tabs.current;
        for (i, tab) in session.tabs.open.iter().enumerate() {
            while tab.work.output.try_recv().is_ok() {
                dirty |= i == front;
            }
        }

        while let Ok(outcome) = started.try_recv() {
            match outcome {
                // The new session writes its own state.json within a moment,
                // and the panel is watching that directory — so the row
                // appears on its own, and there is nothing to announce.
                Ok(_) => app.error = None,
                // Already worded where it was raised: one channel now carries
                // more than one kind of errand.
                Err(e) => app.error = Some(format!("{e:#}")),
            }
            dirty = true;
        }

        if watch.changed() || last_refresh.elapsed() >= REFRESH {
            app.refresh();
            // The session in the pane is only "in front of you" while the
            // terminal has focus; in another application it is as invisible
            // as any other, and must ping like one. A session open in a tab
            // *behind* another tab is not in front of you either.
            let open = session.tabs.short().map(str::to_string);
            let pinged = session
                .ping
                .poll(&app.snapshot, watching(window_focused, open.as_deref()));
            app.alert(pinged);
            last_refresh = Instant::now();
            dirty = true;
        }

        if resize_if_needed(&mut session)? {
            dirty = true;
        }

        if jog_new_panes(&mut session) {
            dirty = true;
        }

        if close_finished_shells(&mut session) {
            terminal.clear()?;
            dirty = true;
        }
    }
}

/// Which session you are actually watching, and so need no ping about.
///
/// A session open in the pane is only in front of you while the terminal has
/// your attention. In another application it is as invisible as any other
/// session, and staying quiet about it is how you miss the question you most
/// needed to see.
fn watching(focused: bool, open: Option<&str>) -> Option<&str> {
    open.filter(|_| focused)
}

/// Bring the selected session to the front: its existing tab if it has one,
/// otherwise a new tab running it.
///
/// Opening a session you already have open is the common case once tabs exist,
/// and it must *not* start a second copy — that is how you end up attached to
/// one session twice and reading the wrong one.
fn open_selected(session: &mut Session, app: &mut App) -> Result<bool> {
    let Some(short) = app.selected_job().map(|job| job.short.clone()) else {
        return Ok(false);
    };
    open_short(session, app, &short)
}

/// Bring a session to the front by id, opening it if it has no pane yet.
fn open_short(session: &mut Session, app: &mut App, short: &str) -> Result<bool> {
    if let Some(index) = session.tabs.position(short) {
        session.tabs.go_to(index);
        app.attend_to(short);
        app.select(short);
        return Ok(true);
    }

    let Some(job) = app.snapshot.jobs.iter().find(|j| j.short == short) else {
        return Ok(false);
    };
    let short = short.to_string();
    let (command, cwd) = resume(job);
    let (cols, rows) = session.work_size();
    // Spawn before touching the tab list, so a failure changes nothing.
    let work = Work::spawn(&command, Some(&cwd), cols, rows, Origin::Opened)?;
    session.tabs.push(Pane {
        short: Some(short.clone()),
        reopen: Some((command, cwd)),
        work,
        // An attach replays the session as it was drawn in another terminal,
        // at another width. Jog it into repainting at this one.
        redraw: Some(Instant::now() + REDRAW_AFTER),
    });
    // Going to a session is the clearest possible way of saying you saw which
    // one it was.
    app.attend_to(&short);
    app.select(&short);
    Ok(true)
}

/// Close a tab of your own whose shell has exited.
///
/// `exit` closes the tab, the way it closes a tab in any terminal — there is
/// nothing on a finished shell's screen worth keeping you there. A *session's*
/// pane is the opposite case and keeps its last screen, because that screen is
/// usually the reason it stopped. And the shell Savras arrived in is neither:
/// leaving that one is leaving Savras, which is handled where the quit is.
fn close_finished_shells(session: &mut Session) -> bool {
    let done: Vec<usize> = (0..session.tabs.open.len())
        .filter(|i| {
            let tab = &mut session.tabs.open[*i];
            tab.short.is_none()
                && tab.work.origin == Origin::Opened
                && tab.work.exit_code().is_some()
        })
        .collect();
    // Back to front, so the indices ahead of each one stay where they were.
    let mut closed = false;
    for i in done.into_iter().rev() {
        closed |= session.tabs.close(i);
    }
    closed
}

/// Make a freshly opened pane draw itself again, at the size it is really
/// being shown at.
///
/// See [`Pane::redraw`] for why: a replayed transcript is wrapped for the
/// terminal it was written in, not for this pane, and the two layouts land on
/// top of each other. The tty signals only on a size that *changed*, so the
/// jog goes one column narrow and straight back — a wrong width for an instant
/// is the price of a correct screen after it.
fn jog_new_panes(session: &mut Session) -> bool {
    let (cols, rows) = session.work_size();
    let now = Instant::now();
    let mut jogged = false;
    for tab in &mut session.tabs.open {
        if tab.redraw.is_some_and(|at| now >= at) {
            tab.redraw = None;
            let _ = tab.work.resize(cols.saturating_sub(1).max(1), rows);
            let _ = tab.work.resize(cols, rows);
            jogged = true;
        }
    }
    jogged
}

/// Open another pane of your own, running what Savras was started with.
///
/// The terminal's own cmd-T is the wrong tab: it gives you a window beside
/// Savras rather than one inside it, without the panel and without the
/// sessions you have open. This is the same gesture, one level in.
///
/// It runs the command Savras was started with — your shell for plain `svr`,
/// and whatever followed `--` otherwise — in the directory Savras was started
/// in. `Origin::Opened`, so leaving it closes the tab rather than Savras: only
/// the shell you *arrived* in still takes the panel with it.
fn new_tab(session: &mut Session) -> Result<()> {
    let (cols, rows) = session.work_size();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let command = session.command.clone();
    // Spawn before touching the tab list, so a failure changes nothing.
    let work = Work::spawn(&command, Some(&cwd), cols, rows, Origin::Opened)?;
    session.tabs.push(Pane {
        short: None,
        reopen: Some((command, cwd)),
        work,
        redraw: Some(Instant::now() + REDRAW_AFTER),
    });
    Ok(())
}

/// Where one press of a switch key lands.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// One of your own panes. They sit in front of the first session, in the
    /// order you opened them, which is the order the panel lists them in.
    Shell(usize),
    Session(String),
}

/// The stop one step from where you are, in the order the panel shows.
///
/// The cycle is the panel's own list, with your shell at the head of it. That
/// is the whole correction over the first cut, which cycled the *panes that
/// happened to be alive*: flipping has to walk the rows you can see, or it
/// takes you somewhere the screen never mentioned — an empty shell and the one
/// session you had opened, while five more sat in the list untouched.
///
/// Landing on a session opens it if it has no pane yet, so every row is one
/// press away rather than three.
fn neighbour(snapshot: &Snapshot, shells: usize, front: &Front, delta: isize) -> Option<Stop> {
    // Your own panes are stops zero upward; the sessions follow them in
    // display order. One shell and no sessions is one stop, and nowhere to go.
    let stops = (snapshot.jobs.len() + shells) as isize;
    if stops < 2 {
        return None;
    }
    let here = match front {
        Front::Shell(i) => *i as isize,
        Front::Session(short) => snapshot
            .jobs
            .iter()
            .position(|j| &j.short == short)
            .map_or(0, |i| (i + shells) as isize),
    };
    let next = ((here + delta) % stops + stops) % stops;
    Some(if next < shells as isize {
        Stop::Shell(next as usize)
    } else {
        Stop::Session(snapshot.jobs[next as usize - shells].short.clone())
    })
}

/// Run the front tab's session again, in place, after it exited.
fn reopen(session: &mut Session) -> Result<bool> {
    let Some((command, cwd)) = session.tabs.front().reopen.clone() else {
        return Ok(false);
    };
    let (cols, rows) = session.work_size();

    // Spawn before dropping the old one, so a failure leaves the pane intact.
    let work = Work::spawn(&command, Some(&cwd), cols, rows, Origin::Opened)?;
    session.tabs.front_mut().work = work;
    Ok(true)
}

/// Put the session asked for in the pane before the first frame is drawn.
///
/// Failure is not fatal and barely even an error: you asked to start on a
/// session, and if it is not there you start on the shell with the panel
/// saying why. Nothing else about Savras depends on it.
fn open_at_startup(session: &mut Session, app: &mut App, open: &Open) {
    let short = match open {
        Open::Shell => return,
        Open::Top => match app.first_job() {
            Some(job) => job.short.clone(),
            None => return, // no sessions at all; the shell is all there is
        },
        Open::Named(name) => match app.job_named(name) {
            Some(job) => job.short.clone(),
            None => {
                app.error = Some(format!("no session called {name}"));
                return;
            }
        },
    };
    if let Err(e) = open_short(session, app, &short) {
        app.error = Some(format!("could not open: {e}"));
    }
}

/// Delete a session, having asked. Its tab goes with it: an `attach` to a
/// session that no longer exists is a pane that can only tell you so.
///
/// Off the main thread, because `claude stop` and `claude rm` take long enough
/// between them that doing it here would visibly stall the panel — and the row
/// disappears on its own when the directory does, so there is nothing to
/// announce on success.
fn delete_session(
    session: &mut Session,
    app: &mut App,
    outcome: &mpsc::Sender<Result<String>>,
    short: String,
    name: String,
) {
    if let Some(index) = session.tabs.position(&short) {
        session.tabs.close(index);
    }
    app.error = Some(format!("deleting {name}…"));
    let outcome = outcome.clone();
    std::thread::spawn(move || {
        let _ = outcome.send(
            crate::agents::delete(&short).with_context(|| format!("could not delete {name}")),
        );
    });
}

/// Start a parallel agent under the selected session's lead.
///
/// The lead is the selected session's *base* name, so pressing this on
/// `AGENT-3` adds a fourth agent under `AGENT` rather than starting a group
/// beneath a group. It runs in the lead's own repository, because an agent
/// that cannot see the code is no use.
///
/// This is the one thing Savras does that is not looking: it *starts*
/// sessions. It still never writes to `~/.claude/`, and it still never
/// interrupts a session that is already running — the new agent introduces
/// itself to its lead, through Claude Code's own messaging, as its first act.
fn start_agent(app: &mut App, outcome: &mpsc::Sender<Result<String>>) {
    let Some(job) = app.selected_job() else {
        return;
    };
    // The lead has to be a session that is actually running, because the new
    // agent is told to message it by name. The *numbering* follows the base
    // name, so a group led by `SAVRAS-2` still adds `SAVRAS-4` next.
    let leader = crate::agents::lead_of(&app.snapshot, job);
    let lead = leader.name.clone();
    // Its own repository: an agent that cannot see the code is no use.
    let cwd = leader.cwd.clone();
    let (base, _) = crate::agents::split(&job.name);
    let name = crate::agents::next_name(&app.snapshot, base);

    app.error = Some(format!("starting {name}…"));
    let outcome = outcome.clone();
    std::thread::spawn(move || {
        let briefing = crate::agents::briefing(&lead, &name);
        let _ = outcome
            .send(crate::agents::start(&name, &briefing, &cwd).context("could not start an agent"));
    });
}

/// Clear the mark on whatever session is now in front — flipping to a tab is
/// going to it.
fn attend_front(session: &mut Session, app: &mut App) {
    if let Some(short) = session.tabs.short().map(str::to_string) {
        app.attend_to(&short);
    }
}

/// Keys for a pane whose program has exited. Its last screen is still on
/// display — usually the error that explains the exit — so the keys are about
/// what to do next, not about typing into a dead terminal.
fn dead_pane_key(bytes: &[u8]) -> Action {
    match bytes {
        [b'\r'] | [b'\n'] => Action::Reopen,
        // Closing the tab, not Savras: the other sessions you have open are
        // not implicated in this one exiting.
        [b'q'] => Action::CloseFront,
        _ => Action::Nothing,
    }
}

/// Which way to flip, if this keystroke says to flip at all.
///
/// Two ways in. The switch key — Ctrl-O by default — always goes forward, and
/// works in every terminal because a control byte is delivered unchanged
/// everywhere. Shift-Option with an arrow goes either way, and is nicer, but
/// only reaches us in terminals that encode modifiers on arrows; Terminal.app
/// does not, so it cannot be the only way in.
///
/// `somewhere_to_go` is whether there is any session to flip to at all. With
/// none, the keys are handed to the child instead — a panel showing nothing
/// has no business taking ctrl-w from your shell.
fn switch(bytes: &[u8], keys: Switch, somewhere_to_go: bool) -> Option<isize> {
    if !somewhere_to_go {
        return None;
    }
    if keys.back.is_some_and(|k| bytes.contains(&k)) {
        return Some(-1);
    }
    if keys.forward.is_some_and(|k| bytes.contains(&k)) {
        return Some(1);
    }
    tab_chord(bytes)
}

/// A modified arrow: flip one tab back or forward.
///
/// Every terminal that reports modifiers at all uses the same shape,
/// `ESC [ 1 ; <modifiers> <arrow>`, so accepting several modifier values costs
/// nothing and means the chord works wherever it can be sent — no capability
/// negotiation, no configuration. A terminal that cannot send one simply never
/// sends it, and the control keys are still there.
///
/// The values are xterm's: 1 plus shift(1) + alt(2) + ctrl(4), and +8 again
/// for meta. So `6` is ctrl-shift, `4` is shift-alt, and `10` is shift-meta,
/// which terminals set to send Option as Meta use instead of `4`.
///
/// Deliberately absent: `2` (plain shift) and `5` (plain ctrl) are selection
/// and word-movement in the programs running in the pane, and `9`/`13` would
/// be Command — which no terminal ever sends, since Command is not in this
/// encoding at all and every terminal keeps those chords for its own tabs.
const CHORDS: [&[u8]; 3] = [b"6", b"4", b"10"];

fn tab_chord(bytes: &[u8]) -> Option<isize> {
    for modifiers in CHORDS {
        for (arrow, delta) in [(b'A', -1), (b'D', -1), (b'B', 1), (b'C', 1)] {
            let mut sequence = vec![ESC, b'[', b'1', b';'];
            sequence.extend_from_slice(modifiers);
            sequence.push(arrow);
            if bytes.windows(sequence.len()).any(|w| w == sequence) {
                return Some(delta);
            }
        }
    }
    None
}

/// How to reopen a session: the command, and where to run it.
fn resume(job: &crate::job::Job) -> (Vec<String>, PathBuf) {
    (job.open_command(), job.cwd.clone())
}

/// Send keystrokes where they belong.
fn route(
    bytes: &[u8],
    focus: Focus,
    app: &mut App,
    work: &mut Work,
    confirming: Option<&Confirm>,
) -> Result<Action> {
    // While the panel is asking whether to quit, the answer is the only thing
    // that matters — including ctrl-g, which would otherwise leave the
    // question hanging behind the working pane.
    if bytes.contains(&FOCUS_TOGGLE) && confirming.is_none() {
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
        Focus::Panel => Ok(panel_key(bytes, app, confirming)),
    }
}

/// The panel is read-only, so it needs a cursor, a way in, and a way out.
///
/// `confirming` is set once `Q` has been pressed: the next key either confirms
/// the quit or cancels it, and nothing else happens in between.
fn panel_key(bytes: &[u8], app: &mut App, confirming: Option<&Confirm>) -> Action {
    match confirming {
        Some(Confirm::Quit) => {
            return match bytes {
                [b'Q'] => {
                    app.should_quit = true;
                    Action::Nothing
                }
                // Anything else is "no": a quit that closes your working pane
                // should need saying twice, and mean it both times.
                _ => Action::Focus(Focus::Panel),
            };
        }
        // Deleting a session is not something Savras can undo, and neither can
        // you: the conversation goes with it.
        Some(Confirm::Delete { .. }) => {
            return match bytes {
                [b'd'] => Action::Delete,
                _ => Action::Focus(Focus::Panel),
            };
        }
        None => {}
    }
    match bytes {
        [b'j'] | [ESC, b'[', b'B'] => app.step(1),
        [b'k'] | [ESC, b'[', b'A'] => app.step(-1),
        [b'g'] => app.jump(false),
        [b'G'] => app.jump(true),
        [b'r'] => app.refresh(),
        [REPAINT] => return Action::Repaint,
        [b'\r'] | [b'\n'] => return Action::Open,
        // An open tab is a live session and a screen buffer, so a tab set you
        // can only grow is a leak you cannot see.
        [b'x'] => return Action::CloseSelected,
        [b'a'] => return Action::AddAgent,
        [b's'] => return Action::Regroup,
        // Closing a tab leaves the session running, which is the point of
        // tabs — and why finished sessions pile up in `claude agents` with
        // nothing here to get rid of them. This is that.
        [b'd'] => return Action::ConfirmDelete,
        // A tab of your own, for the shell you wanted without leaving Savras
        // to get it. Ctrl-T does the same from the working pane.
        [b'n'] => return Action::NewTab,
        // Quitting closes the working pane as well, so it asks first.
        [b'Q'] => return Action::ConfirmQuit,
        // Esc and q give the terminal back rather than quitting: in a side
        // panel, closing the whole window is rarely what was meant.
        [ESC] | [b'q'] => return Action::Focus(Focus::Work),
        _ => {}
    }
    Action::Nothing
}

fn draw(
    frame: &mut Frame,
    app: &mut App,
    session: &Session,
    focus: Focus,
    dead: Option<u32>,
    confirming: Option<&Confirm>,
) {
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

    let question = confirming.map(Confirm::question);
    let hint = match (focus, &question) {
        (Focus::Panel, Some(what)) => Hint::Confirming(what),
        (Focus::Panel, None) => Hint::Focused,
        (Focus::Work, _) => Hint::Background,
    };
    ui::draw_in(frame, panel_area, app, hint);

    let divider = Paragraph::new(
        std::iter::repeat_n(Line::from("│"), chunks[1].height as usize).collect::<Vec<_>>(),
    )
    .style(Style::default().fg(Color::Indexed(238)));
    frame.render_widget(divider, chunks[1]);

    let parser = session.tabs.work().parser.lock().unwrap();
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
    session.tabs.resize_all(cols.max(1), rows)?;
    Ok(true)
}

/// What to call the panes Savras runs for you, in the panel's list.
///
/// The program's own name, so `svr -- claude` lists "claude" and plain `svr`
/// lists "shell" — the row should say what is in the pane, and with `--` the
/// user already told us.
fn pane_label(command: &[String]) -> String {
    match command.first() {
        Some(program) => Path::new(program)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| program.clone()),
        None => "shell".to_string(),
    }
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
    // So a Savras started in here knows it would be the second one. Read by
    // [`nested`]; the value is the panel's own pid, which makes it obvious in
    // `env` what set it and which panel is meant.
    builder.env(NESTED, std::process::id().to_string());
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
    wants_focus: Arc<AtomicBool>,
) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut watcher = focus::Watcher::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    wants_focus.store(watcher.feed(&buf[..n]), Ordering::Relaxed);
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
    fn quitting_from_the_panel_takes_two_presses() {
        // `q` hands the keyboard back — closing the whole window is rarely
        // what was meant in a side panel — so quitting is `Q`, and because it
        // closes the working pane with it, it asks first.
        let f = Fixture::new("quit").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());

        assert!(matches!(
            panel_key(b"q", &mut app, None),
            Action::Focus(Focus::Work)
        ));
        assert!(!app.should_quit, "q is not a quit in the side panel");

        assert!(matches!(
            panel_key(b"Q", &mut app, None),
            Action::ConfirmQuit
        ));
        assert!(!app.should_quit, "the first Q only asks");

        panel_key(b"Q", &mut app, Some(&Confirm::Quit));
        assert!(app.should_quit, "the second Q means it");
    }

    #[test]
    fn deleting_a_session_takes_two_presses_and_names_it_in_between() {
        // `x` closes a tab and leaves the session running, which is what tabs
        // are for and why finished sessions pile up. `d` is the other thing,
        // and it cannot be undone — so it asks, and the question says which
        // session, because "delete it" is only answerable if you can see what
        // "it" is.
        let f = Fixture::new("delete").job("aaa", r#"{"state":"done","name":"OLD"}"#);
        let mut app = App::new(f.0.clone());

        assert!(matches!(
            panel_key(b"d", &mut app, None),
            Action::ConfirmDelete
        ));
        let asking = Confirm::Delete {
            short: "aaa".into(),
            name: "OLD".into(),
        };
        assert!(asking.question().contains("OLD"), "{}", asking.question());
        assert!(matches!(
            panel_key(b"d", &mut app, Some(&asking)),
            Action::Delete
        ));
        // Anything else keeps it, including the keys that do something else
        // entirely when no question is open.
        for answer in [&b"j"[..], b"\r", b"\x1b", b"x", b"q"] {
            assert!(
                matches!(
                    panel_key(answer, &mut app, Some(&asking)),
                    Action::Focus(Focus::Panel)
                ),
                "{answer:?} must not delete a session"
            );
        }
    }

    #[test]
    fn anything_but_a_second_q_cancels_the_quit() {
        let f = Fixture::new("quit-cancel").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        for answer in [&b"j"[..], b"\r", b"\x1b", b"q"] {
            panel_key(answer, &mut app, Some(&Confirm::Quit));
            assert!(!app.should_quit, "{answer:?} must not quit Savras");
        }
    }

    #[test]
    fn the_panel_can_be_told_to_paint_itself_again() {
        // Terminal.app lets you scroll the view of an alternate-screen
        // application, and a scrollbar drag sends no bytes for Savras to
        // notice, so the only cure is being asked to repaint.
        let f = Fixture::new("repaint").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(
            panel_key(b"\x0c", &mut app, None),
            Action::Repaint
        ));
    }

    #[test]
    fn an_open_session_is_only_spared_a_ping_while_you_are_there() {
        // Looking at it: it is asking you in person, on the other half of the
        // screen, and a notification would be noise.
        assert_eq!(watching(true, Some("aaa")), Some("aaa"));
        // In another application: that pane is as invisible as any other
        // session, and this is precisely the question you must not miss.
        assert_eq!(watching(false, Some("aaa")), None);
        // Focus is unknown until the terminal says otherwise, and unknown
        // counts as away.
        assert_eq!(watching(false, None), None);
    }

    /// A pane running something harmless that stays alive, for testing the
    /// bookkeeping around panes rather than the panes themselves.
    fn pane(short: Option<&str>) -> Pane {
        let work = Work::spawn(
            &["cat".to_string()],
            None,
            80,
            24,
            short.map_or(Origin::Initial, |_| Origin::Opened),
        )
        .unwrap();
        Pane {
            short: short.map(str::to_string),
            reopen: None,
            work,
            redraw: None,
        }
    }

    fn three_tabs() -> Tabs {
        let mut tabs = Tabs::new(pane(None).work);
        tabs.push(pane(Some("aaa")));
        tabs.push(pane(Some("bbb")));
        tabs
    }

    #[test]
    fn the_chord_reaches_us_on_both_axes_and_both_encodings() {
        // Up and left go back, down and right go forward: the panel's list is
        // vertical, and the terminal tab keys people already know are not.
        // Ctrl-shift, which is what most terminals send for the chord.
        assert_eq!(tab_chord(b"\x1b[1;6A"), Some(-1));
        assert_eq!(tab_chord(b"\x1b[1;6D"), Some(-1));
        assert_eq!(tab_chord(b"\x1b[1;6C"), Some(1));
        // Shift-option, for terminals that send that instead.
        assert_eq!(tab_chord(b"\x1b[1;4A"), Some(-1));
        assert_eq!(tab_chord(b"\x1b[1;4D"), Some(-1));
        assert_eq!(tab_chord(b"\x1b[1;4B"), Some(1));
        assert_eq!(tab_chord(b"\x1b[1;4C"), Some(1));
        // Terminals set to send Option as Meta encode the same chord as ;10.
        assert_eq!(tab_chord(b"\x1b[1;10A"), Some(-1));
        assert_eq!(tab_chord(b"\x1b[1;10C"), Some(1));
    }

    #[test]
    fn the_switch_keys_flip_both_ways_from_any_terminal() {
        // Control bytes are the only keys every terminal delivers unchanged,
        // which is the whole reason these exist: Terminal.app sends
        // ctrl-shift-arrows as plain arrows, so a chord cannot reach us there.
        let keys = Switch::default();
        assert_eq!(switch(&[SWITCH_BACK], keys, true), Some(-1));
        assert_eq!(switch(&[SWITCH_FORWARD], keys, true), Some(1));
        // Typing that arrives in the same read as the key still counts.
        assert_eq!(switch(b"\x13ls\r", keys, true), Some(1));
        // Other letters, from `--switch`.
        let moved = Switch {
            back: Some(0x0f),
            forward: Some(0x15),
        };
        assert_eq!(switch(&[SWITCH_BACK], moved, true), None);
        assert_eq!(switch(&[0x0f], moved, true), Some(-1));
        assert_eq!(switch(&[0x15], moved, true), Some(1));
    }

    #[test]
    fn the_switch_keys_are_the_childs_when_there_is_nowhere_to_go() {
        // Ctrl-W is delete-previous-word. A panel showing no sessions at all
        // has no business taking it from your shell.
        let keys = Switch::default();
        assert_eq!(switch(&[SWITCH_BACK], keys, false), None);
        assert_eq!(switch(&[SWITCH_FORWARD], keys, false), None);
        // ...and `--switch off` never takes them at all.
        assert_eq!(switch(&[SWITCH_BACK], Switch::OFF, true), None);
        // The arrow chord is still read when it arrives.
        assert_eq!(switch(b"\x1b[1;6D", Switch::OFF, true), Some(-1));
    }

    #[test]
    fn the_footer_names_whichever_keys_are_bound() {
        assert_eq!(Switch::default().label().as_deref(), Some("ctrl-w/s"));
        assert_eq!(
            Switch {
                back: None,
                forward: Some(0x0f)
            }
            .label()
            .as_deref(),
            Some("ctrl-o")
        );
        assert_eq!(Switch::OFF.label(), None);
    }

    #[test]
    fn ordinary_keys_are_never_mistaken_for_the_chord() {
        // A plain arrow, a shift-arrow and a ctrl-arrow all belong to the
        // child; stealing any of them would break editing in the pane.
        for keys in [
            b"\x1b[A".as_slice(),
            b"\x1b[1;2A",
            b"\x1b[1;5D",
            b"\x1b[1;3C",
            b"hello",
            b"\x1b",
        ] {
            assert_eq!(tab_chord(keys), None, "stole {keys:?}");
        }
    }

    fn three_sessions() -> Fixture {
        Fixture::new("host-neighbour")
            .job("aaa", r#"{"state":"working","name":"A"}"#)
            .job("bbb", r#"{"state":"working","name":"B"}"#)
            .job("ccc", r#"{"state":"working","name":"C"}"#)
    }

    /// The panel's own order, which is what flipping has to follow.
    fn order(app: &App) -> Vec<String> {
        app.snapshot.jobs.iter().map(|j| j.short.clone()).collect()
    }

    #[test]
    fn flipping_walks_the_rows_the_panel_shows() {
        // The first cut cycled the panes that happened to be alive, which with
        // one session opened meant flipping between an empty shell and that
        // one session while every other row sat there untouched.
        let f = three_sessions();
        let app = App::new(f.0.clone());
        let rows = order(&app);
        let snap = &app.snapshot;

        // From the shell, forward is the first row on screen.
        assert_eq!(
            neighbour(snap, 1, &Front::Shell(0), 1),
            Some(Stop::Session(rows[0].clone()))
        );
        // ...and on through the list, in the order you can see.
        assert_eq!(
            neighbour(snap, 1, &Front::Session(rows[0].clone()), 1),
            Some(Stop::Session(rows[1].clone()))
        );
        assert_eq!(
            neighbour(snap, 1, &Front::Session(rows[1].clone()), -1),
            Some(Stop::Session(rows[0].clone()))
        );
    }

    #[test]
    fn the_shell_sits_at_the_head_of_the_cycle() {
        // It must stay reachable: it is where you started, and often where the
        // command you actually wanted to run lives.
        let f = three_sessions();
        let app = App::new(f.0.clone());
        let rows = order(&app);
        let snap = &app.snapshot;

        assert_eq!(
            neighbour(snap, 1, &Front::Session(rows[0].clone()), -1),
            Some(Stop::Shell(0))
        );
        // Wrapping the other way: past the last session is the shell again.
        assert_eq!(
            neighbour(snap, 1, &Front::Session(rows[2].clone()), 1),
            Some(Stop::Shell(0))
        );
        assert_eq!(
            neighbour(snap, 1, &Front::Shell(0), -1),
            Some(Stop::Session(rows[2].clone()))
        );
    }

    #[test]
    fn a_tab_of_your_own_is_a_stop_like_any_other() {
        // The point of opening one is being able to get back to it, and the
        // flip keys are how you get anywhere here.
        let f = three_sessions();
        let app = App::new(f.0.clone());
        let rows = order(&app);
        let snap = &app.snapshot;

        // Two shells at the head, then the sessions, in the panel's order.
        assert_eq!(
            neighbour(snap, 2, &Front::Shell(0), 1),
            Some(Stop::Shell(1))
        );
        assert_eq!(
            neighbour(snap, 2, &Front::Shell(1), 1),
            Some(Stop::Session(rows[0].clone()))
        );
        assert_eq!(
            neighbour(snap, 2, &Front::Session(rows[0].clone()), -1),
            Some(Stop::Shell(1))
        );
        // Wrapping past the last session lands on the first shell, not the
        // one you happened to open last.
        assert_eq!(
            neighbour(snap, 2, &Front::Session(rows[2].clone()), 1),
            Some(Stop::Shell(0))
        );
    }

    #[test]
    fn two_shells_and_no_sessions_still_flip() {
        // Savras with no sessions to watch is still two panes you opened, and
        // the keys have somewhere to go.
        let f = Fixture::new("host-two-shells");
        let app = App::new(f.0.clone());
        assert_eq!(
            neighbour(&app.snapshot, 2, &Front::Shell(0), 1),
            Some(Stop::Shell(1))
        );
        assert_eq!(
            neighbour(&app.snapshot, 2, &Front::Shell(1), 1),
            Some(Stop::Shell(0))
        );
    }

    #[test]
    fn shells_are_counted_and_found_in_the_order_they_were_opened() {
        let mut tabs = three_tabs();
        tabs.push(pane(None));
        assert_eq!(tabs.shells(), 2);
        // The shell Savras started with is first, the one you added second,
        // wherever the sessions between them sit.
        assert_eq!(tabs.shell_at(0), Some(0));
        assert_eq!(tabs.shell_at(1), Some(3));
        assert_eq!(tabs.shell_at(2), None);
        assert_eq!(tabs.front_ref(), Front::Shell(1));
        tabs.go_to(1);
        assert_eq!(tabs.front_ref(), Front::Session("aaa".into()));
        tabs.go_to(0);
        assert_eq!(tabs.front_ref(), Front::Shell(0));
    }

    #[test]
    fn a_shell_you_opened_closes_itself_when_you_exit_it() {
        // `exit` closes the tab, as it does in any terminal. A session's pane
        // keeps its last screen — that screen is why it stopped — and the
        // shell Savras arrived in is a quit, handled elsewhere.
        let mut tabs = three_tabs();
        let mut opened = pane(None);
        opened.work.origin = Origin::Opened;
        tabs.push(opened);
        let mut session = session_with(tabs);
        assert_eq!(session.tabs.shells(), 2);

        // Still running: nothing closes.
        assert!(!close_finished_shells(&mut session));

        // The shell exits, and its tab goes with it.
        let _ = session.tabs.open[3].work.child.kill();
        let _ = session.tabs.open[3].work.child.wait();
        assert!(close_finished_shells(&mut session));
        assert_eq!(session.tabs.shells(), 1);
        // The session panes are untouched, dead or alive.
        assert_eq!(session.tabs.shorts(), ["aaa", "bbb"]);
    }

    /// A `Session` around a set of tabs, for the bookkeeping that needs one.
    fn session_with(tabs: Tabs) -> Session {
        Session {
            jobs_dir: PathBuf::from("/nonexistent"),
            width: 44,
            side: Side::Right,
            size: (100, 40),
            input: std::sync::mpsc::channel().1,
            tabs,
            command: Vec::new(),
            switch: Switch::default(),
            ping: crate::ping::Ping::new(
                crate::ping::When::default(),
                false,
                std::time::Duration::from_secs(0),
            ),
        }
    }

    #[test]
    fn a_new_tab_is_asked_for_from_either_side_of_the_divider() {
        // Ctrl-T from the working pane, `n` from the panel: the terminal keeps
        // cmd-T for itself, so the reflex needs somewhere to land.
        let f = three_sessions();
        let mut app = App::new(f.0.clone());
        assert!(matches!(panel_key(b"n", &mut app, None), Action::NewTab));
        assert!(b"\x14".contains(&NEW_TAB));
    }

    #[test]
    fn what_the_pane_opens_on_is_a_name_unless_it_is_one_of_two_words() {
        assert_eq!(Open::parse("shell"), Open::Shell);
        assert_eq!(Open::parse("top"), Open::Top);
        // Anything else is a session, including words that look like options.
        assert_eq!(Open::parse("SAVRAS"), Open::Named("SAVRAS".into()));
    }

    #[test]
    fn a_pane_is_named_after_what_runs_in_it() {
        assert_eq!(pane_label(&[]), "shell");
        assert_eq!(pane_label(&["claude".to_string()]), "claude");
        assert_eq!(pane_label(&["/bin/zsh".to_string()]), "zsh");
    }

    #[test]
    fn with_no_sessions_there_is_nowhere_to_flip() {
        let f = Fixture::new("host-nowhere");
        let app = App::new(f.0.clone());
        assert_eq!(neighbour(&app.snapshot, 1, &Front::Shell(0), 1), None);
        // ...and the keys are left to the child rather than swallowed.
        assert_eq!(switch(&[SWITCH_BACK], Switch::default(), false), None);
    }

    #[test]
    fn a_session_already_open_is_found_rather_than_started_twice() {
        // The whole point of tabs: opening a session you have open must bring
        // its pane forward, not attach to the same session a second time.
        let tabs = three_tabs();
        assert_eq!(tabs.position("aaa"), Some(1));
        assert_eq!(tabs.position("bbb"), Some(2));
        assert_eq!(tabs.position("never-opened"), None);
        assert_eq!(tabs.shorts(), ["aaa", "bbb"]);
        assert_eq!(tabs.short(), Some("bbb"));
    }

    #[test]
    fn closing_a_tab_lands_on_a_neighbour() {
        let mut tabs = three_tabs();
        tabs.go_to(1);
        assert!(tabs.close(1));
        assert_eq!(tabs.shorts(), ["bbb"]);
        assert_eq!(tabs.current, 0, "closing lands on the tab before it");
    }

    #[test]
    fn the_last_tab_cannot_be_closed() {
        // It is the working pane itself, and closing that is quitting — which
        // is a different key, and asks first.
        let mut tabs = Tabs::new(pane(None).work);
        assert!(!tabs.close(0));
        assert_eq!(tabs.open.len(), 1);
    }

    #[test]
    fn closing_a_tab_behind_you_leaves_you_where_you_are() {
        let mut tabs = three_tabs();
        tabs.go_to(2);
        assert!(tabs.close(0));
        assert_eq!(tabs.current, 1, "still on bbb, now one place along");
        assert_eq!(tabs.short(), Some("bbb"));
    }

    #[test]
    fn x_on_the_panel_closes_a_tab() {
        let f = Fixture::new("host-close").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(
            panel_key(b"x", &mut app, None),
            Action::CloseSelected
        ));
    }

    #[test]
    fn a_starts_a_parallel_agent() {
        let f = Fixture::new("host-agent").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(panel_key(b"a", &mut app, None), Action::AddAgent));
    }

    #[test]
    fn the_panel_knows_which_sessions_are_open_and_which_is_in_front() {
        let f = Fixture::new("host-marks")
            .job("aaa", r#"{"state":"working","name":"A"}"#)
            .job("bbb", r#"{"state":"working","name":"B"}"#)
            .job("ccc", r#"{"state":"working","name":"C"}"#);
        let mut app = App::new(f.0.clone());
        app.set_tabs(
            Front::Session("bbb".into()),
            vec!["aaa".into(), "bbb".into()],
            1,
        );

        let tab_of = |app: &App, short: &str| {
            let job = app
                .snapshot
                .jobs
                .iter()
                .find(|j| j.short == short)
                .unwrap()
                .clone();
            app.tab(&job)
        };
        assert_eq!(tab_of(&app, "bbb"), crate::app::Tab::Front);
        assert_eq!(tab_of(&app, "aaa"), crate::app::Tab::Behind);
        assert_eq!(tab_of(&app, "ccc"), crate::app::Tab::None);
        // Your shell counts: it is a live pane behind you like any other.
        assert_eq!(
            app.behind_count(),
            2,
            "the shell and the session behind you, but not the one in front"
        );
    }

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
        assert!(matches!(panel_key(b"\r", &mut app, None), Action::Open));
        assert!(matches!(panel_key(b"\n", &mut app, None), Action::Open));
    }

    #[test]
    fn a_dead_pane_offers_a_way_out_instead_of_swallowing_keys() {
        assert!(matches!(dead_pane_key(b"\r"), Action::Reopen));
        // `q` closes the tab, not Savras: the other sessions you have open are
        // not implicated in this one exiting.
        assert!(matches!(dead_pane_key(b"q"), Action::CloseFront));
        assert!(matches!(dead_pane_key(b"z"), Action::Nothing));
    }

    #[test]
    fn esc_hands_the_keyboard_back_instead_of_quitting() {
        let f = Fixture::new("host-esc").job("aaa", r#"{"state":"working","name":"X"}"#);
        let mut app = App::new(f.0.clone());
        assert!(matches!(
            panel_key(b"\x1b", &mut app, None),
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
