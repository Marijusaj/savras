//! tmux, driven: the panel is one pane of a tmux window, and every tab is a
//! real tmux pane that takes its turn in the slot beside it.
//!
//! Hosting the panes ourselves meant being a terminal: reading each child
//! through `vt100`, redrawing its cells, and passing on by hand everything a
//! terminal carries besides cells — mouse modes, bracketed paste, focus,
//! clipboard copies, links, the selection. Each of those was a bug until
//! someone noticed it was missing. tmux has done all of them for years, so it
//! owns the terminal now and Savras is the panel. See
//! `docs/decisions/2026-09-30-tmux-owns-the-terminal.md`.
//!
//! This module is the only place that speaks tmux's command language. The
//! rest of Savras asks it for a pane, to show a pane, or what a pane is doing.
//!
//! **Tabs are windows.** Each one is a pane in a window of its own, behind the
//! window you are looking at. Showing a tab swaps its pane into the slot
//! beside the panel, so the program in it never restarts and never notices
//! beyond a resize.
//!
//! **Keys come back as bytes.** The keys that were Savras's — ctrl-g, ctrl-t,
//! the flip keys — are bound in tmux, and a binding hands its key to the panel
//! pane wrapped in a private sequence ([`wrap`]), so the panel reads its orders
//! on its own input exactly as it did when it owned the keyboard.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::host::Side;

/// Set in the environment of the Savras that tmux starts as the panel, so it
/// knows to drive tmux rather than host the panes itself.
pub const INSIDE: &str = "SAVRAS_TMUX";

/// The tmux server Savras starts: its own socket, so its config never touches
/// a tmux you already run, and yours never touches it.
pub const SOCKET: &str = "savras";

/// How old the facts about the panes may be before they are asked for again.
/// Each asking is a `tmux` and a `ps`, and the loop checks the front pane
/// every tick; a tab exiting is noticed within this, which is soon enough.
const FRESH: Duration = Duration::from_millis(400);

/// How a key forwarded by a binding is wrapped: an APC string, which no
/// keyboard sends and no terminal answers with, so it cannot be mistaken for
/// something typed.
const OPEN: &[u8] = b"\x1b_svr:";
const CLOSE: &[u8] = b"\x1b\\";

/// Bytes a binding sends to the panel for `key`.
pub fn wrap(key: &[u8]) -> Vec<u8> {
    [OPEN, key, CLOSE].concat()
}

/// The key inside a wrapped sequence, if these bytes are one.
///
/// Read from the whole read, not from the start of it: a key typed in the
/// panel and a forwarded one can arrive in the same read, and the panel's own
/// key would then be lost. So the forwarded key is taken out and the rest is
/// returned beside it.
pub fn unwrap(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let start = find(bytes, OPEN)?;
    let body = start + OPEN.len();
    let end = body + find(&bytes[body..], CLOSE)?;
    let key = bytes[body..end].to_vec();
    let rest = [&bytes[..start], &bytes[end + CLOSE.len()..]].concat();
    Some((key, rest))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What tmux says about one pane.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// The exit status, once the program in it has exited. The pane stays,
    /// with its last screen, until Savras or you close it.
    pub dead: Option<u32>,
    /// The title the program set. tmux starts every pane titled with the host
    /// name, which is not something the program said, so that is dropped.
    pub title: String,
    /// The process group in the foreground of the pane's terminal.
    pub foreground: Option<i32>,
    /// The id of the window the pane is in.
    pub window: String,
    pub width: u16,
}

/// The layout as it stands: where the panel is, what is beside it, and how
/// big the window is.
#[derive(Debug, Clone, Default)]
struct Seen {
    panes: HashMap<String, Facts>,
    /// The pane beside the panel, if there is one.
    slot: Option<String>,
    window: (u16, u16),
    zoomed: bool,
    /// The pane with the keyboard, in the panel's window.
    active: Option<String>,
    at: Option<Instant>,
}

/// The layout Savras last asked for, and when.
#[derive(Debug, Clone, PartialEq)]
struct Arranged {
    front: String,
    panel_active: bool,
    at: Instant,
}

/// The pane option naming the machine a pane's program runs on, when that is
/// not this one. Read by [`paste_image`].
pub const HOST: &str = "svr-host";

