//! The board: what one agent says, where every other agent can read it.
//!
//! Claude Code already has `SendMessage`, and it is point-to-point: you must
//! name the recipient, and the recipient must be running. That covers a lead
//! handing work to its agents (M2.3) and nothing else. The board is the other
//! shape — broadcast, kept, and reachable by anything that can run a command,
//! which is the one capability every CLI agent has. Codex cannot `SendMessage`
//! a Claude Code session; it can run `svr board post`.
//!
//! # The constraint that shapes all of this
//!
//! **Nothing can put words into a session that is already running.** Savras
//! learned this in M2.3 and worked around it by briefing an agent through its
//! *opening prompt*. The same wall stands here: the board can be written at
//! any moment and can only be *read* when an agent chooses to read it. So the
//! board's own job is to be a well-behaved store, and being noticed is the
//! harness's job — a `UserPromptSubmit` hook running [`unread`] prepends what
//! is new to a turn that was going to happen anyway. See [`install_text`].
//!
//! # Facts, not conclusions
//!
//! The log holds messages. Everything else is derived: threads come from
//! `re`, "unread" comes from a per-reader cursor, and the rendered board comes
//! from both. Nothing summarised is stored, so a changed definition is a
//! changed function rather than a migration over history.
//!
//! # Append-only, and why that is the whole storage design
//!
//! `board.jsonl` is opened `O_APPEND` and written one line at a time. Two
//! agents posting at the same instant need no lock, because the kernel makes
//! the offset update atomic — and a file that is only ever appended to cannot
//! be caught mid-rewrite, which is exactly the failure M2.1 spent a milestone
//! closing on `state.json`. A torn line is still conceivable for a very large
//! write, so [`LIMIT`] caps a message well under the page size rather than
//! leaving it to luck, and a line that does not parse is skipped rather than
//! killing the read.
//!
//! Savras still writes nothing to `~/.claude/` and still disturbs no running
//! session. The board is its own file, beside `machines` in its own config
//! directory.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The global lane: posted to with `--all`, and read by everyone whatever
/// repository they are in.
///
/// A path can never collide with this, because a topic is always absolute.
pub const EVERYWHERE: &str = "*";

/// The longest a message may be.
///
/// This is a storage decision rather than a stylistic one — see the module
/// note on append atomicity. It is also about the reader: the hook puts
/// unread messages into somebody's context on every turn, and an agent that
/// can paste a megabyte into that is an agent that empties everyone's window.
pub const LIMIT: usize = 2000;

/// How many messages a bare `read` shows, and the most a hook will inject.
pub const WINDOW: usize = 30;

/// One thing said on the board.
///
/// Serialised one per line. Unknown fields are kept out of the way rather than
/// rejected, so an older `svr` can read a newer board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub at: DateTime<Utc>,
    /// The display name of the session that posted, as resolved at post time.
    pub from: String,
    /// The repository this belongs to, or [`EVERYWHERE`].
    pub topic: String,
    /// The message this answers, when it answers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    pub text: String,
}

impl Message {
    /// Whether a reader sitting in `topic` should see this.
    ///
    /// Pure, and the whole of the scoping rule: your own repository, plus the
    /// global lane. Per-repo keeps a `BOOKS` agent out of a `SAVRAS`
    /// conversation it cannot help with; the lane is how something that
    /// genuinely concerns everyone gets said once.
    pub fn concerns(&self, topic: &str) -> bool {
        self.topic == EVERYWHERE || self.topic == topic
    }

    /// The one line an agent reads.
    ///
    /// Machine-shaped rather than pretty — but still plain text, because a
    /// board you cannot `tail` is a board you cannot debug. The id leads so
    /// that replying is a copy rather than a lookup.
    pub fn line(&self) -> String {
        let lane = if self.topic == EVERYWHERE {
            " (all)".to_string()
        } else {
            String::new()
        };
        let re = match &self.re {
            Some(id) => format!(" re:{id}"),
            None => String::new(),
        };
        format!(
            "[{}] {} {}{}{}: {}",
            self.id,
            self.at.format("%H:%M"),
            self.from,
            lane,
            re,
            self.text
        )
    }
}

