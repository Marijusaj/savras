//! The board: what the agents working in one repository say to each other,
//! where the others — and the owner — can read it.
//!
//! Claude Code already has `SendMessage`, and it is point-to-point: you must
//! name the recipient, and the recipient must be running. That covers a lead
//! handing work to its agents (M2.3) and nothing else. The board is the other
//! shape — broadcast, kept, and reachable by anything that can run a command,
//! which is the one capability every CLI agent has. Codex cannot `SendMessage`
//! a Claude Code session; it can run `svr board post`.
//!
//! # A board is something a repository has
//!
//! **Only where the owner made one.** A repository has a board when its file
//! exists, and not otherwise: the owner creates it, cleans it, deletes it and
//! creates it again, and an agent can read and post to one that is there and do
//! nothing else. There is no lane across repositories. The file existing *is*
//! the board being on — there is no list of enabled repositories beside it that
//! could disagree. See `docs/decisions/2026-09-14-board-scope.md`.
//!
//! # The constraint that shapes all of this
//!
//! **Nothing can put words into a session that is already running.** The board
//! can be written at any moment and can only be *read* when an agent chooses to
//! read it. So the board's own job is to be a well-behaved store, and being
//! noticed is the harness's job — a `UserPromptSubmit` hook running
//! `svr board unread --hook` prepends what is new to a turn that was going to
//! happen anyway, and says nothing at all in a repository with no board.
//!
//! # Facts, not conclusions
//!
//! A board holds messages. Everything else is derived: threads come from `re`,
//! "unread" from a per-reader cursor, and the rendered board from both.
//!
//! # Append-only, and why that is the whole storage design
//!
//! A board is opened `O_APPEND` and written one line at a time. Two agents
//! posting at the same instant need no lock, because the kernel makes the offset
//! update atomic — and a file that is only ever appended to cannot be caught
//! mid-rewrite. [`LIMIT`] caps a message well under the page size, and a line
//! that does not parse is skipped rather than killing the read. Cleaning a board
//! truncates it, which an appending writer survives: its next line lands at the
//! new end.
//!
//! Savras still writes nothing to `~/.claude/` and nothing into a repository.
//! The boards are its own files, beside `machines` in its own config directory.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The longest a message may be.
///
/// A storage decision rather than a stylistic one — see the module note on
/// append atomicity. It is also about the reader: the hook puts unread messages
/// into somebody's context on every turn, and an agent that can paste a
/// megabyte into that is an agent that empties everyone's window.
pub const LIMIT: usize = 2000;

/// How many messages a bare `read` shows, and the most a hook will inject.
pub const WINDOW: usize = 30;

/// The name a post from the panel carries.
///
/// The panel is the person at the keyboard rather than an agent, and the
/// agents reading need to tell the two apart: an instruction from the owner is
/// not a suggestion from a peer.
pub const OWNER: &str = "owner";

/// One thing said on a board.
///
/// Serialised one per line. Unknown fields are kept out of the way rather than
/// rejected, so an older `svr` can read a newer board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub at: DateTime<Utc>,
    /// The display name of the session that posted, as resolved at post time.
    pub from: String,
    /// The repository this was said in. Redundant with the file it is in, and
    /// kept because a message read on its own — `--json`, a migrated log — must
    /// still say where it belongs.
    pub topic: String,
    /// The message this answers, when it answers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    pub text: String,
}

impl Message {
    /// The one line an agent reads.
    ///
    /// Machine-shaped rather than pretty — but still plain text, because a
    /// board you cannot `tail` is a board you cannot debug. The id leads so
    /// that replying is a copy rather than a lookup.
    pub fn line(&self) -> String {
        let re = match &self.re {
            Some(id) => format!(" re:{id}"),
            None => String::new(),
        };
        format!(
            "[{}] {} {}{}: {}",
            self.id,
            self.at.format("%H:%M"),
            self.from,
            re,
            self.text
        )
    }
}

// --- who may do what -----------------------------------------------------

/// Proof that the owner, not an agent, is asking.
///
/// Creating, cleaning and deleting a board each take one, and there are only
/// two ways to get one: the panel, which runs at the owner's keyboard, and a
/// command line that carries no agent's marks. So the rule is not a sentence
/// in an agent's instructions, which an agent can ignore; it is the type of an
/// argument, which nothing can call past.
pub struct Owner(());

impl Owner {
    /// The panel is the owner: it is the thing at their keyboard.
    pub fn at_the_panel() -> Self {
        Owner(())
    }

    /// The command line's claim to be the owner, refused under an agent.
    ///
    /// Claude Code marks every shell it starts, and that mark is what is read.
    /// An agent could unset the variables and pass, and that is the honest
    /// limit of asking the environment; the panel has no such gap. Other
    /// clients that mark nothing are not recognised as agents.
    pub fn from_env() -> Result<Self> {
        match agent_marker(|key| std::env::var(key).ok()) {
            Some(marker) => anyhow::bail!(
                "only the owner creates, cleans or deletes a board, and this is \
                 running under an agent ({marker} is set) — use the board in the \
                 panel, or a terminal of your own"
            ),
            None => Ok(Owner(())),
        }
    }
}