/// The tmux Savras is running inside.
pub struct Mux {
    /// The panel's own pane, `%N`.
    panel: String,
    /// The session it is in, `$N`: what new windows are opened in, and what
    /// ends when Savras does. A pane id is not a session target.
    session: String,
    seen: Mutex<Seen>,
    /// The layout last arranged, so asking for it again costs nothing.
    arranged: Mutex<Option<Arranged>>,
}

static MUX: OnceLock<Mux> = OnceLock::new();

/// The tmux this Savras drives, if it was started as the panel of one.
///
/// Process-wide, because it is a fact about the process: every pane this
/// Savras opens is opened the same way, and asking every caller to carry the
/// answer would only give them a way to get it wrong.
pub fn get() -> Option<&'static Mux> {
    MUX.get()
}

/// Whether this Savras is the panel of a tmux Savras started. Its tabs have
/// the variable too, emptied, so a Savras run in one of them is not fooled.
pub fn inside() -> bool {
    std::env::var(INSIDE).is_ok_and(|v| v == "1") && std::env::var_os("TMUX_PANE").is_some()
}

/// Take the tmux we were started in, if we were. Called once, as the panel
/// starts.
pub fn adopt() -> Option<&'static Mux> {
    if !inside() {
        return None;
    }
    let panel = std::env::var("TMUX_PANE").ok()?;
    let session = run(&["display-message", "-p", "-t", &panel, "#{session_id}"])
        .ok()?
        .trim()
        .to_string();
    // Every tab keeps its last screen when its program exits; the panel must
    // not, or a panel that stops leaves a session of nothing behind it.
    let _ = run(&["set-option", "-p", "-t", &panel, "remain-on-exit", "off"]);
    let _ = MUX.set(Mux {
        panel,
        session,
        seen: Mutex::new(Seen::default()),
        arranged: Mutex::new(None),
    });
    get()
}

