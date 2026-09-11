//! Savras — a side panel that sees every Claude Code session you have running.
//!
//! It watches `~/.claude/jobs/` and never writes there, so it cannot disturb
//! the sessions it reports on. It can *start* one — see `agents` — which is
//! the only thing it does that is not looking.

mod agents;
mod app;
mod board;
mod focus;
mod host;
mod job;
mod panel;
mod ping;
mod remote;
#[cfg(test)]
mod testing;
mod ui;
mod watch;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use app::App;
use host::{Side, Switch};
use ping::{Ping, When};
use watch::Watch;

const USAGE: &str = "\
savras — see every Claude Code session you have running

usage: svr [options] [-- <command>...]
       svr solo [options]
       svr board <post|read|unread|install> [...]

By default Savras opens the panel as a column beside your work, with the
session at the top of the list in the pane next to it — or the command after
`--`, if you name one. Your shell is always the first tab, one ctrl-w away.

commands:
  (none)              the side panel, with the top session beside it
  solo                just the panel, with no working pane, for its own tab
  board               the agent board — see `svr board --help`

options:
  --side <left|right> which side the panel sits on (default right)
  --width <cols>      width of the panel column (default 44)
  --tmux              build the layout with tmux instead of hosting it directly
  --dry-run           print the tmux layout instead of building it
  --ping <when>       ping on needs (default), done, or off
  --no-sound          notify without a sound
  --quiet <seconds>   silence after a ping, news held until it ends (default 20)
  --group <what>      group rows by repo (the default) or status; s flips it
  --switch <keys>     ctrl-<back><forward> flips tabs (default ws), or off
  --open <what>       what the working pane starts on: top, the first session
                      in the panel (the default), a session name, or shell
  --once              print the current sessions as plain text and exit
  --keys              print what this terminal sends for each key, and what
                      Savras makes of it, when a chord seems to do nothing
  --machine <host>    also watch the Claude Code sessions on an ssh host, and
                      open them in the tmux window they are running in. Repeat
                      it, or separate hosts with commas. With none named, the
                      hosts in <config>/savras/machines are watched — one per
                      line, # for comments — so plain `svr` keeps them.
                      `--machine off` watches none of them
  --jobs-dir <path>   read jobs from somewhere other than ~/.claude/jobs
  -h, --help          show this help
  -V, --version       show the version

examples:
  svr                          panel on the right, a shell beside it
  svr -- claude                panel on the right, Claude Code beside it
  svr --side left --width 52
  svr --open shell             start on a prompt instead of a session
  svr solo                     the panel on its own
  svr --once                   plain text, for scripts and status lines
  svr --machine claude-box     your machine's sessions and the box's, one list

keys, with the panel focused:
  ctrl-g     move the keyboard between your work and the panel
  ctrl-w/s   walk the panel's list, opening each session as you land,
             as do ctrl-shift-arrows where the terminal sends them
  ↑/↓, k/j   move        enter   open the selected session
  g/G        first/last  esc, q  back to your work
  ctrl-t     a tab of your own, running what Savras was started with,
             in the directory of the session you are on;
             n does the same with the panel focused. With machines
             written down it asks where first: 1 here, 2.. the box,
             and a tab on a box is a tmux window that survives it
  x          close the selected tab, yours or a session's
  a          start a parallel agent under this session's lead
  s          group by repository or by status
  < >        narrow or widen the panel     [ ]  put it left or right
  d          delete the selected session for good, after asking:
             `claude stop` then `claude rm`, which x does not do.
             On another machine there is no daemon to ask, so it
             stops the session's process over ssh instead
  r          refresh     Q       quit Savras, after asking
  ctrl-l     paint the screen again, if the terminal has scrolled it

In `svr solo` there is no working pane to go back to, so q quits.

A session arriving in Needs input pings: a sound, and a notification through
the terminal where the terminal understands one. `--ping done` pings for
finished sessions too. The session in the working pane is spared, but only
while the terminal has focus — behind another window it pings like any other.
";

/// How often to redraw. Ages tick in seconds, so this needs to be sub-second,
/// but a panel does not need to be a game.
const TICK: Duration = Duration::from_millis(250);
/// Filesystem events arrive in bursts as Claude Code rewrites state.json;
/// wait for the burst to settle before re-reading.
const DEBOUNCE: Duration = Duration::from_millis(120);
/// Re-read even without an event, so a missed notification cannot leave the
/// panel stale indefinitely.
const MAX_STALE: Duration = Duration::from_secs(3);