/// Where the machine's boards are kept, without opening them.
///
/// [`Boards::open`] also migrates the machine-wide log, which is right for a
/// command or a panel starting up and wrong for anything that merely wants
/// the path — a test building an `App` must never be what moves the owner's
/// real board.
pub fn default_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "savras")
        .context("no config directory for savras on this system")?;
    Ok(dirs.config_dir().join("board"))
}

/// The variable that says this process belongs to an agent, if one does.
fn agent_marker(var: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    const MARKERS: [&str; 3] = ["CLAUDECODE", "CLAUDE_CODE_AGENT", "CLAUDE_JOB_DIR"];
    MARKERS
        .into_iter()
        .find(|key| var(key).is_some_and(|value| !value.trim().is_empty()))
}

// --- the boards ----------------------------------------------------------

/// Every board on this machine, and the only way to any of them.
///
/// Each is a file named for its repository, so which boards exist is a
/// directory listing and nothing has to be kept in step with it.
pub struct Boards {
    dir: PathBuf,
}

impl Boards {
    /// The machine's boards, in Savras's config directory beside `machines`.
    ///
    /// The first open after upgrading from the machine-wide log splits it into
    /// a board per repository it held — see [`Boards::migrate`].
    pub fn open() -> Result<Self> {
        let boards = Boards::at(default_dir()?);
        boards.migrate()?;
        Ok(boards)
    }

