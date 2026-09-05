//! Savras — a side panel that sees every Claude Code session you have running.
//!
//! Read-only by design: it watches `~/.claude/jobs/` and never writes there,
//! so it cannot disturb the sessions it reports on.

mod app;
mod job;
mod panel;
#[cfg(test)]
mod testing;
mod ui;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use notify::{RecursiveMode, Watcher};
use ratatui::prelude::*;

use app::App;

const USAGE: &str = "\
savras — see every Claude Code session you have running

usage: svr [options]
       svr panel [--width <cols>] [-- <command>...]

commands:
  panel               open the panel as a column on the left of this terminal,
                      with your work beside it (uses tmux)

options:
  --width <cols>      width of the panel column (panel only, default 44)
  --dry-run           print the tmux layout instead of building it (panel only)
  --once              print the current sessions as plain text and exit
  --jobs-dir <path>   read jobs from somewhere other than ~/.claude/jobs
  -h, --help          show this help
  -V, --version       show the version

examples:
  svr panel                    panel on the left, a shell on the right
  svr panel -- claude          panel on the left, Claude Code on the right
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
    } = options.mode
    {
        return panel::run(width, command, dry_run);
    }

    let jobs_dir = match options.jobs_dir {
        Some(dir) => dir,
        None => job::default_jobs_dir()?,
    };

    match options.mode {
        Mode::Once => print_once(&jobs_dir),
        _ => run_tui(jobs_dir),
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

    // Keep the watcher alive for the whole loop; dropping it stops events.
    let (tx, rx) = mpsc::channel();
    let watcher = start_watcher(&jobs_dir, tx);
    app.watching = watcher.is_some();

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

        if drain(&rx) {
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

/// Watch the jobs directory. Returns `None` if watching is unavailable, in
/// which case the caller falls back to polling.
fn start_watcher(
    jobs_dir: &std::path::Path,
    tx: mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    // Watching a directory that does not exist fails; watch the parent so the
    // panel comes alive the moment Claude Code creates it.
    let target = if jobs_dir.exists() {
        jobs_dir.to_path_buf()
    } else {
        jobs_dir.parent()?.to_path_buf()
    };
    if !target.exists() {
        return None;
    }

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&target, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}

/// Collapse a burst of events into a single "something changed".
fn drain(rx: &Receiver<()>) -> bool {
    let mut any = false;
    loop {
        match rx.try_recv() {
            Ok(()) => any = true,
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return any,
        }
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

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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
