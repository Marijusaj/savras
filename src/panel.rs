//! `svr panel` — put the panel in a column on the left of the terminal you are
//! already working in.
//!
//! Terminal.app, and every other emulator without split panes, cannot do this
//! alone, so the layout is built with tmux. The tmux is meant to be invisible:
//! no status bar, no key bindings to learn, just a divider down the left of the
//! window. It is also the only mechanism that works the same on macOS, Linux
//! and Windows (WSL), which the alternatives — per-emulator scripting — do not.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context as _, Result};

use crate::host::Side;

/// The tmux session Savras creates when you are not already in one.
const SESSION: &str = "savras";

/// Default width of the left column, in columns. Wide enough for a name, a
/// readable slice of summary, and the age.
pub const DEFAULT_WIDTH: u16 = 44;

/// Config for the session Savras starts. Deliberately minimal: it makes tmux
/// look like a plain terminal with a divider, and is only ever applied to a
/// server Savras starts itself, so it cannot disturb an existing setup.
const TMUX_CONF: &str = "\
# Written by savras. Applied only to the tmux server savras starts.
set -g mouse on
set -g escape-time 10
set -g history-limit 50000
set -g focus-events on
set -g status off
set -g pane-border-style fg=colour238
set -g pane-active-border-style fg=colour238
set -g base-index 0
setw -g pane-base-index 0
";

/// Everything needed to decide the tmux commands, gathered so the decision
/// itself stays pure and testable.
#[derive(Debug, Clone)]
pub struct Context {
    pub inside_tmux: bool,
    pub session_exists: bool,
    pub width: u16,
    pub side: Side,
    pub size: (u16, u16),
    pub svr: String,
    pub cwd: String,
    pub config: String,
    /// What to run in the working pane. Empty means the user's shell.
    pub command: Vec<String>,
}

/// tmux invocations to make, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Run and wait for each.
    pub steps: Vec<Vec<String>>,
    /// Replace this process with this one, if any (attaching to the session).
    pub exec: Option<Vec<String>>,
}

pub fn plan(ctx: &Context) -> Plan {
    let width = ctx.width.to_string();

    // Already in tmux: add the column to the window in front of the user and
    // hand focus straight back, so the panel appears beside their work.
    if ctx.inside_tmux {
        // `-b` puts the new pane before the current one, i.e. on the left.
        let direction = match ctx.side {
            Side::Left => "-hb",
            Side::Right => "-h",
        };
        return Plan {
            steps: vec![
                vec![
                    "split-window".into(),
                    direction.into(),
                    "-l".into(),
                    width,
                    "-c".into(),
                    ctx.cwd.clone(),
                    ctx.svr.clone(),
                ],
                // `-l` is "last active pane" — the one they were working in.
                vec!["select-pane".into(), "-l".into()],
            ],
            exec: None,
        };
    }

    // A session is already laid out: just go to it.
    if ctx.session_exists {
        return Plan {
            steps: vec![],
            exec: Some(vec!["attach-session".into(), "-t".into(), SESSION.into()]),
        };
    }

    // Build the session detached, then attach. The panel is pane 0 (left) so
    // the layout is deterministic; the working pane is 1 and gets the focus.
    let (cols, rows) = ctx.size;
    // Pane 0 is the leftmost pane, so which program starts the session depends
    // on which side the panel is on.
    let (first, second) = match ctx.side {
        Side::Left => (ctx.svr.clone(), command_or_shell(ctx)),
        Side::Right => (command_or_shell(ctx), ctx.svr.clone()),
    };
    let panel_pane = match ctx.side {
        Side::Left => 0,
        Side::Right => 1,
    };
    let work_pane = 1 - panel_pane;
    Plan {
        steps: vec![
            vec![
                "-f".into(),
                ctx.config.clone(),
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                SESSION.into(),
                "-n".into(),
                "main".into(),
                // Size it like the real terminal so attaching does not reflow
                // the column away from the width we asked for.
                "-x".into(),
                cols.to_string(),
                "-y".into(),
                rows.to_string(),
                "-c".into(),
                ctx.cwd.clone(),
                first,
            ],
            {
                let mut split = vec![
                    "split-window".into(),
                    "-h".into(),
                    "-t".into(),
                    format!("{SESSION}:main.0"),
                    "-c".into(),
                    ctx.cwd.clone(),
                ];
                if !second.is_empty() {
                    split.push(second);
                }
                split
            },
            vec![
                "resize-pane".into(),
                "-t".into(),
                format!("{SESSION}:main.{panel_pane}"),
                "-x".into(),
                width,
            ],
            vec![
                "select-pane".into(),
                "-t".into(),
                format!("{SESSION}:main.{work_pane}"),
            ],
        ],
        exec: Some(vec!["attach-session".into(), "-t".into(), SESSION.into()]),
    }
}