    /// The boards kept under `dir` — which is the real directory everywhere but
    /// in a test, and a test that wrote to the real one would be talking to
    /// agents.
    pub fn at(dir: PathBuf) -> Self {
        Boards { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, repo: &str) -> PathBuf {
        self.dir
            .join("repos")
            .join(format!("{}.jsonl", encode(repo)))
    }

    /// Where the readers' places on one board are kept: a small file per
    /// reader, holding the id it has seen up to. A cursor is a fact about a
    /// reader, which is why it is not a flag written back onto the message.
    fn seen(&self, repo: &str) -> PathBuf {
        self.dir.join("seen").join(encode(repo))
    }

    /// Whether this repository has a board.
    pub fn exists(&self, repo: &str) -> bool {
        self.file(repo).is_file()
    }

    /// The repositories that have a board, by path.
    pub fn list(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.dir.join("repos")) else {
            return Vec::new();
        };
        let mut repos: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                decode(name.strip_suffix(".jsonl")?)
            })
            .collect();
        repos.sort();
        repos
    }

    /// Give a repository a board. `false` when it already had one, which is
    /// not an error: the board the owner asked for exists either way.
    pub fn create(&self, _owner: &Owner, repo: &str) -> Result<bool> {
        let path = self.file(repo);
        if path.is_file() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating the board directory {}", parent.display()))?;
        }
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("creating the board at {}", path.display()))?;
        Ok(true)
    }

    /// Remove every message and keep the board, so its agents go on posting to
    /// it. Returns how many were removed.
    pub fn clean(&self, _owner: &Owner, repo: &str) -> Result<usize> {
        let path = self.file(repo);
        anyhow::ensure!(path.is_file(), "{} has no board to clean", topic_name(repo));
        let gone = self.count(repo);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_len(0))
            .with_context(|| format!("emptying the board at {}", path.display()))?;
        // Every place a reader held was a message that is gone.
        let _ = fs::remove_dir_all(self.seen(repo));
        Ok(gone)
    }

    /// Remove the board itself: its agents' posts are refused and their hooks
    /// go quiet until the owner creates it again. `false` when there was none.
    pub fn delete(&self, _owner: &Owner, repo: &str) -> Result<bool> {
        let path = self.file(repo);
        match fs::remove_file(&path) {
            Ok(()) => {
                let _ = fs::remove_dir_all(self.seen(repo));
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("deleting the board at {}", path.display())),
        }
    }

    /// Append a message to a repository's board, and return it.
    ///
    /// Every caller gets the same treatment — the panel, the CLI and the hook
    /// all arrive here, so identity, ordering, truncation and the timestamp are
    /// decided once. The file is opened without `create`: a board is made by its
    /// owner, never by a post, and a board deleted a moment ago stays deleted.
    pub fn post(&self, from: &str, repo: &str, re: Option<String>, text: &str) -> Result<Message> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "a message with no words in it");

        let path = self.file(repo);
        let mut file = match OpenOptions::new().append(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => anyhow::bail!("{}", no_board(repo)),
            Err(e) => {
                return Err(e).with_context(|| format!("opening the board at {}", path.display()))
            }
        };

        let message = Message {
            id: new_id(),
            at: Utc::now(),
            from: from.to_string(),
            topic: repo.to_string(),
            re,
            text: truncate(text, LIMIT),
        };
        let mut line = serde_json::to_string(&message).context("encoding the message")?;
        line.push('\n');
        // One `write_all` of one line, on a handle opened `O_APPEND`: the kernel
        // orders it against every other writer, so no lock is needed.
        file.write_all(line.as_bytes())
            .context("writing to the board")?;
        Ok(message)
    }

    /// Every message on a board, oldest first. No board is no messages.
    ///
    /// A line that does not parse is skipped, not fatal: several processes
    /// write a board, and one bad line must not silence the rest.
    fn all(&self, repo: &str) -> Vec<Message> {
        fs::read_to_string(self.file(repo))
            .map(|text| parse(&text))
            .unwrap_or_default()
    }

    /// The last `limit` messages on a board, oldest first. Moves no cursor:
    /// this is looking, and the panel reads through it for exactly that reason.
    pub fn read(&self, repo: &str, limit: usize) -> Vec<Message> {
        let mut messages = self.all(repo);
        let cut = messages.len().saturating_sub(limit);
        messages.split_off(cut)
    }

    pub fn count(&self, repo: &str) -> usize {
        self.all(repo).len()
    }

    /// What `reader` has not seen on this board, and the cursor that would
    /// mark it seen.
    ///
    /// Split from the writing of the cursor so that "what is new" can be asked
    /// without answering it — the hook wants both, a person debugging wants only
    /// the first, and a reader whose turn is abandoned should not have silently
    /// lost the messages.
    pub fn unread(&self, reader: &str, repo: &str, limit: usize) -> (Vec<Message>, Option<String>) {
        let messages = self.all(repo);
        // An unknown place — a cleaned board, a new reader — starts from the
        // window rather than replaying the whole history at somebody.
        let start = self
            .cursor(reader, repo)
            .and_then(|id| messages.iter().position(|m| m.id == id))
            .map_or(messages.len().saturating_sub(limit), |at| at + 1);
        let tail = &messages[start.min(messages.len())..];
        let mark = tail.last().map(|m| m.id.clone());
        let cut = tail.len().saturating_sub(limit);
        (tail[cut..].to_vec(), mark)
    }

    fn cursor(&self, reader: &str, repo: &str) -> Option<String> {
        let id = fs::read_to_string(self.seen(repo).join(sanitize(reader))).ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    }

    /// Record that `reader` has seen this board up to `id`.
    pub fn mark_seen(&self, reader: &str, repo: &str, id: &str) -> Result<()> {
        let dir = self.seen(repo);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(sanitize(reader));
        fs::write(&path, id).with_context(|| format!("writing the cursor at {}", path.display()))
    }

    /// Split the machine-wide log M5 kept into a board per repository in it.
    ///
    /// The old log is renamed out of the way *first*: the hook runs on every
    /// agent's turn, so two processes can arrive here together, and only the
    /// one whose rename succeeds does the work. It is renamed, not removed, so
    /// nothing said is lost if this goes wrong. Messages to the global lane,
    /// which no longer exists, stay only in that renamed file.
    ///
    /// Each reader's place is carried across — to the last message on each new
    /// board that it had already been shown — so the first turn after the
    /// upgrade does not replay what an agent was already told.
    fn migrate(&self) -> Result<()> {
        let old = self.dir.join("board.jsonl");
        if !old.is_file() {
            return Ok(());
        }
        let claimed = self
            .dir
            .join(format!("board.jsonl.migrating-{}", std::process::id()));
        if fs::rename(&old, &claimed).is_err() {
            return Ok(()); // somebody else got there first
        }

        let messages = parse(&fs::read_to_string(&claimed).unwrap_or_default());
        let places = old_places(&self.dir.join("cursors"));

        let mut by_repo: HashMap<&str, Vec<usize>> = HashMap::new();
        for (at, message) in messages.iter().enumerate() {
            if message.topic.starts_with('/') {
                by_repo.entry(&message.topic).or_default().push(at);
            }
        }

        for (repo, indices) in &by_repo {
            let path = self.file(repo);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new().append(true).create(true).open(&path)?;
            for &at in indices {
                let mut line = serde_json::to_string(&messages[at])?;
                line.push('\n');
                file.write_all(line.as_bytes())?;
            }
            for (reader, id) in &places {
                let Some(seen) = messages.iter().position(|m| &m.id == id) else {
                    continue;
                };
                if let Some(&last) = indices.iter().rev().find(|&&at| at <= seen) {
                    self.mark_seen(reader, repo, &messages[last].id)?;
                }
            }
        }

        fs::rename(&claimed, self.dir.join("board.jsonl.before-per-repo"))?;
        let _ = fs::rename(
            self.dir.join("cursors"),
            self.dir.join("cursors.before-per-repo"),
        );
        Ok(())
    }
}

/// The readers' places in the machine-wide log, as `(reader, id)`.
fn old_places(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let reader = entry.file_name().into_string().ok()?;
            let id = fs::read_to_string(entry.path()).ok()?.trim().to_string();
            (!id.is_empty()).then_some((reader, id))
        })
        .collect()
}

/// What an agent is told when it posts where there is no board.
fn no_board(repo: &str) -> String {
    format!(
        "{} has no board. A board is made by the owner, in the panel or with \
         `svr board create`; until then there is nobody here to read it.",
        topic_name(repo)
    )
}