// --- where it lives ------------------------------------------------------

/// The board's directory, beside `machines`.
pub fn dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "savras")
        .context("no config directory for savras on this system")?;
    Ok(dirs.config_dir().join("board"))
}

/// The log itself.
pub fn log_path() -> Result<PathBuf> {
    Ok(dir()?.join("board.jsonl"))
}

/// Where a reader's cursor is kept.
///
/// One small file per reader holding the id it has seen up to. A cursor is a
/// fact about a reader, which is why it lives here and not as a flag written
/// back onto the message — a read flag on a message has N writers and one
/// truth, and they drift.
fn cursor_path(reader: &str) -> Result<PathBuf> {
    Ok(dir()?.join("cursors").join(sanitize(reader)))
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

/// The topic a directory posts under: the repository it is in.
///
/// The git root rather than the working directory, so a post from `src/` and a
/// post from the root are the same conversation. A directory that is not in a
/// repository is its own topic — which is right for a scratch directory and
/// costs nothing.
///
/// A worktree resolves to the repository it was cut from, not to itself. Two
/// agents in two worktrees of one project are the likeliest pair on the
/// machine to have something to say to each other — they are the same work on
/// two branches — and filing them under separate topics would put a wall
/// between exactly the two that needed none.
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
/// and the caller falls back to the directory itself. A topic we cannot work
/// out must never be a post we lose.
fn main_repo_of(marker: &Path) -> Option<String> {
    let text = fs::read_to_string(marker).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let (root, _) = target.split_once("/.git/")?;
    (!root.is_empty()).then(|| root.to_string())
}

/// What a topic is called when it is shown.
pub fn topic_name(topic: &str) -> String {
    if topic == EVERYWHERE {
        return "all".to_string();
    }
    Path::new(topic)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| topic.to_string())
}

// --- reading and writing -------------------------------------------------

/// Append a message, and return it.
///
/// Every caller gets the same treatment — the panel, the CLI and the hook all
/// arrive here, so identity, ordering, truncation and the timestamp are
/// decided once rather than by whoever wrote the caller.
pub fn post(from: &str, topic: &str, re: Option<String>, text: &str) -> Result<Message> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "a message with no words in it");
    let text = truncate(text, LIMIT);

    let message = Message {
        id: new_id(),
        at: Utc::now(),
        from: from.to_string(),
        topic: topic.to_string(),
        re,
        text,
    };

    let path = log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating the board directory {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(&message).context("encoding the message")?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening the board at {}", path.display()))?;
    // One `write_all` of one line, on a handle opened `O_APPEND`: the kernel
    // orders it against every other writer, so no lock is needed and none is
    // taken. See the module note.
    file.write_all(line.as_bytes())
        .context("writing to the board")?;
    Ok(message)
}

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
/// the same id. Entropy that is not there cannot be borrowed. A counter is
/// exact within a process and the pid separates processes, so a collision now
/// needs two machines' worth of coincidence rather than a fast loop.
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

/// Every message on the board, oldest first.
///
/// A line that does not parse is skipped, not fatal. The board is written by
/// several processes and read by all of them; one bad line must not be able to
/// silence the rest, which is the same call M2.1 made about a half-written
/// `state.json` and for the same reason.
pub fn all() -> Result<Vec<Message>> {
    let path = log_path()?;
    let Ok(text) = fs::read_to_string(&path) else {
        // No board yet is an empty board, not an error. The first `post`
        // creates it.
        return Ok(Vec::new());
    };
    Ok(parse(&text))
}

/// The parsing half of [`all`], separated so it can be tested without a disk.
pub fn parse(text: &str) -> Vec<Message> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .collect()
}