#[derive(Debug)]
enum Mode {
    Tui,
    Once,
    /// Print what the terminal sends for each key, and stop.
    Keys,
    Panel {
        width: u16,
        command: Vec<String>,
        dry_run: bool,
        tmux: bool,
        side: Side,
    },
}

#[derive(Debug)]
struct Options {
    mode: Mode,
    jobs_dir: Option<PathBuf>,
    ping: When,
    sound: bool,
    quiet: Duration,
    /// The keys that flip between tabs.
    switch: Switch,
    /// What the working pane starts on, once the default is worked out.
    open: host::Open,
    /// Whether the panel groups its rows by repository or by status.
    group_by: app::GroupBy,
    /// Other machines to watch, as ssh targets.
    machines: Vec<String>,
    /// `--machine off`: watch none, whatever is written down.
    no_machines: bool,
}

fn main() -> Result<()> {
    // `svr board` is a plain command, not a mode of the panel: it is what an
    // agent runs from a shell, and it must not be parsed as though somebody
    // asked for a terminal UI.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match board::dispatch(&argv) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            eprintln!("svr: {e}");
            std::process::exit(2);
        }
    }

    let mut options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return Ok(()),
        Err(e) => {
            eprintln!("svr: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // `--machine` names the machines for this run; with none named, the ones
    // you have written down are watched. A flag that has to be retyped every
    // time is a flag that gets forgotten, and a forgotten flag looks exactly
    // like a machine that has nothing running on it.
    if options.machines.is_empty() && !options.no_machines {
        options.machines = remote::configured();
    }

    // A panel inside a pane of another panel is two of everything: two lists
    // of the same sessions, two sets of the keys, and an `attach` opened twice
    // over the same session if you use them both. Nothing about it is what was
    // wanted, and the way to another pane is ctrl-t rather than a second copy.
    // `--once` is exempt: it is plain text, and a status line inside a pane is
    // a perfectly good place to want it.
    if host::nested() && !matches!(options.mode, Mode::Once | Mode::Keys) {
        eprintln!(
            "svr: Savras is already running in this terminal.\n\
             Press ctrl-t for a tab of your own, and ctrl-w/ctrl-s to flip between them."
        );
        std::process::exit(2);
    }

    if let Mode::Panel {
        width,
        command,
        dry_run,
        tmux,
        side,
    } = options.mode
    {
        if tmux || dry_run {
            return panel::run(width, side, command, dry_run);
        }
        return host::run(host::Setup {
            width,
            side,
            command,
            jobs_dir: jobs_dir(options.jobs_dir)?,
            switch: options.switch,
            open: options.open,
            group_by: options.group_by,
            ping: Ping::new(options.ping, options.sound, options.quiet),
            machines: options.machines,
        });
    }

    let jobs_dir = jobs_dir(options.jobs_dir)?;

    match options.mode {
        Mode::Keys => host::keys(options.switch),
        Mode::Once => print_once(&jobs_dir),
        _ => run_tui(
            jobs_dir,
            options.group_by,
            Ping::new(options.ping, options.sound, options.quiet),
            options.machines,
        ),
    }
}

/// `--switch ws` is Ctrl-W back and Ctrl-S forward, `--switch o` is one key
/// that wraps forward, and `--switch off` hands both back to the child.
///
/// Control bytes, not chords: they are the only keys every terminal delivers
/// unchanged, which is the whole reason this option exists.
fn parse_switch(raw: &str) -> Result<Switch> {
    if raw == "off" || raw == "none" {
        return Ok(Switch::OFF);
    }
    let letters: Vec<char> = raw.chars().collect();
    match letters.as_slice() {
        // Two letters are back and forward, in that order — `ws` is the
        // default written out.
        [back, forward] => {
            let (back, forward) = (control(*back)?, control(*forward)?);
            if back == forward {
                anyhow::bail!(
                    "--switch {raw} binds one key to both directions; \
                     use a single letter if you want one key that wraps forward"
                );
            }
            Ok(Switch {
                back: Some(back),
                forward: Some(forward),
            })
        }
        // One letter only goes forward, and wraps. With two tabs open, which
        // is the usual number, forward and back are the same place anyway.
        [only] => Ok(Switch {
            back: None,
            forward: Some(control(*only)?),
        }),
        _ => anyhow::bail!("--switch takes one or two letters, or off, not {raw}"),
    }
}

/// The control byte a letter makes, refusing the ones that are already
/// something else.
fn control(letter: char) -> Result<u8> {
    if !letter.is_ascii_alphabetic() {
        anyhow::bail!("--switch takes letters, not {letter}");
    }
    let letter = letter.to_ascii_lowercase();
    // Letters whose control byte is already something else entirely: g, l and
    // t are Savras's own keys, and the rest are enter, tab, backspace and the
    // signals, none of which can be handed to anything.
    if let Some(what) = match letter {
        'g' => Some("ctrl-g, which focuses the panel"),
        'l' => Some("ctrl-l, which repaints the screen"),
        't' => Some("ctrl-t, which opens a tab"),
        'c' => Some("ctrl-c, which interrupts"),
        'd' => Some("ctrl-d, which is end-of-file"),
        'h' => Some("backspace"),
        'i' => Some("tab"),
        'j' | 'm' => Some("enter"),
        _ => None,
    } {
        anyhow::bail!("--switch {letter} is {what}; pick another letter");
    }
    // Ctrl-<letter> is the letter with its top three bits cleared.
    Ok(letter.to_ascii_uppercase() as u8 & 0x1f)
}

fn jobs_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None => job::default_jobs_dir(),
    }
}