impl Mux {
    /// Run a tmux command against the server we are in, and return what it
    /// printed.
    fn tmux<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        run(args)
    }

    /// Open a pane running `command` in `cwd`, in a window behind the one you
    /// are looking at, and return its id.
    pub fn spawn(&self, command: &[String], cwd: &Path) -> Result<String> {
        let mut args: Vec<String> = vec![
            "new-window".into(),
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
            "-t".into(),
            format!("{}:", self.session),
            "-c".into(),
            cwd.to_string_lossy().into_owned(),
            // So a Savras started in here knows it would be the second one.
            "-e".into(),
            format!("{}={}", crate::host::NESTED, std::process::id()),
            // Only the panel drives tmux; a Savras in a tab must not think it
            // is one.
            "-e".into(),
            format!("{INSIDE}="),
        ];
        args.extend(command.iter().cloned());
        let pane = self.tmux(&args)?.trim().to_string();
        if !pane.starts_with('%') {
            bail!("tmux did not say which pane it opened");
        }
        // The pane's title starts as the host name, which is not something the
        // program said. Cleared, so the panel reads nothing rather than it.
        let _ = self.tmux(&["select-pane", "-t", &pane, "-T", ""]);
        self.forget();
        Ok(pane)
    }

    /// Close a pane, and whatever runs in it.
    pub fn kill(&self, pane: &str) {
        let _ = self.tmux(&["kill-pane", "-t", pane]);
        self.forget();
    }

    /// Hand keys to the program in a pane, byte for byte.
    pub fn send(&self, pane: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "send-keys".to_string(),
            "-t".into(),
            pane.into(),
            "-H".into(),
        ];
        args.extend(bytes.iter().map(|b| format!("{b:02x}")));
        self.tmux(&args).map(|_| ())
    }

    /// Mark a pane with a fact about it, for tmux bindings to read — which
    /// machine it is on, for pasting an image into it.
    pub fn mark(&self, pane: &str, key: &str, value: &str) {
        let _ = self.tmux(&["set-option", "-p", "-t", pane, &format!("@{key}"), value]);
    }

    /// Make the layout this: `front` in the slot beside the panel, and the
    /// keyboard in the panel or in the slot.
    ///
    /// Only what differs is changed, so calling it every time round the loop
    /// costs nothing once the layout is right — and what does change is one
    /// tmux call, since flipping through tabs is done a press at a time and
    /// each press waits for it.
    pub fn arrange(&self, front: &str, panel_active: bool, side: Side, width: u16) -> Result<()> {
        let mut arranged = self.arranged.lock().unwrap();
        if arranged
            .as_ref()
            .is_some_and(|a| a.front == front && a.panel_active == panel_active)
        {
            return Ok(());
        }
        // Where the slot is, from the last arrangement when there was one —
        // nothing else moves panes — and from tmux when something did.
        let slot = match arranged.as_ref() {
            Some(a) => Some(a.front.clone()),
            None => self.seen()?.slot,
        };
        let mut commands: Vec<Vec<String>> = Vec::new();
        if slot.as_deref() != Some(front) {
            match slot {
                // The panes trade places; the one leaving goes to the window
                // the arriving one came from.
                Some(slot) => commands.push(words(&["swap-pane", "-d", "-s", front, "-t", &slot])),
                // Nothing beside the panel — the tab that was there closed —
                // so the pane is brought in beside it.
                None => {
                    let mut join =
                        words(&["join-pane", "-d", "-h", "-s", front, "-t", &self.panel]);
                    if side == Side::Right {
                        join.insert(3, "-b".into());
                    }
                    commands.push(join);
                    commands.push(words(&[
                        "resize-pane",
                        "-t",
                        &self.panel,
                        "-x",
                        &width.to_string(),
                    ]));
                }
            }
        }
        let wanted = if panel_active {
            self.panel.as_str()
        } else {
            front
        };
        commands.push(words(&["select-pane", "-t", wanted]));
        self.tmux(&commands.join(&[";".to_string()][..]))?;
        *arranged = Some(Arranged {
            front: front.to_string(),
            panel_active,
            at: Instant::now(),
        });
        drop(arranged);
        self.forget_seen();
        Ok(())
    }

    /// Whether you moved the keyboard yourself — a click on the other pane —
    /// since Savras last put it somewhere: `Some(true)` for the panel,
    /// `Some(false)` for the pane beside it.
    ///
    /// Read off what tmux says is active, not off focus events. tmux also
    /// moves the keyboard when the pane beside the panel closes, and a focus
    /// event cannot tell that from you; comparing against what Savras asked
    /// for can, because Savras puts it back before asking again.
    pub fn moved(&self) -> Option<bool> {
        let seen = self.seen().ok()?;
        let mut arranged = self.arranged.lock().unwrap();
        let a = arranged.as_mut()?;
        if seen.at.is_none_or(|at| at <= a.at) {
            return None;
        }
        let wanted = if a.panel_active {
            &self.panel
        } else {
            &a.front
        };
        let active = seen.active?;
        if &active == wanted {
            return None;
        }
        let to_panel = active == self.panel;
        if !to_panel && active != a.front {
            return None;
        }
        a.panel_active = to_panel;
        Some(to_panel)
    }

    /// Make the panel this wide.
    pub fn resize(&self, width: u16) -> Result<()> {
        self.tmux(&["resize-pane", "-t", &self.panel, "-x", &width.to_string()])?;
        self.forget();
        Ok(())
    }

    /// Put the panel on the other side of the pane beside it.
    pub fn flip(&self, width: u16) -> Result<()> {
        if let Some(slot) = self.seen()?.slot {
            self.tmux(&["swap-pane", "-d", "-s", &self.panel, "-t", &slot])?;
            self.resize(width)?;
        }
        Ok(())
    }

    /// What tmux says about a pane, as of a moment ago.
    pub fn facts(&self, pane: &str) -> Option<Facts> {
        self.seen().ok()?.panes.get(pane).cloned()
    }

    /// The whole window, and how wide the panel is in it now — which is not
    /// always what Savras last asked for, since a mouse can drag the divider.
    ///
    /// The width only while there is a divider to have dragged: zoomed over a
    /// board, or alone for the moment between one tab closing and the next
    /// taking its place, the panel is as wide as the window, and taking that
    /// for your choice leaves the next tab one column wide.
    pub fn window(&self) -> Option<((u16, u16), Option<u16>)> {
        let seen = self.seen().ok()?;
        let panel = seen.panes.get(&self.panel)?.width;
        let divided = seen.slot.is_some() && !seen.zoomed;
        Some((seen.window, divided.then_some(panel)))
    }

    /// Make a program repaint by telling it its terminal changed size, which
    /// is what a resize would have told it, without moving the divider.
    pub fn jog(&self, pane: &str) {
        if let Some(group) = self.facts(pane).and_then(|facts| facts.foreground) {
            let _ = Command::new("kill")
                .args(["-WINCH", "--", &format!("-{group}")])
                .output();
        }
    }

    /// Whether a terminal showing this tmux has the focus of your desktop.
    pub fn focused(&self) -> bool {
        self.tmux(&["list-clients", "-F", "#{client_flags}"])
            .map(|out| {
                out.lines()
                    .any(|flags| flags.split(',').any(|f| f == "focused"))
            })
            .unwrap_or(false)
    }

    /// End the tmux Savras started, and every pane in it.
    pub fn quit(&self) {
        let _ = self.tmux(&["kill-session", "-t", &self.session]);
    }

    /// Bind the keys that were Savras's, so they reach the panel from
    /// whichever pane has the keyboard.
    pub fn bind(&self, keys: &[(String, Vec<u8>)], paste: Option<&Path>) -> Result<()> {
        for (key, bytes) in keys {
            self.tmux(&bind_forward(key, &self.panel, bytes))?;
        }
        // On a pane whose program has exited there is nothing to type into,
        // and the banner offers enter and q. Everywhere else they are keys.
        for (key, byte) in [("Enter", b'\r'), ("q", b'q')] {
            let forward = sh_join(&send_hex(&self.panel, &wrap(&[byte])));
            self.tmux(&[
                "bind-key",
                "-n",
                key,
                "if-shell",
                "-F",
                "#{pane_dead}",
                &forward,
                &format!("send-keys {key}"),
            ])?;
        }
        if let Some(svr) = paste {
            // ctrl-v is Claude Code's image paste. Here it reads this machine's
            // clipboard itself; in a tab on another machine it cannot, so
            // Savras carries the image over first. See `paste_image`.
            let run = format!(
                "{} paste-image #{{pane_id}}",
                crate::sh::quote(&svr.to_string_lossy())
            );
            self.tmux(&["bind-key", "-n", "C-v", "run-shell", "-b", &run])?;
        }
        Ok(())
    }

    /// Ask again next time, and arrange again next time.
    fn forget(&self) {
        self.forget_seen();
        if let Ok(mut arranged) = self.arranged.lock() {
            *arranged = None;
        }
    }

    fn forget_seen(&self) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.at = None;
        }
    }

    /// The layout, asked for again if it is older than [`FRESH`].
    fn seen(&self) -> Result<Seen> {
        let mut seen = self.seen.lock().unwrap();
        if seen.at.is_some_and(|at| at.elapsed() < FRESH) {
            return Ok(seen.clone());
        }
        let out = self.tmux(&["list-panes", "-s", "-t", &self.panel, "-F", FORMAT])?;
        let mut asked = parse(&out, &self.panel);
        let pids: Vec<String> = asked.pids.values().map(|pid| pid.to_string()).collect();
        if !pids.is_empty() {
            let groups = foreground(&pids);
            for (pane, pid) in &asked.pids {
                if let Some(facts) = asked.seen.panes.get_mut(pane) {
                    facts.foreground = groups.get(pid).copied();
                }
            }
        }
        asked.seen.at = Some(Instant::now());
        *seen = asked.seen;
        Ok(seen.clone())
    }
}