/// The last `limit` messages that concern `topic`, oldest first.
pub fn read(topic: Option<&str>, limit: usize) -> Result<Vec<Message>> {
    let mut messages = all()?;
    if let Some(topic) = topic {
        messages.retain(|m| m.concerns(topic));
    }
    let cut = messages.len().saturating_sub(limit);
    Ok(messages.split_off(cut))
}

/// What `reader` has not seen yet, and the cursor that would mark it seen.
///
/// Split from the writing of the cursor so that "what is new" can be asked
/// without answering it — the hook wants both, a person debugging wants only
/// the first, and a reader whose turn is abandoned should not have silently
/// lost the messages.
pub fn unread(reader: &str, topic: &str, limit: usize) -> Result<(Vec<Message>, Option<String>)> {
    let seen = cursor(reader)?;
    let messages = all()?;

    // Everything after the cursor. An unknown cursor — a board that was
    // trimmed, or a reader from another machine — means start from the window
    // rather than replaying the whole history at somebody.
    let start = match &seen {
        Some(id) => match messages.iter().position(|m| &m.id == id) {
            Some(at) => at + 1,
            None => messages.len().saturating_sub(limit),
        },
        None => messages.len().saturating_sub(limit),
    };

    let tail = &messages[start.min(messages.len())..];
    // The cursor advances past everything, including what this reader is not
    // shown: a message for another repository is not news it is still owed.
    let mark = tail.last().map(|m| m.id.clone());
    let mut mine: Vec<Message> = tail.iter().filter(|m| m.concerns(topic)).cloned().collect();
    let cut = mine.len().saturating_sub(limit);
    Ok((mine.split_off(cut), mark))
}

/// The id `reader` has seen up to.
fn cursor(reader: &str) -> Result<Option<String>> {
    let path = cursor_path(reader)?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            let id = text.trim().to_string();
            Ok(if id.is_empty() { None } else { Some(id) })
        }
        Err(_) => Ok(None),
    }
}

/// Record that `reader` has seen up to `id`.
pub fn mark_seen(reader: &str, id: &str) -> Result<()> {
    let path = cursor_path(reader)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, id).with_context(|| format!("writing the cursor at {}", path.display()))
}