fn parse_args() -> Result<Option<Options>> {
    parse(std::env::args().skip(1).collect())
}

fn parse(args: Vec<String>) -> Result<Option<Options>> {
    let mut options = Options {
        machines: Vec::new(),
        no_machines: false,
        mode: Mode::Tui,
        jobs_dir: None,
        ping: When::default(),
        sound: true,
        quiet: ping::QUIET,
        switch: Switch::default(),
        // Filled in below: what the pane starts on depends on whether you
        // named a command for it.
        open: host::Open::Shell,
        group_by: app::GroupBy::Repo,
    };
    let mut open: Option<host::Open> = None;
    let mut width = panel::DEFAULT_WIDTH;
    let mut command = Vec::new();
    // The side panel is the point of the tool, so it is what you get by
    // default; `solo` is the older behaviour of a panel with nothing beside it.
    let mut is_panel = true;
    let mut dry_run = false;
    let mut tmux = false;
    let mut side = Side::Right;

    let mut args = args.into_iter().peekable();
    match args.peek().map(String::as_str) {
        // `panel` is still accepted; it is what this used to be called.
        Some("panel") => {
            args.next();
        }
        Some("solo") => {
            args.next();
            is_panel = false;
        }
        _ => {}
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("savras {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            // Everything after `--` is the command for the working pane, and
            // must not be read as an option of ours.
            "--" => {
                command.extend(args.by_ref());
                break;
            }
            "--once" => {
                options.mode = Mode::Once;
                is_panel = false;
            }
            "--keys" => {
                options.mode = Mode::Keys;
                is_panel = false;
            }
            "--no-sound" => options.sound = false,
            "--open" => {
                let raw = args
                    .next()
                    .context("--open needs shell, top, or a session name")?;
                open = Some(host::Open::parse(&raw));
            }
            "--machine" => {
                let host = args.next().context("--machine needs an ssh host")?;
                // `off` for the run you want kept to this machine — and for
                // the tests, which must not depend on what you have written
                // down or on a box being reachable.
                if host == "off" || host == "none" {
                    options.no_machines = true;
                    options.machines.clear();
                    continue;
                }
                for host in host.split(',').map(str::trim).filter(|h| !h.is_empty()) {
                    options.machines.push(host.to_string());
                }
            }
            "--group" => {
                let raw = args.next().context("--group needs repo or status")?;
                options.group_by = app::GroupBy::parse(&raw)
                    .with_context(|| format!("--group must be repo or status, not {raw}"))?;
            }
            "--switch" => {
                let raw = args.next().context("--switch needs a letter, or off")?;
                options.switch = parse_switch(&raw)?;
            }
            "--quiet" => {
                let raw = args.next().context("--quiet needs a number of seconds")?;
                let secs: f64 = raw
                    .parse()
                    .with_context(|| format!("--quiet must be a number of seconds, not {raw}"))?;
                if !(0.0..=600.0).contains(&secs) {
                    anyhow::bail!("--quiet must be between 0 and 600 seconds, not {raw}");
                }
                options.quiet = Duration::from_secs_f64(secs);
            }
            "--ping" => {
                let raw = args.next().context("--ping needs needs, done or off")?;
                options.ping = When::parse(&raw)
                    .with_context(|| format!("--ping must be needs, done or off, not {raw}"))?;
            }
            "--dry-run" => dry_run = true,
            "--tmux" => tmux = true,
            "--side" => {
                let raw = args.next().context("--side needs left or right")?;
                side = match raw.as_str() {
                    "left" => Side::Left,
                    "right" => Side::Right,
                    other => anyhow::bail!("--side must be left or right, not {other}"),
                };
            }
            "--width" => {
                let raw = args.next().context("--width needs a number")?;
                width = raw
                    .parse()
                    .with_context(|| format!("--width {raw} is not a number"))?;
                if width < 12 {
                    anyhow::bail!("--width must be at least 12 columns");
                }
            }
            "--jobs-dir" => {
                let dir = args.next().context("--jobs-dir needs a path")?;
                options.jobs_dir = Some(PathBuf::from(dir));
            }
            other => anyhow::bail!("unknown option {other}"),
        }
    }

    // The session at the top of the list is what you came for; an empty shell
    // beside a panel full of sessions is a prompt you have to do something
    // with before anything happens. But a command after `--` is you saying
    // what the pane is for, and Savras does not know better.
    options.open = open.unwrap_or(if command.is_empty() {
        host::Open::Top
    } else {
        host::Open::Shell
    });

    if is_panel {
        options.mode = Mode::Panel {
            width,
            command,
            dry_run,
            tmux,
            side,
        };
    } else if !command.is_empty() {
        anyhow::bail!("a command after `--` needs a working pane to run in, which `svr solo` and `--once` do not have");
    }
    Ok(Some(options))
}