/// One line per pane: everything [`parse`] reads, tab-separated.
const FORMAT: &str = "#{pane_id}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_pid}\t#{window_id}\t#{pane_width}\t#{window_width}\t#{window_height}\t#{window_zoomed_flag}\t#{pane_active}\t#{host}\t#{pane_title}";

struct Asked {
    seen: Seen,
    pids: HashMap<String, i32>,
}

/// Read what `list-panes` said: each pane's facts, which pane shares the
/// panel's window, and the window's size.
fn parse(out: &str, panel: &str) -> Asked {
    let mut seen = Seen::default();
    let mut pids = HashMap::new();
    let mut windows: Vec<(String, String)> = Vec::new();
    let mut actives: Vec<String> = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(12, '\t').collect();
        if f.len() < 12 {
            continue;
        }
        let (id, window) = (f[0].to_string(), f[4].to_string());
        let host = f[10];
        let title = f[11].trim();
        // tmux names a fresh pane after the host; that is not the program.
        let title = if title == host || host.split('.').next() == Some(title) {
            String::new()
        } else {
            title.to_string()
        };
        if id == panel {
            seen.window = (f[6].parse().unwrap_or(0), f[7].parse().unwrap_or(0));
            seen.zoomed = f[8] == "1";
        }
        if let Ok(pid) = f[3].parse() {
            pids.insert(id.clone(), pid);
        }
        seen.panes.insert(
            id.clone(),
            Facts {
                dead: (f[1] == "1").then(|| f[2].parse().unwrap_or(0)),
                title,
                foreground: None,
                window: window.clone(),
                width: f[5].parse().unwrap_or(0),
            },
        );
        windows.push((id.clone(), window));
        if f[9] == "1" {
            actives.push(id);
        }
    }
    if let Some(home) = seen.panes.get(panel).map(|facts| facts.window.clone()) {
        seen.active = actives
            .iter()
            .find(|id| {
                seen.panes
                    .get(*id)
                    .is_some_and(|facts| facts.window == home)
            })
            .cloned();
        seen.slot = windows
            .iter()
            .find(|(id, window)| *window == home && id != panel)
            .map(|(id, _)| id.clone());
    }
    Asked { seen, pids }
}