/// A repository path as a file name, reversibly.
///
/// Every byte that is not a letter, a digit, `.`, `-` or `_` becomes `%XX`, so
/// the name holds no separator and decodes back to exactly the path — which is
/// what lets the directory listing be the list of boards.
fn encode(repo: &str) -> String {
    let mut out = String::with_capacity(repo.len());
    for byte in repo.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// [`encode`], backwards. `None` for a name this did not make.
fn decode(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A reader key is a filename, so it may not be a path or a surprise.
fn sanitize(key: &str) -> String {
    let cleaned: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

// --- who is posting ------------------------------------------------------

/// The name a post is stamped with, and the key its cursor is kept under.
///
/// Resolved by looking, never by being claimed: an agent that supplies its own
/// name in the message body is an agent that can supply somebody else's. The
/// chain runs from most explicit to most general, and always ends somewhere —
/// an unidentifiable poster is still a poster, and refusing it would lose the
/// message over a label.
pub fn whoami(explicit: Option<&str>) -> String {
    if let Some(name) = explicit {
        return name.to_string();
    }
    if let Ok(name) = std::env::var("SAVRAS_BOARD_AS") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    if let Some(name) = name_from_job_dir() {
        return name;
    }
    format!("pid-{}", std::process::id())
}

/// The session name behind `$CLAUDE_JOB_DIR`.
///
/// Claude Code puts a background session's own directory in the environment,
/// and the job's `state.json` beside it carries the name the panel shows. That
/// makes a post from an agent say `SAVRAS-2` rather than a pid, and it costs
/// one small read that only happens when something is actually posted.
fn name_from_job_dir() -> Option<String> {
    let raw = std::env::var("CLAUDE_JOB_DIR").ok()?;
    // The variable points at the job's `tmp`; the state file is its sibling.
    let tmp = Path::new(&raw);
    let candidates = [tmp.join("state.json"), tmp.parent()?.join("state.json")];
    for path in candidates {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(name) = value.get("name").and_then(|n| n.as_str()) {
            if !name.trim().is_empty() {
                return Some(name.trim().to_string());
            }
        }
    }
    None
}

// --- what a message belongs to -------------------------------------------

/// The repository a directory belongs to: the one whose board it posts to.
///
/// The git root rather than the working directory, so a post from `src/` and a
/// post from the root are the same conversation. A directory that is not in a
/// repository is its own — which is right for a scratch directory and costs
/// nothing.
///
/// A worktree resolves to the repository it was cut from, not to itself. Two
/// agents in two worktrees of one project are the likeliest pair on the
/// machine to have something to say to each other — they are the same work on
/// two branches — and two boards would put a wall between exactly those two.
pub fn topic_of(cwd: &Path) -> String {
    let mut at = cwd;
    loop {
        let marker = at.join(".git");
        if marker.is_dir() {
            return at.to_string_lossy().to_string();
        }
        if marker.is_file() {
            // A worktree's marker is a file pointing back at
            // `<main>/.git/worktrees/<name>`, so the repository it belongs to
            // is everything in front of that.
            return main_repo_of(&marker).unwrap_or_else(|| at.to_string_lossy().to_string());
        }
        match at.parent() {
            Some(parent) => at = parent,
            None => return cwd.to_string_lossy().to_string(),
        }
    }
}

/// The repository a worktree's marker file points back to.
///
/// `None` for anything we do not recognise — a submodule, a future format —
/// and the caller falls back to the directory itself.
fn main_repo_of(marker: &Path) -> Option<String> {
    let text = fs::read_to_string(marker).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let (root, _) = target.split_once("/.git/")?;
    (!root.is_empty()).then(|| root.to_string())
}

/// What a repository is called when it is shown.
pub fn topic_name(topic: &str) -> String {
    Path::new(topic)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| topic.to_string())
}

// --- reading and writing -------------------------------------------------

/// Cut a message to `limit`, on a character boundary, saying that it was cut.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}… [cut at {limit}]")
}

/// A unique, roughly sortable id.
///
/// Sortable is a convenience — the file's own order is the board's order — so
/// this needs to be unique and short rather than a real ULID with a dependency
/// behind it.
///
/// The first cut mixed the clock's nanoseconds into it and collided about half
/// the time, because `SystemTime` on macOS does not actually advance that
/// fast: two posts in the same millisecond read the same nanoseconds and got
/// the same id. A counter is exact within a process and the pid separates
/// processes.
fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}{:04x}{:04x}",
        now.as_millis(),
        std::process::id() & 0xffff,
        seq & 0xffff
    )
}

/// Messages from the text of a board, skipping any line that does not parse.
pub fn parse(text: &str) -> Vec<Message> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .collect()
}