/// Plain-text snapshot: useful in scripts, status lines, and anywhere without
/// a TTY.
fn print_once(jobs_dir: &std::path::Path) -> Result<()> {
    let snapshot = job::load(jobs_dir)?;
    if snapshot.is_empty() {
        println!("no Claude Code sessions");
        return Ok(());
    }
    let now = Utc::now();
    for status in [
        job::Status::NeedsInput,
        job::Status::Working,
        job::Status::Done,
    ] {
        let group: Vec<_> = snapshot
            .jobs
            .iter()
            .filter(|j| j.status == status)
            .collect();
        if group.is_empty() {
            continue;
        }
        println!("{}", status.heading());
        for j in group {
            // The same age the panel shows: how long the session has been
            // open, not how recently it spoke. See `Job::created_at`.
            println!(
                "  {:<12} {:<7} {:>4}  {}",
                j.name,
                j.word(),
                job::age(j.created_at.or(j.updated_at), now),
                j.summary
            );
        }
    }
    Ok(())
}

fn run_tui(
    jobs_dir: PathBuf,
    group_by: app::GroupBy,
    ping: Ping,
    machines: Vec<String>,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, jobs_dir, group_by, ping, machines);
    restore_terminal(&mut terminal)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    jobs_dir: PathBuf,
    group_by: app::GroupBy,
    mut ping: Ping,
    machines: Vec<String>,
) -> Result<()> {
    let mut app = App::new(jobs_dir.clone());
    app.set_group_by(group_by);
    // The panel App::new already loaded is the state of the world, not news.
    ping.poll(&app.snapshot, None);

    let watch = Watch::start(&jobs_dir);
    app.watching = watch.live;
    let machines = remote::watch(&machines);

    let mut dirty_since: Option<Instant> = None;
    let mut last_load = Instant::now();

    // Starting an agent takes long enough to stall a panel that redraws on a
    // tick, so it happens on a thread and reports back here — the same
    // arrangement the side panel uses.
    let (starting, started) = std::sync::mpsc::channel::<Result<String>>();

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut app, key, &starting)
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        agents::settle(&mut app, &started);
        while let Ok(news) = machines.try_recv() {
            match news {
                remote::News::Running(host, jobs) => app.set_remote(host, jobs),
                remote::News::Trouble(host, why) => app.error = Some(format!("{host}: {why}")),
            }
        }

        if watch.changed() {
            dirty_since.get_or_insert_with(Instant::now);
        }

        let settled = dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE);
        if settled || last_load.elapsed() >= MAX_STALE {
            app.refresh();
            // Nothing is open beside the panel in this mode, so every session
            // is one you are not looking at.
            let pinged = ping.poll(&app.snapshot, None);
            app.alert(pinged);
            dirty_since = None;
            last_load = Instant::now();
        }
    }
    Ok(())
}