/// The foreground process group of each of these processes' terminals, asked
/// in one `ps` for all of them.
fn foreground(pids: &[String]) -> HashMap<i32, i32> {
    let Ok(out) = Command::new("ps")
        .args(["-o", "pid=,tpgid=", "-p", &pids.join(",")])
        .output()
    else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let group: i32 = words.next()?.parse().ok()?;
            (group > 0).then_some((pid, group))
        })
        .collect()
}

fn run<S: AsRef<str>>(args: &[S]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args.iter().map(AsRef::as_ref))
        .output()
        .context("running tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args.first().map(AsRef::as_ref).unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn send_hex(pane: &str, bytes: &[u8]) -> Vec<String> {
    let mut args = vec![
        "send-keys".to_string(),
        "-t".into(),
        pane.to_string(),
        "-H".into(),
    ];
    args.extend(bytes.iter().map(|b| format!("{b:02x}")));
    args
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn sh_join(args: &[String]) -> String {
    args.join(" ")
}

/// `bind-key -n <key> send-keys -t <panel> -H …`: the key, wrapped, to the
/// panel.
fn bind_forward(key: &str, panel: &str, bytes: &[u8]) -> Vec<String> {
    let mut args = vec!["bind-key".to_string(), "-n".into(), key.to_string()];
    args.extend(send_hex(panel, &wrap(bytes)));
    args
}

/// tmux's name for a control byte: `C-w` for 0x17.
pub fn ctrl_key(byte: u8) -> String {
    format!("C-{}", (byte | 0x60) as char)
}

/// Config for the tmux Savras starts. It makes tmux look like a plain terminal
/// with a divider — no status bar, no prefix key to learn — and hands the
/// things tmux does better than we did straight to it.
pub fn config() -> String {
    let mut conf = String::from(
        "\
# Written by savras. Applied only to the tmux server savras starts itself,
# on its own socket (`tmux -L savras`), so it cannot touch yours.
set -g prefix None
set -g prefix2 None
unbind-key -a -T prefix
set -g status off
set -g mouse on
set -g escape-time 10
set -g history-limit 50000
set -g focus-events on
set -g extended-keys on
set -g default-terminal tmux-256color
set -g allow-passthrough on
set -g allow-rename off
set -g set-titles off
set -s set-clipboard on
set -as terminal-features ',*:clipboard:hyperlinks:extkeys'
set -g pane-border-style fg=colour238
set -g pane-active-border-style fg=colour238
set -g pane-border-lines single
set -g remain-on-exit on
set -g remain-on-exit-format ' session exited (#{pane_dead_status}) · enter to try again · ctrl-g for the panel · q to close '
set -g window-size latest
",
    );
    // Terminal.app ignores OSC 52, so a selection would reach tmux and stop
    // there. pbcopy puts it on the clipboard you actually paste from.
    if cfg!(target_os = "macos") {
        conf.push_str("set -s copy-command pbcopy\n");
    }
    conf
}

/// Write the config where tmux can read it, and say where.
pub fn write_config() -> Result<std::path::PathBuf> {
    let dir = directories::ProjectDirs::from("", "", "savras")
        .map(|dirs| dirs.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("tmux.conf");
    std::fs::write(&path, config())?;
    Ok(path)
}

/// Whether tmux is here to be used.
pub fn available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The tmux command that starts Savras as the panel of a tmux of its own.
///
/// `args` are what this Savras was started with; the one tmux starts gets the
/// same, and [`INSIDE`] to tell it that it is the panel. Each run is a session
/// of its own on Savras's server, so two terminals each get their own panel.
fn launch_args(args: &[String], conf: &Path) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("finding the svr binary")?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((160, 45));
    let mut line: Vec<String> = vec![
        "-L".into(),
        SOCKET.into(),
        "-f".into(),
        conf.to_string_lossy().into_owned(),
        "new-session".into(),
        "-s".into(),
        format!("svr-{}", std::process::id()),
        "-x".into(),
        cols.to_string(),
        "-y".into(),
        rows.to_string(),
        "-e".into(),
        format!("{INSIDE}=1"),
        exe.to_string_lossy().into_owned(),
    ];
    line.extend(args.iter().cloned());
    Ok(line)
}

/// What [`launch`] would run, for `--dry-run`.
pub fn launch_line(args: &[String]) -> Result<String> {
    let conf = write_config()?;
    let args: Vec<String> = args.iter().filter(|a| *a != "--dry-run").cloned().collect();
    Ok(format!(
        "tmux {}",
        crate::sh::join(&launch_args(&args, &conf)?)
    ))
}

/// Start Savras as the panel of a tmux of its own, and wait for it to end.
pub fn launch(args: &[String]) -> Result<()> {
    let conf = write_config()?;
    let status = Command::new("tmux")
        .args(launch_args(args, &conf)?)
        // Inside another tmux, tmux refuses to start a client unless told the
        // nesting is meant. It is: this server is Savras's own.
        .env_remove("TMUX")
        .status()
        .context("starting tmux")?;
    if !status.success() {
        bail!("tmux exited with {status}");
    }
    Ok(())
}

/// `svr paste-image <pane>`, run by tmux when you press ctrl-v.
///
/// ctrl-v is how Claude Code and Codex paste an image: they read the
/// clipboard themselves. That works for a session on this machine, and cannot
/// for one on another — its clipboard is over there, and the image is here.
/// So for a pane marked with a [`HOST`], an image on the clipboard is copied
/// over the ssh connection Savras already holds to that machine, and its path
/// there is pasted, which both programs take as an image. Anything else —
/// a pane here, a clipboard with no image, a machine that cannot be reached —
/// is ctrl-v, passed on as if nothing had been in the way.
pub fn paste_image(pane: &str) -> Result<()> {
    let host = run(&["show-options", "-p", "-v", "-t", pane, &format!("@{HOST}")])
        .map(|out| out.trim().to_string())
        .unwrap_or_default();
    let pass_on = || run(&["send-keys", "-t", pane, "C-v"]).map(|_| ());
    if host.is_empty() {
        return pass_on();
    }
    let local = std::env::temp_dir().join(format!("svr-paste-{}.png", std::process::id()));
    if !clipboard_image(&local) {
        return pass_on();
    }
    let remote = format!(
        "/tmp/svr-paste-{}.png",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
    );
    let copied = copy_to(&host, &local, &remote);
    let _ = std::fs::remove_file(&local);
    if let Err(e) = copied {
        let _ = run(&[
            "display-message",
            &format!("svr: could not copy the image to {host}: {e:#}"),
        ]);
        return pass_on();
    }
    // Pasted, not typed: a bracketed paste of a path is what dropping a file
    // on the terminal sends, and it is what makes it an image.
    run(&["set-buffer", "-b", "svr-image", "--", &remote])?;
    run(&["paste-buffer", "-p", "-d", "-b", "svr-image", "-t", pane])?;
    Ok(())
}

/// Write the image on this Mac's clipboard to `to` as a PNG. False when there
/// is none, or this is not a Mac.
fn clipboard_image(to: &Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    // A screenshot is PNG or TIFF on the pasteboard; an image file copied in
    // the Finder is its path.
    const SCRIPT: &str = r#"
ObjC.import('AppKit');
function run(argv) {
  var pb = $.NSPasteboard.generalPasteboard;
  var data = pb.dataForType($.NSPasteboardTypePNG);
  if (data.isNil()) {
    var tiff = pb.dataForType($.NSPasteboardTypeTIFF);
    if (!tiff.isNil()) {
      data = $.NSBitmapImageRep.imageRepWithData(tiff)
        .representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
    }
  }
  if (data.isNil()) {
    var url = pb.stringForType($.NSPasteboardTypeFileURL);
    if (!url.isNil()) {
      var path = $.NSURL.URLWithString(url).path;
      var image = $.NSImage.alloc.initWithContentsOfFile(path);
      if (!image.isNil()) {
        data = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation)
          .representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
      }
    }
  }
  if (data.isNil()) return "none";
  data.writeToFileAtomically(argv[0], true);
  return "ok";
}
"#;
    Command::new("osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .arg(to)
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok"
        })
}