/// The messages, as an agent reads them.
///
/// Grouped by repository only when more than one is present — `read
/// --all-topics` — because a heading over a single group is noise in somebody's
/// context window.
pub fn render(messages: &[Message]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut repos: Vec<&str> = Vec::new();
    for m in messages {
        if !repos.contains(&m.topic.as_str()) {
            repos.push(&m.topic);
        }
    }
    if repos.len() < 2 {
        return messages
            .iter()
            .map(Message::line)
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut by_repo: HashMap<&str, Vec<String>> = HashMap::new();
    for m in messages {
        by_repo.entry(&m.topic).or_default().push(m.line());
    }
    repos
        .iter()
        .filter_map(|repo| {
            by_repo
                .get(repo)
                .map(|lines| format!("{}:\n{}", topic_name(repo), lines.join("\n")))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// --- being noticed -------------------------------------------------------

/// What to put in `settings.json`, and the sentence that tells agents a board
/// may exist.
///
/// Printed rather than installed. This edits global configuration for every
/// session on the machine, and a tool that does that without being watched is
/// a tool you stop trusting — the same instinct that keeps Savras out of
/// `~/.claude/` everywhere else.
pub fn install_text() -> String {
    r#"The board needs two things: a hook, so a session in a repository with a
board is told what is new, and a sentence, so every agent knows it can post.

1. In ~/.claude/settings.json, so each turn carries what was said since the
   last one. It prints nothing at all in a repository with no board.

    {
      "hooks": {
        "UserPromptSubmit": [
          {
            "hooks": [
              { "type": "command", "command": "svr board unread --hook" }
            ]
          }
        ]
      }
    }

2. In ~/.claude/CLAUDE.md — and in ~/.codex/AGENTS.md, or whatever the other
   client reads — so posting is a thing agents know they may do:

    ## The board

    A repository may have a board, which the owner creates. Other agents
    working in the same repository can read what you write there.
    `svr board post "<message>"` says something to them, `svr board read`
    shows the recent conversation, and `svr board post --re <id> "<message>"`
    answers one message in particular. Where there is no board, posting says
    so — leave it; making one is the owner's call.

    Post when you learn something another agent would otherwise have to
    rediscover, when you are about to change something shared, or when you are
    asked to. It is a conversation between agents, not a log — nobody is
    reading it out of duty.

Boards themselves are the owner's: `svr board create` in a repository, or `b`
on one of its sessions in the panel and then `c`.
"#
    .to_string()
}

// --- the command ---------------------------------------------------------

/// `svr board …`.
///
/// Returns `Ok(false)` when the first argument is not `board`, so `main` can
/// carry on parsing the panel's own options. The board is a plain command
/// rather than a mode of the TUI because *bash* is the one thing every agent
/// can do.
pub fn dispatch(args: &[String]) -> Result<bool> {
    if args.first().map(String::as_str) != Some("board") {
        return Ok(false);
    }
    run(&args[1..]).map(|()| true)
}

const USAGE: &str = "\
svr board — what the agents in one repository say to each other

    svr board post [options] <message>    say something on this repository's board
    svr board read [options]              the recent conversation
    svr board unread [options]            only what is new to you
    svr board list                        the repositories that have a board

The owner's, refused under an agent:
    svr board create [--repo <path>]      give a repository a board
    svr board clean --yes [--repo <path>] remove every message, keep the board
    svr board delete --yes [--repo <path>] remove the board itself

    svr board install                     how to wire it into every session
    svr board path                        where the boards are kept

Post options:
    --re <id>         answer one message in particular
    --as <name>       post under a name, when we cannot work out yours

Read options:
    --all-topics      every board, not only this repository's
    --limit <n>       how many messages (default 30)
    --json            one message per line, as stored
    --hook            for UserPromptSubmit: a heading, or nothing at all
";

/// What `--all` meets now that there is no lane across repositories.
const NO_LANE: &str = "there is no global lane any more: a board belongs to one \
                       repository, and posts reach the agents working in it";

fn run(args: &[String]) -> Result<()> {
    let (command, rest) = args
        .split_first()
        .map(|(c, r)| (c.as_str(), r))
        .unwrap_or(("read", &[] as &[String]));
    match command {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "post" => post_command(rest),
        "read" => read_command(rest, false),
        "unread" => read_command(rest, true),
        "list" => list_command(),
        "create" | "clean" | "delete" => owner_command(command, rest),
        "install" => {
            print!("{}", install_text());
            Ok(())
        }
        "path" => {
            println!("{}", Boards::open()?.dir().display());
            Ok(())
        }
        other => anyhow::bail!("no such board command: {other}\n\n{USAGE}"),
    }
}

fn here() -> Result<String> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    Ok(topic_of(&cwd))
}

fn post_command(args: &[String]) -> Result<()> {
    let mut re = None;
    let mut as_name = None;
    let mut words: Vec<String> = Vec::new();

    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Refused before anything is opened: the lane is gone, and a post
            // that silently went to this repository instead would reach people
            // the poster did not mean.
            "--all" => anyhow::bail!("{NO_LANE}"),
            "--re" => re = Some(args.next().context("--re needs a message id")?.clone()),
            "--as" => as_name = Some(args.next().context("--as needs a name")?.clone()),
            "--" => {
                words.extend(args.by_ref().cloned());
                break;
            }
            other => words.push(other.to_string()),
        }
    }

    let text = words.join(" ");
    anyhow::ensure!(!text.trim().is_empty(), "nothing to post\n\n{USAGE}");

    let repo = here()?;
    let from = whoami(as_name.as_deref());
    let message = Boards::open()?.post(&from, &repo, re, &text)?;
    println!(
        "posted to {} as {} — id {}",
        topic_name(&message.topic),
        message.from,
        message.id
    );
    Ok(())
}

fn read_command(args: &[String], only_new: bool) -> Result<()> {
    let mut all_topics = false;
    let mut as_json = false;
    let mut hook = false;
    let mut limit = WINDOW;
    let mut as_name = None;

    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all-topics" | "--all" => all_topics = true,
            "--json" => as_json = true,
            "--hook" => hook = true,
            "--as" => as_name = Some(args.next().context("--as needs a name")?.clone()),
            "--limit" => {
                let raw = args.next().context("--limit needs a number")?;
                limit = raw
                    .parse()
                    .with_context(|| format!("--limit must be a number, not {raw}"))?;
            }
            other => anyhow::bail!("no such option: {other}\n\n{USAGE}"),
        }
    }

    let boards = Boards::open()?;
    let repo = here()?;

    let messages = if all_topics && !only_new {
        let mut every: Vec<Message> = boards
            .list()
            .iter()
            .flat_map(|repo| boards.read(repo, limit))
            .collect();
        every.sort_by_key(|m| m.at);
        let cut = every.len().saturating_sub(limit);
        every.split_off(cut)
    } else if !boards.exists(&repo) {
        // A hook writes into somebody's context, so where there is no board it
        // says nothing at all. A person gets a sentence, because silence at a
        // prompt reads as a broken command.
        if !hook {
            println!("{}", no_board(&repo));
        }
        return Ok(());
    } else if only_new {
        let reader = whoami(as_name.as_deref());
        let (new, mark) = boards.unread(&reader, &repo, limit);
        // The cursor moves whether or not anything was shown: what has gone
        // past is past, and a hook that fails to advance replays the same
        // conversation into every turn for the rest of the session.
        if let Some(id) = mark {
            boards.mark_seen(&reader, &repo, &id)?;
        }
        new
    } else {
        boards.read(&repo, limit)
    };

    if as_json {
        for m in &messages {
            println!("{}", serde_json::to_string(m).context("encoding")?);
        }
        return Ok(());
    }

    if messages.is_empty() {
        if !hook {
            println!("nothing on the board for {}", topic_name(&repo));
        }
        return Ok(());
    }

    if hook {
        println!(
            "New on this repository's agent board since your last turn — other \
             agents working alongside you wrote these. Reply with `svr board post \
             --re <id> \"…\"` if one concerns you.\n"
        );
    }
    println!("{}", render(&messages));
    Ok(())
}