/// The working pane runs the given command, or nothing at all, which leaves
/// tmux to start the user's shell.
fn command_or_shell(ctx: &Context) -> String {
    if ctx.command.is_empty() {
        String::new()
    } else {
        shell_join(&ctx.command)
    }
}

/// Build the layout and hand the terminal over to tmux.
pub fn run(width: u16, side: Side, command: Vec<String>, dry_run: bool) -> Result<()> {
    if !dry_run && which_tmux().is_none() {
        bail!("{}", TMUX_MISSING);
    }

    let ctx = Context {
        inside_tmux: std::env::var_os("TMUX").is_some(),
        session_exists: session_exists(),
        width,
        side,
        size: crossterm::terminal::size().unwrap_or((160, 45)),
        svr: shell_quote(&current_exe()?),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into()),
        config: write_config()?,
        command,
    };

    let plan = plan(&ctx);

    // `--dry-run` prints the layout instead of building it, so what Savras
    // does to your terminal can be read before it happens.
    if dry_run {
        for step in plan.steps.iter().chain(plan.exec.iter()) {
            println!("tmux {}", shell_join(step));
        }
        return Ok(());
    }

    for step in &plan.steps {
        let status = Command::new("tmux")
            .args(step)
            .status()
            .context("running tmux")?;
        if !status.success() {
            bail!(
                "tmux {} failed",
                step.first().map(String::as_str).unwrap_or("")
            );
        }
    }

    if let Some(exec) = plan.exec {
        let status = Command::new("tmux")
            .args(&exec)
            .status()
            .context("running tmux")?;
        if !status.success() {
            bail!("could not attach to the savras session");
        }
    }
    Ok(())
}

const TMUX_MISSING: &str = "\
the side panel needs tmux, which is not installed.

  macOS    brew install tmux
  Debian   sudo apt install tmux
  Fedora   sudo dnf install tmux
  Arch     sudo pacman -S tmux
  Windows  use WSL, or run `svr` in a Windows Terminal split pane

Terminal emulators without split panes cannot show a side column on their own,
which is what tmux is here for. Plain `svr` still works in its own tab.";

fn which_tmux() -> Option<()> {
    Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}

fn session_exists() -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", SESSION])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn current_exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("finding the savras binary")?
        .to_string_lossy()
        .to_string())
}

/// Where the generated tmux config lives. Written every run so an upgrade to
/// Savras updates it, and kept out of the user's own tmux config entirely.
fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "savras")
        .context("could not determine a config directory")?;
    Ok(dirs.config_dir().join("tmux.conf"))
}

fn write_config() -> Result<String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, TMUX_CONF).with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_string_lossy().to_string())
}