/// Copy a file to `host`, over the connection the panel already holds open to
/// it (see `remote::watch`), so it costs no second handshake.
fn copy_to(host: &str, local: &Path, remote: &str) -> Result<()> {
    let file = std::fs::File::open(local)?;
    let out = Command::new("ssh")
        .args([
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPath=~/.ssh/savras-%r@%h:%p",
            "-o",
            "ControlPersist=10m",
            "--",
            host,
            &format!("cat > {}", crate::sh::quote(remote)),
        ])
        .stdin(file)
        .output()
        .context("running ssh")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_key_is_found_wherever_it_sits_in_the_read() {
        assert_eq!(unwrap(&wrap(b"\x17")), Some((b"\x17".to_vec(), Vec::new())));
        let mixed = [b"j".as_slice(), &wrap(b"\x07"), b"k"].concat();
        assert_eq!(unwrap(&mixed), Some((b"\x07".to_vec(), b"jk".to_vec())));
        assert_eq!(unwrap(b"\x1b[A"), None);
        assert_eq!(unwrap(b"\x1b_svr:\x17"), None, "unterminated is not a key");
    }

    #[test]
    fn the_slot_is_the_other_pane_in_the_panels_window() {
        let out = "%0\t0\t\t10\t@0\t44\t160\t40\t0\t0\thost.local\thost.local\n\
                   %1\t0\t\t11\t@0\t115\t160\t40\t0\t1\thost.local\t✳ ROADMAP\n\
                   %2\t1\t3\t12\t@1\t160\t160\t40\t0\t1\thost.local\t\n";
        let asked = parse(out, "%0");
        assert_eq!(asked.seen.slot.as_deref(), Some("%1"));
        assert_eq!(
            asked.seen.active.as_deref(),
            Some("%1"),
            "the active pane of the panel's window, not of a tab's"
        );
        assert_eq!(asked.seen.window, (160, 40));
        assert_eq!(
            asked.seen.panes["%0"].title, "",
            "the host name is not a title"
        );
        assert_eq!(asked.seen.panes["%1"].title, "✳ ROADMAP");
        assert_eq!(asked.seen.panes["%2"].dead, Some(3));
        assert_eq!(asked.seen.panes["%1"].dead, None);
        assert_eq!(asked.pids["%2"], 12);
    }

    #[test]
    fn alone_in_its_window_the_panel_has_nothing_beside_it() {
        let out = "%0\t0\t\t10\t@0\t160\t160\t40\t0\t1\th\t\n\
                   %3\t0\t\t13\t@2\t160\t160\t40\t0\t1\th\t\n";
        assert_eq!(parse(out, "%0").seen.slot, None);
    }

    #[test]
    fn a_binding_hands_the_panel_its_key_wrapped() {
        let args = bind_forward("C-w", "%0", b"\x17");
        assert_eq!(
            args,
            [
                "bind-key",
                "-n",
                "C-w",
                "send-keys",
                "-t",
                "%0",
                "-H",
                "1b",
                "5f",
                "73",
                "76",
                "72",
                "3a",
                "17",
                "1b",
                "5c"
            ]
        );
        assert_eq!(ctrl_key(0x17), "C-w");
        assert_eq!(ctrl_key(0x07), "C-g");
    }

    #[test]
    fn the_config_hands_copying_to_tmux_and_keeps_its_hands_off_keys() {
        let conf = config();
        assert!(conf.contains("set -g prefix None"));
        assert!(conf.contains("set -s set-clipboard on"));
        assert!(conf.contains("set -g mouse on"));
        assert!(conf.contains("remain-on-exit on"));
        assert_eq!(
            conf.contains("copy-command pbcopy"),
            cfg!(target_os = "macos")
        );
    }
}