fn list_command() -> Result<()> {
    let boards = Boards::open()?;
    let repos = boards.list();
    if repos.is_empty() {
        println!("no boards yet — the owner makes one with `svr board create`");
    }
    for repo in repos {
        println!(
            "{:<20} {:>4} messages  {}",
            topic_name(&repo),
            boards.count(&repo),
            repo
        );
    }
    Ok(())
}

/// `create`, `clean` and `delete`: the owner's, and refused under an agent
/// before anything else is looked at.
fn owner_command(which: &str, args: &[String]) -> Result<()> {
    let mut sure = false;
    let mut repo = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--yes" => sure = true,
            "--repo" => {
                let path = args.next().context("--repo needs a path")?;
                repo = Some(topic_of(Path::new(path)));
            }
            other => anyhow::bail!("no such option: {other}\n\n{USAGE}"),
        }
    }

    let owner = Owner::from_env()?;
    let boards = Boards::open()?;
    let repo = match repo {
        Some(repo) => repo,
        None => here()?,
    };
    let name = topic_name(&repo);

    match which {
        "create" => {
            if boards.create(&owner, &repo)? {
                println!("created a board for {name}");
            } else {
                println!("{name} already has a board");
            }
        }
        // Neither can be taken back, so each says what it is about to do and
        // waits for `--yes` — the command-line form of the panel's question.
        "clean" if !sure => anyhow::bail!(
            "this removes every message on {name}'s board for good ({} of them); \
             run `svr board clean --yes` to do it",
            boards.count(&repo)
        ),
        "clean" => {
            let gone = boards.clean(&owner, &repo)?;
            println!("cleaned {name}'s board: {gone} messages removed");
        }
        "delete" if !sure => anyhow::bail!(
            "this deletes {name}'s board and its {} messages, and its agents can no \
             longer post; run `svr board delete --yes` to do it",
            boards.count(&repo)
        ),
        _ => {
            if boards.delete(&owner, &repo)? {
                println!("deleted {name}'s board");
            } else {
                println!("{name} has no board");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, from: &str, topic: &str, text: &str) -> Message {
        Message {
            id: id.to_string(),
            at: Utc::now(),
            from: from.to_string(),
            topic: topic.to_string(),
            re: None,
            text: text.to_string(),
        }
    }

    /// A board directory of the test's own, removed when it is dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "savras-boards-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn boards(&self) -> Boards {
            Boards::at(self.0.clone())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SAVRAS: &str = "/code/savras";
    const BOOKS: &str = "/code/books";

    #[test]
    fn a_repository_has_a_board_only_once_the_owner_makes_one() {
        let s = Scratch::new("create");
        let boards = s.boards();
        let owner = Owner::at_the_panel();

        let refused = boards.post("AGENT", SAVRAS, None, "hello").unwrap_err();
        assert!(
            refused.to_string().contains("has no board"),
            "a post must not make a board: {refused}"
        );
        assert!(!boards.exists(SAVRAS));
        assert!(boards.list().is_empty());

        assert!(boards.create(&owner, SAVRAS).unwrap());
        assert!(
            !boards.create(&owner, SAVRAS).unwrap(),
            "twice is not an error"
        );
        boards.post("AGENT", SAVRAS, None, "hello").unwrap();
        assert_eq!(boards.list(), [SAVRAS]);
        assert_eq!(boards.count(SAVRAS), 1);
    }

    #[test]
    fn a_board_holds_only_its_own_repository() {
        let s = Scratch::new("own");
        let boards = s.boards();
        let owner = Owner::at_the_panel();
        boards.create(&owner, SAVRAS).unwrap();
        boards.create(&owner, BOOKS).unwrap();
        boards.post("SAVRAS-2", SAVRAS, None, "here").unwrap();
        boards.post("BOOKS-1", BOOKS, None, "there").unwrap();

        let read: Vec<String> = boards
            .read(SAVRAS, 10)
            .into_iter()
            .map(|m| m.text)
            .collect();
        assert_eq!(read, ["here"]);
        assert_eq!(boards.list(), [BOOKS, SAVRAS]);
    }

    #[test]
    fn cleaning_empties_the_board_and_deleting_removes_it() {
        let s = Scratch::new("lifecycle");
        let boards = s.boards();
        let owner = Owner::at_the_panel();
        boards.create(&owner, SAVRAS).unwrap();
        boards.post("A", SAVRAS, None, "one").unwrap();
        let two = boards.post("A", SAVRAS, None, "two").unwrap();
        boards.mark_seen("A", SAVRAS, &two.id).unwrap();

        // Clean keeps the board, so its agents go on posting.
        assert_eq!(boards.clean(&owner, SAVRAS).unwrap(), 2);
        assert!(boards.exists(SAVRAS));
        assert_eq!(boards.count(SAVRAS), 0);
        boards.post("A", SAVRAS, None, "after").unwrap();
        let (new, _) = boards.unread("A", SAVRAS, WINDOW);
        assert_eq!(new.len(), 1, "a place on a cleaned board is forgotten");

        // Delete stops them until the owner makes it again.
        assert!(boards.delete(&owner, SAVRAS).unwrap());
        assert!(!boards.delete(&owner, SAVRAS).unwrap());
        assert!(boards.post("A", SAVRAS, None, "gone?").is_err());
        assert!(boards.clean(&owner, SAVRAS).is_err());

        assert!(boards.create(&owner, SAVRAS).unwrap());
        assert_eq!(boards.count(SAVRAS), 0, "a new board, not the old one");
    }

    #[test]
    fn a_place_on_one_board_says_nothing_about_another() {
        let s = Scratch::new("places");
        let boards = s.boards();
        let owner = Owner::at_the_panel();
        boards.create(&owner, SAVRAS).unwrap();
        boards.create(&owner, BOOKS).unwrap();
        let seen = boards.post("X", SAVRAS, None, "savras").unwrap();
        boards.post("Y", BOOKS, None, "books").unwrap();
        boards.mark_seen("READER", SAVRAS, &seen.id).unwrap();

        assert!(boards.unread("READER", SAVRAS, WINDOW).0.is_empty());
        assert_eq!(boards.unread("READER", BOOKS, WINDOW).0.len(), 1);
    }

    #[test]
    fn a_repository_path_survives_being_a_file_name() {
        for repo in [
            "/Users/me/Code/savras",
            "/Users/me/My Code/it's",
            "/a%2Fb",
            "/ž/日本",
        ] {
            let name = encode(repo);
            assert!(!name.contains('/'), "{name}");
            assert_eq!(decode(&name).as_deref(), Some(repo));
        }
        assert_eq!(decode("%zz"), None);
        assert_eq!(decode("%4"), None);
    }

    #[test]
    fn the_machine_wide_log_is_split_once_and_nobody_is_told_twice() {
        let s = Scratch::new("migrate");
        let log = [
            msg("1", "SAVRAS-8", SAVRAS, "first in savras"),
            msg("2", "RESEARCH", BOOKS, "first in books"),
            msg("3", "SAVRAS-8", "*", "to everyone"),
            msg("4", "ROADMAP", SAVRAS, "second in savras"),
            msg("5", "RESEARCH", BOOKS, "second in books"),
        ]
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(s.0.join("board.jsonl"), log).unwrap();
        fs::create_dir_all(s.0.join("cursors")).unwrap();
        // This reader had been shown up to message 3.
        fs::write(s.0.join("cursors/SAVRAS-8"), "3").unwrap();

        let boards = s.boards();
        boards.migrate().unwrap();

        assert_eq!(boards.list(), [BOOKS, SAVRAS]);
        let savras: Vec<String> = boards.read(SAVRAS, 10).into_iter().map(|m| m.id).collect();
        assert_eq!(savras, ["1", "4"], "the global lane went nowhere");
        // Shown up to 3: message 1 on savras and 2 on books were already seen.
        let ids = |repo| -> Vec<String> {
            boards
                .unread("SAVRAS-8", repo, WINDOW)
                .0
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(ids(SAVRAS), ["4"]);
        assert_eq!(ids(BOOKS), ["5"]);

        assert!(!s.0.join("board.jsonl").exists());
        assert!(s.0.join("board.jsonl.before-per-repo").is_file());
        // Once: a second open finds nothing to split and doubles nothing.
        boards.migrate().unwrap();
        assert_eq!(boards.count(SAVRAS), 2);
    }

    #[test]
    fn there_is_no_global_lane_to_post_to() {
        let refused = run(&["post".into(), "--all".into(), "hello".into()]).unwrap_err();
        assert!(refused.to_string().contains("no global lane"), "{refused}");
    }

    #[test]
    fn an_agent_cannot_pass_for_the_owner() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            agent_marker(env(&[("CLAUDECODE", "1")])),
            Some("CLAUDECODE")
        );
        assert_eq!(
            agent_marker(env(&[("CLAUDE_JOB_DIR", "/x/jobs/abc")])),
            Some("CLAUDE_JOB_DIR")
        );
        assert_eq!(agent_marker(env(&[("HOME", "/Users/me")])), None);
        assert_eq!(agent_marker(env(&[("CLAUDECODE", "")])), None);
    }

    #[test]
    fn a_line_that_does_not_parse_does_not_silence_the_rest() {
        let good = serde_json::to_string(&msg("1", "A", "/r", "first")).unwrap();
        let also = serde_json::to_string(&msg("2", "B", "/r", "second")).unwrap();
        let text = format!("{good}\n{{ half a line\n\n{also}\n");

        let parsed = parse(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].text, "first");
        assert_eq!(parsed[1].text, "second");
    }

    #[test]
    fn an_id_is_not_reused_within_a_millisecond() {
        let ids: std::collections::HashSet<String> = (0..5000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 5000, "ids collided");
    }

    #[test]
    fn a_long_message_is_cut_rather_than_refused() {
        let long = "x".repeat(LIMIT + 500);
        let cut = truncate(&long, LIMIT);
        assert!(cut.starts_with(&"x".repeat(LIMIT)));
        assert!(cut.contains("cut at"));
        assert_eq!(truncate("short", LIMIT), "short");
    }

    #[test]
    fn a_topic_is_the_repository_not_the_directory_you_are_in() {
        let tmp = std::env::temp_dir().join(format!("savras-board-{}", std::process::id()));
        let deep = tmp.join("src").join("inner");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(tmp.join(".git")).unwrap();
        assert_eq!(topic_of(&deep), topic_of(&tmp));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_worktree_posts_under_the_repository_it_was_cut_from() {
        let main = std::env::temp_dir().join(format!("savras-wt-{}", std::process::id()));
        let tree = main.join(".claude").join("worktrees").join("board");
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::write(
            tree.join(".git"),
            format!("gitdir: {}/.git/worktrees/board\n", main.display()),
        )
        .unwrap();
        assert_eq!(topic_of(&tree.join("src")), main.to_string_lossy());
        fs::remove_dir_all(&main).ok();
    }

    #[test]
    fn a_marker_file_we_do_not_understand_falls_back_to_itself() {
        let odd = std::env::temp_dir().join(format!("savras-odd-{}", std::process::id()));
        fs::create_dir_all(&odd).unwrap();
        fs::write(odd.join(".git"), "nothing we recognise\n").unwrap();
        assert_eq!(topic_of(&odd), odd.to_string_lossy());
        fs::remove_dir_all(&odd).ok();
    }

    #[test]
    fn a_directory_outside_a_repository_is_its_own_topic() {
        let tmp = std::env::temp_dir().join(format!("savras-bare-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        assert!(topic_of(&tmp).ends_with(tmp.file_name().unwrap().to_str().unwrap()));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_name_is_resolved_rather_than_claimed() {
        assert_eq!(whoami(Some("AGENT-2")), "AGENT-2");
        assert!(!whoami(None).is_empty());
    }

    #[test]
    fn one_repository_is_shown_without_a_heading_over_it() {
        let messages = vec![msg("1", "A", "/r", "first"), msg("2", "B", "/r", "second")];
        let out = render(&messages);
        assert!(out.contains("A: first"));
        assert!(out.contains("B: second"));
        assert!(!out.contains("r:\n"));
    }

    #[test]
    fn two_repositories_are_told_apart() {
        let messages = vec![
            msg("1", "A", SAVRAS, "local"),
            msg("2", "B", BOOKS, "there"),
        ];
        let out = render(&messages);
        assert!(out.contains("savras:"));
        assert!(out.contains("books:"));
    }

    #[test]
    fn a_reply_carries_what_it_answers() {
        let mut m = msg("2", "B", "/r", "yes");
        m.re = Some("1".to_string());
        assert!(m.line().contains("re:1"));
    }

    #[test]
    fn a_reader_key_cannot_escape_its_directory() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("SAVRAS-2"), "SAVRAS-2");
        assert_eq!(sanitize(""), "unknown");
    }

    #[test]
    fn the_installation_says_both_halves_and_no_lane() {
        let text = install_text();
        assert!(text.contains("UserPromptSubmit"));
        assert!(text.contains("svr board unread --hook"));
        assert!(text.contains("svr board post"));
        assert!(
            !text.contains("--all"),
            "the lane is gone from the instructions too"
        );
    }
}