/// The keys `svr solo` answers to.
///
/// Fewer than the side panel's, and the difference is not arbitrary: opening a
/// session, closing a tab and flipping between tabs all need a working pane,
/// and solo is the panel with nothing beside it. Starting an agent needs no
/// pane, so `a` belongs in both.
fn handle_key(app: &mut App, key: KeyEvent, starting: &std::sync::mpsc::Sender<Result<String>>) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true
        }
        KeyCode::Down | KeyCode::Char('j') => app.step(1),
        KeyCode::Up | KeyCode::Char('k') => app.step(-1),
        KeyCode::Char('g') | KeyCode::Home => app.jump(false),
        KeyCode::Char('G') | KeyCode::End => app.jump(true),
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Char('s') => {
            app.regroup();
        }
        KeyCode::Char('a') => agents::add(app, starting),
        _ => {}
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // A panic in raw mode leaves the terminal unusable; put it back first.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Options {
        parse(args.iter().map(|s| s.to_string()).collect())
            .unwrap()
            .expect("not a help/version exit")
    }

    #[test]
    fn no_arguments_opens_the_side_panel_beside_your_work() {
        assert!(matches!(parse_ok(&[]).mode, Mode::Panel { .. }));
        // `panel` is what this used to be called, and still works.
        assert!(matches!(parse_ok(&["panel"]).mode, Mode::Panel { .. }));
    }

    #[test]
    fn solo_is_the_panel_with_nothing_beside_it() {
        assert!(matches!(parse_ok(&["solo"]).mode, Mode::Tui));
    }

    #[test]
    fn a_command_needs_a_pane_to_run_in() {
        assert!(parse(vec!["solo".into(), "--".into(), "claude".into()]).is_err());
        assert!(parse(vec!["--once".into(), "--".into(), "claude".into()]).is_err());
        // But the default mode has a pane, so this is fine.
        assert!(matches!(
            parse_ok(&["--", "claude"]).mode,
            Mode::Panel { .. }
        ));
    }

    #[test]
    fn panel_takes_a_width_and_a_command() {
        let options = parse_ok(&["--width", "52", "--", "claude", "--resume", "x"]);
        match options.mode {
            Mode::Panel { width, command, .. } => {
                assert_eq!(width, 52);
                assert_eq!(command, ["claude", "--resume", "x"]);
            }
            _ => panic!("expected panel mode"),
        }
    }

    #[test]
    fn the_pane_starts_on_the_top_session_unless_you_said_otherwise() {
        // An empty shell beside a panel full of sessions is a prompt you have
        // to do something with before anything happens.
        assert_eq!(parse_ok(&[]).open, host::Open::Top);
        // ...but a command after `--` is you saying what the pane is for.
        assert_eq!(parse_ok(&["--", "claude"]).open, host::Open::Shell);
        // And either way it can be said outright.
        assert_eq!(parse_ok(&["--open", "shell"]).open, host::Open::Shell);
        assert_eq!(
            parse_ok(&["--open", "PLAN", "--", "claude"]).open,
            host::Open::Named("PLAN".into())
        );
    }

    #[test]
    fn options_after_the_separator_belong_to_the_command_not_to_us() {
        // `--once` here is Claude Code's flag, not ours; parsing it would send
        // the user somewhere they did not ask to go.
        let options = parse_ok(&["--", "claude", "--once", "--width", "9"]);
        match options.mode {
            Mode::Panel { width, command, .. } => {
                assert_eq!(width, panel::DEFAULT_WIDTH);
                assert_eq!(command, ["claude", "--once", "--width", "9"]);
            }
            _ => panic!("expected panel mode"),
        }
    }

    #[test]
    fn the_panel_hosts_the_pane_itself_unless_tmux_is_asked_for() {
        // The default needs no tmux installed; --tmux opts into it for the
        // detach and reattach that hosting cannot give.
        match parse_ok(&["panel"]).mode {
            Mode::Panel { tmux, .. } => assert!(!tmux),
            _ => panic!("expected panel mode"),
        }
        match parse_ok(&["panel", "--tmux"]).mode {
            Mode::Panel { tmux, .. } => assert!(tmux),
            _ => panic!("expected panel mode"),
        }
    }

    #[test]
    fn the_panel_sits_on_the_right_unless_asked_otherwise() {
        match parse_ok(&["panel"]).mode {
            Mode::Panel { side, .. } => assert_eq!(side, Side::Right),
            _ => panic!("expected panel mode"),
        }
        match parse_ok(&["panel", "--side", "left"]).mode {
            Mode::Panel { side, .. } => assert_eq!(side, Side::Left),
            _ => panic!("expected panel mode"),
        }
        assert!(parse(vec!["panel".into(), "--side".into(), "up".into()]).is_err());
    }

    #[test]
    fn a_width_too_small_to_read_is_rejected() {
        assert!(parse(vec!["panel".into(), "--width".into(), "3".into()]).is_err());
        assert!(parse(vec!["panel".into(), "--width".into(), "wide".into()]).is_err());
    }

    #[test]
    fn pinging_is_on_for_questions_and_off_for_everything_else() {
        // The default has to be useful without being a car alarm: a session
        // asking you something is blocking; a session finishing is not.
        let options = parse_ok(&[]);
        assert_eq!(options.ping, When::Needs);
        assert!(options.sound);

        assert_eq!(parse_ok(&["--ping", "done"]).ping, When::Done);
        assert_eq!(parse_ok(&["--ping", "off"]).ping, When::Off);
        assert!(!parse_ok(&["--no-sound"]).sound);
        assert!(parse(vec!["--ping".into(), "loud".into()]).is_err());
        assert!(parse(vec!["--ping".into()]).is_err());
    }

    #[test]
    fn the_switch_keys_can_be_moved_or_given_back() {
        assert_eq!(parse_ok(&[]).switch, Switch::default());
        assert_eq!(
            parse_ok(&["--switch", "ws"]).switch,
            Switch::default(),
            "the default, written out"
        );
        // Two letters are back and forward, in that order.
        assert_eq!(
            parse_ok(&["--switch", "OU"]).switch,
            Switch {
                back: Some(0x0f),
                forward: Some(0x15)
            }
        );
        // One letter only goes forward, and wraps.
        assert_eq!(
            parse_ok(&["--switch", "o"]).switch,
            Switch {
                back: None,
                forward: Some(0x0f)
            }
        );
        assert_eq!(parse_ok(&["--switch", "off"]).switch, Switch::OFF);

        // Letters whose control byte is already something else say so, rather
        // than silently binding a key that can never arrive.
        for taken in ["g", "l", "c", "d", "i", "m"] {
            assert!(
                parse(vec!["--switch".into(), taken.into()]).is_err(),
                "--switch {taken} must be refused"
            );
        }
        assert!(parse(vec!["--switch".into(), "oo".into()]).is_err());
        assert!(parse(vec!["--switch".into(), "1".into()]).is_err());
        assert!(parse(vec!["--switch".into()]).is_err());
    }

    #[test]
    fn the_quiet_period_can_be_moved_but_not_to_nonsense() {
        assert_eq!(parse_ok(&[]).quiet, ping::QUIET);
        assert_eq!(
            parse_ok(&["--quiet", "1.5"]).quiet,
            Duration::from_millis(1500)
        );
        // Nothing is held back at all, which is a choice someone may want.
        assert_eq!(parse_ok(&["--quiet", "0"]).quiet, Duration::ZERO);
        assert!(parse(vec!["--quiet".into(), "ages".into()]).is_err());
        assert!(parse(vec!["--quiet".into(), "-1".into()]).is_err());
        assert!(parse(vec!["--quiet".into(), "9999".into()]).is_err());
        assert!(parse(vec!["--quiet".into()]).is_err());
    }

    #[test]
    fn ping_options_before_the_separator_are_ours_and_after_it_are_not() {
        let options = parse_ok(&["--ping", "off", "--", "claude", "--ping", "done"]);
        assert_eq!(options.ping, When::Off);
        match options.mode {
            Mode::Panel { command, .. } => assert_eq!(command, ["claude", "--ping", "done"]),
            _ => panic!("expected panel mode"),
        }
    }

    #[test]
    fn unknown_options_are_rejected_rather_than_ignored() {
        assert!(parse(vec!["--colour".into()]).is_err());
    }
}