/// tmux takes a command as one shell string, so a path or argument containing
/// spaces has to survive a trip through the shell.
fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            inside_tmux: false,
            session_exists: false,
            width: 44,
            side: Side::Left,
            size: (180, 45),
            svr: "/usr/local/bin/svr".into(),
            cwd: "/home/me/code".into(),
            config: "/cfg/tmux.conf".into(),
            command: vec![],
        }
    }

    fn joined(plan: &Plan) -> String {
        let mut all: Vec<String> = plan.steps.iter().map(|s| s.join(" ")).collect();
        if let Some(e) = &plan.exec {
            all.push(e.join(" "));
        }
        all.join("\n")
    }

    #[test]
    fn a_fresh_session_puts_the_panel_left_and_the_focus_right() {
        let p = plan(&ctx());
        let text = joined(&p);

        assert!(text.contains("new-session -d -s savras"));
        // Pane 0 is the panel, so the layout is deterministic.
        assert!(text.contains("-c /home/me/code /usr/local/bin/svr"));
        assert!(text.contains("split-window -h -t savras:main.0"));
        assert!(text.contains("resize-pane -t savras:main.0 -x 44"));
        // The user lands in the working pane, not the panel.
        assert!(text.contains("select-pane -t savras:main.1"));
        assert_eq!(p.exec.unwrap(), vec!["attach-session", "-t", "savras"]);
    }

    #[test]
    fn on_the_right_the_panel_becomes_pane_one_and_the_work_pane_pane_zero() {
        let mut c = ctx();
        c.side = Side::Right;
        c.command = vec!["claude".into()];
        let steps = plan(&c).steps;

        // Pane 0 is the leftmost, so with the panel on the right it is the work.
        assert!(steps[0].join(" ").ends_with("claude"), "{:?}", steps[0]);
        assert!(
            steps[1].join(" ").ends_with("/usr/local/bin/svr"),
            "{:?}",
            steps[1]
        );
        assert!(steps[2].join(" ").contains("savras:main.1 -x 44"));
        assert!(steps[3].join(" ").contains("savras:main.0"));
    }

    #[test]
    fn inside_tmux_the_new_pane_goes_to_the_asked_for_side() {
        let mut c = ctx();
        c.inside_tmux = true;
        c.side = Side::Right;
        assert!(plan(&c).steps[0]
            .join(" ")
            .contains("split-window -h -l 44"));

        c.side = Side::Left;
        assert!(plan(&c).steps[0]
            .join(" ")
            .contains("split-window -hb -l 44"));
    }

    #[test]
    fn the_session_is_sized_like_the_real_terminal() {
        // Otherwise attaching reflows the panes and the column loses its width.
        let text = joined(&plan(&ctx()));
        assert!(text.contains("-x 180 -y 45"), "{text}");
    }

    #[test]
    fn a_command_runs_in_the_working_pane_not_the_panel() {
        let mut c = ctx();
        c.command = vec!["claude".into(), "--resume".into(), "abc".into()];
        let steps = plan(&c).steps;

        let new_session = steps[0].join(" ");
        assert!(!new_session.contains("claude"), "panel pane must run svr");
        assert!(steps[1].join(" ").ends_with("claude --resume abc"));
    }

    #[test]
    fn inside_tmux_it_splits_the_window_in_front_of_you() {
        let mut c = ctx();
        c.inside_tmux = true;
        let p = plan(&c);

        assert!(p.exec.is_none(), "must not attach from inside tmux");
        assert!(p.steps[0].join(" ").contains("split-window -hb -l 44"));
        // Focus goes back to the pane they were working in.
        assert_eq!(p.steps[1], vec!["select-pane", "-l"]);
    }

    #[test]
    fn an_existing_session_is_attached_not_rebuilt() {
        let mut c = ctx();
        c.session_exists = true;
        let p = plan(&c);

        assert!(p.steps.is_empty(), "must not duplicate the layout");
        assert_eq!(p.exec.unwrap(), vec!["attach-session", "-t", "savras"]);
    }

    #[test]
    fn paths_and_arguments_with_spaces_survive_the_shell() {
        let mut c = ctx();
        c.svr = shell_quote("/Users/me/My Code/svr");
        c.command = vec!["claude".into(), "a b".into(), "it's".into()];
        let text = joined(&plan(&c));

        assert!(text.contains("'/Users/me/My Code/svr'"), "{text}");
        assert!(text.contains(r"claude 'a b' 'it'\''s'"), "{text}");
    }

    #[test]
    fn ordinary_paths_are_left_alone() {
        assert_eq!(shell_quote("/usr/local/bin/svr"), "/usr/local/bin/svr");
        assert_eq!(shell_quote("claude"), "claude");
    }

    #[test]
    fn the_generated_config_hides_tmux_rather_than_showing_it() {
        assert!(TMUX_CONF.contains("set -g status off"));
        assert!(TMUX_CONF.contains("set -g mouse on"));
    }
}
