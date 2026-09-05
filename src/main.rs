//! Savras — a side panel that sees every Claude Code session you have running.
//!
//! Read-only by design: it watches `~/.claude/jobs/` and never writes there,
//! so it cannot disturb the sessions it reports on.

mod app;
mod host;
mod job;
mod panel;
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
use host::Side;
use watch::Watch;

const USAGE: &str = "\
savras — see every Claude Code session you have running

usage: svr [options]
       svr panel [--width <cols>] [-- <command>...]

commands:
  panel               open the panel as a column on the left of this terminal,
                      with your work beside it (uses tmux)

options:
  --side <left|right> which side the panel sits on (panel only, default right)
  --width <cols>      width of the panel column (panel only, default 44)
  --tmux              build the panel with tmux instead of hosting it directly
  --dry-run           print the tmux layout instead of building it (panel only)
  --once              print the current sessions as plain text and exit
  --jobs-dir <path>   read jobs from somewhere other than ~/.claude/jobs
  -h, --help          show this help
  -V, --version       show the version

examples:
  svr panel                    panel on the left, a shell on the right
  svr panel -- claude          panel on the left, Claude Code on the right
  svr panel --side left        panel on the left instead
  svr panel --width 52

keys:
  ↑/↓, k/j   move        g/G  first/last
  r          refresh     q    quit
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
}

fn main() -> Result<()> {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return Ok(()),
        Err(e) => {
            eprintln!("svr: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

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
        return host::run(width, side, command, jobs_dir(options.jobs_dir)?);
    }

    let jobs_dir = jobs_dir(options.jobs_dir)?;

    match options.mode {
        Mode::Once => print_once(&jobs_dir),
        _ => run_tui(jobs_dir),
    }
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
        mode: Mode::Tui,
        jobs_dir: None,
    };
    let mut width = panel::DEFAULT_WIDTH;
    let mut command = Vec::new();
    let mut is_panel = false;
    let mut dry_run = false;
    let mut tmux = false;
    let mut side = Side::Right;

    let mut args = args.into_iter().peekable();
    if args.peek().map(String::as_str) == Some("panel") {
        args.next();
        is_panel = true;
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
            "--once" => options.mode = Mode::Once,
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

    if is_panel {
        options.mode = Mode::Panel {
            width,
            command,
            dry_run,
            tmux,
            side,
        };
    } else if !command.is_empty() {
        anyhow::bail!("a command after `--` only makes sense with `svr panel`");
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
            println!(
                "  {:<12} {:>4}  {}",
                j.name,
                job::age(j.updated_at, now),
                j.summary
            );
        }
    }
    Ok(())
}

fn run_tui(jobs_dir: PathBuf) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, jobs_dir);
    restore_terminal(&mut terminal)?;
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, jobs_dir: PathBuf) -> Result<()> {
    let mut app = App::new(jobs_dir.clone());

    let watch = Watch::start(&jobs_dir);
    app.watching = watch.live;

    let mut dirty_since: Option<Instant> = None;
    let mut last_load = Instant::now();

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(&mut app, key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if watch.changed() {
            dirty_since.get_or_insert_with(Instant::now);
        }

        let settled = dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE);
        if settled || last_load.elapsed() >= MAX_STALE {
            app.refresh();
            dirty_since = None;
            last_load = Instant::now();
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
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
    fn no_arguments_opens_the_panel_in_this_terminal() {
        assert!(matches!(parse_ok(&[]).mode, Mode::Tui));
    }

    #[test]
    fn panel_takes_a_width_and_a_command() {
        let options = parse_ok(&["panel", "--width", "52", "--", "claude", "--resume", "x"]);
        match options.mode {
            Mode::Panel { width, command, .. } => {
                assert_eq!(width, 52);
                assert_eq!(command, ["claude", "--resume", "x"]);
            }
            _ => panic!("expected panel mode"),
        }
    }

    #[test]
    fn options_after_the_separator_belong_to_the_command_not_to_us() {
        // `--once` here is Claude Code's flag, not ours; parsing it would send
        // the user somewhere they did not ask to go.
        let options = parse_ok(&["panel", "--", "claude", "--once", "--width", "9"]);
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
    fn a_command_without_panel_is_a_mistake_worth_naming() {
        let err = parse(vec!["--".into(), "claude".into()]).unwrap_err();
        assert!(err.to_string().contains("svr panel"), "{err}");
    }

    #[test]
    fn unknown_options_are_rejected_rather_than_ignored() {
        assert!(parse(vec!["--colour".into()]).is_err());
    }
}