/// The messages, as an agent reads them.
///
/// Grouped by topic only when more than one is present, because a heading over
/// a single group is noise in somebody's context window.
pub fn render(messages: &[Message]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let lanes: Vec<&str> = {
        let mut seen: Vec<&str> = Vec::new();
        for m in messages {
            if !seen.contains(&m.topic.as_str()) {
                seen.push(&m.topic);
            }
        }
        seen
    };
    if lanes.len() < 2 {
        return messages
            .iter()
            .map(Message::line)
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut by_lane: HashMap<&str, Vec<String>> = HashMap::new();
    for m in messages {
        by_lane.entry(&m.topic).or_default().push(m.line());
    }
    lanes
        .iter()
        .filter_map(|lane| {
            by_lane
                .get(lane)
                .map(|lines| format!("{}:\n{}", topic_name(lane), lines.join("\n")))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// --- being noticed -------------------------------------------------------

/// What to put in `settings.json`, and the sentence that tells agents the
/// board exists.
///
/// Printed rather than installed. This edits global configuration for every
/// session on the machine, and a tool that does that without being watched is
/// a tool you stop trusting — the same instinct that keeps Savras out of
/// `~/.claude/` everywhere else.
pub fn install_text() -> String {
    r#"The board needs two things: a hook, so every session is told what is new,
and a sentence, so every agent knows it can post.

1. In ~/.claude/settings.json, so each turn carries what was said since the
   last one. No script file: the hook is the command.

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

    Other agents are working alongside you and can read what you write on the
    board. `svr board post "<message>"` says something to the agents in this
    repository; `svr board post --all "<message>"` says it to every agent on
    the machine. `svr board read` shows the recent conversation, and
    `svr board post --re <id> "<message>"` answers one message in particular.

    Post when you learn something another agent would otherwise have to
    rediscover, when you are about to change something shared, or when you are
    asked to. It is a conversation between agents, not a log — nobody is
    reading it out of duty.
"#
    .to_string()
}

// --- the command ---------------------------------------------------------

/// `svr board …`.
///
/// Returns `Ok(false)` when the first argument is not `board`, so `main` can
/// carry on parsing the panel's own options. The board is a plain command
/// rather than a mode of the TUI because *bash* is the one thing every agent
/// can do — no MCP server to configure, no plugin, nothing that a session
/// started the wrong way is blind to.
pub fn dispatch(args: &[String]) -> Result<bool> {
    if args.first().map(String::as_str) != Some("board") {
        return Ok(false);
    }
    run(&args[1..]).map(|()| true)
}

const USAGE: &str = "\
svr board — what one agent says, where the others can read it

    svr board post [options] <message>    say something
    svr board read [options]              the recent conversation
    svr board unread [options]            only what is new to you
    svr board install                     how to wire it into every session
    svr board path                        where the board is kept

Post options:
    --all             the global lane: every agent, whatever repository
    --re <id>         answer one message in particular
    --as <name>       post under a name, when we cannot work out yours

Read options:
    --all-topics      every repository, not only this one
    --limit <n>       how many messages (default 30)
    --json            one message per line, as stored
    --hook            for UserPromptSubmit: a heading, or nothing at all
";

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
        "install" => {
            print!("{}", install_text());
            Ok(())
        }
        "path" => {
            println!("{}", log_path()?.display());
            Ok(())
        }
        other => anyhow::bail!("no such board command: {other}\n\n{USAGE}"),
    }
}

fn post_command(args: &[String]) -> Result<()> {
    let mut everywhere = false;
    let mut re = None;
    let mut as_name = None;
    let mut words: Vec<String> = Vec::new();

    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => everywhere = true,
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

    let cwd = std::env::current_dir().context("reading the current directory")?;
    let topic = if everywhere {
        EVERYWHERE.to_string()
    } else {
        topic_of(&cwd)
    };
    let from = whoami(as_name.as_deref());
    let message = post(&from, &topic, re, &text)?;
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

    let cwd = std::env::current_dir().context("reading the current directory")?;
    let topic = topic_of(&cwd);
    let reader = whoami(as_name.as_deref());

    let messages = if only_new {
        let (new, mark) = unread(&reader, &topic, limit)?;
        // The cursor moves whether or not anything was shown: what has gone
        // past is past, and a hook that fails to advance replays the same
        // conversation into every turn for the rest of the session.
        if let Some(id) = mark {
            mark_seen(&reader, &id)?;
        }
        new
    } else if all_topics {
        read(None, limit)?
    } else {
        read(Some(&topic), limit)?
    };

    if as_json {
        for m in &messages {
            println!("{}", serde_json::to_string(m).context("encoding")?);
        }
        return Ok(());
    }

    if messages.is_empty() {
        // A hook writes into somebody's context, so it says nothing at all
        // when there is nothing to say. A person gets a sentence, because
        // silence at a prompt reads as a broken command.
        if !hook {
            println!("nothing on the board for {}", topic_name(&topic));
        }
        return Ok(());
    }

    if hook {
        println!(
            "New on the agent board since your last turn — other agents \
             working alongside you wrote these. Reply with `svr board post \
             --re <id> \"…\"` if one concerns you.\n"
        );
    }
    println!("{}", render(&messages));
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

    #[test]
    fn a_message_reaches_its_own_repository_and_the_global_lane() {
        let here = msg("1", "SAVRAS", "/code/savras", "hello");
        let there = msg("2", "BOOKS", "/code/books", "hello");
        let everyone = msg("3", "SAVRAS", EVERYWHERE, "hello");

        assert!(here.concerns("/code/savras"));
        assert!(!here.concerns("/code/books"));
        // The lane is the whole point of per-repo scoping being liveable:
        // something that concerns everyone is said once, not once per repo.
        assert!(everyone.concerns("/code/savras"));
        assert!(everyone.concerns("/code/books"));
        assert!(!there.concerns("/code/savras"));
    }

    #[test]
    fn a_line_that_does_not_parse_does_not_silence_the_rest() {
        // Several processes append to this file. One bad line must cost one
        // message, not the board — the call M2.1 made about `state.json`.
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
        // Two agents posting at once is the normal case, not the rare one, and
        // a duplicate id makes `--re` answer the wrong message.
        let ids: std::collections::HashSet<String> = (0..5000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 5000, "ids collided");
    }

    #[test]
    fn a_long_message_is_cut_rather_than_refused() {
        // The cap is a storage decision, so it must not be a way to lose what
        // somebody said — and the reader has to be told it was cut.
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

        // A post from `src/inner` and a post from the root are the same
        // conversation, or the board splits along a directory nobody chose.
        assert_eq!(topic_of(&deep), topic_of(&tmp));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_worktree_posts_under_the_repository_it_was_cut_from() {
        // Two agents in two worktrees of one project are the likeliest pair on
        // the machine to need each other; separate topics would wall them off.
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
        // A submodule, or a format we have not met. The topic must still be
        // *something*: a post lost over a parse is worse than one filed oddly.
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
        // Walking to `/` and finding nothing must not put every scratch
        // directory on the machine into one shared conversation.
        assert!(topic_of(&tmp).ends_with(tmp.file_name().unwrap().to_str().unwrap()));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_name_is_resolved_rather_than_claimed() {
        // `--as` is the explicit case and wins; with nothing to go on the
        // poster is still identified, because refusing a post over a label
        // would lose the message.
        assert_eq!(whoami(Some("AGENT-2")), "AGENT-2");
        assert!(!whoami(None).is_empty());
    }

    #[test]
    fn one_topic_is_shown_without_a_heading_over_it() {
        let messages = vec![msg("1", "A", "/r", "first"), msg("2", "B", "/r", "second")];
        let out = render(&messages);
        assert!(out.contains("A: first"));
        assert!(out.contains("B: second"));
        // A heading over a single group is noise in somebody's context window.
        assert!(!out.contains("r:\n"));
    }

    #[test]
    fn two_topics_are_told_apart() {
        let messages = vec![
            msg("1", "A", "/code/savras", "local"),
            msg("2", "B", EVERYWHERE, "everyone"),
        ];
        let out = render(&messages);
        assert!(out.contains("savras:"));
        assert!(out.contains("all:"));
    }

    #[test]
    fn a_reply_carries_what_it_answers() {
        let mut m = msg("2", "B", "/r", "yes");
        m.re = Some("1".to_string());
        assert!(m.line().contains("re:1"));
    }

    #[test]
    fn the_global_lane_is_marked_where_it_is_read() {
        // Otherwise a message to every agent on the machine reads exactly like
        // one to this repository, and gets answered as if it were.
        assert!(msg("1", "A", EVERYWHERE, "hi").line().contains("(all)"));
        assert!(!msg("1", "A", "/r", "hi").line().contains("(all)"));
    }

    #[test]
    fn a_reader_key_cannot_escape_its_directory() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("SAVRAS-2"), "SAVRAS-2");
        assert_eq!(sanitize(""), "unknown");
    }

    #[test]
    fn the_installation_says_both_halves() {
        // A hook with no instruction is a board nobody posts to; an
        // instruction with no hook is a board nobody reads.
        let text = install_text();
        assert!(text.contains("UserPromptSubmit"));
        assert!(text.contains("svr board unread --hook"));
        assert!(text.contains("svr board post"));
    }
}
