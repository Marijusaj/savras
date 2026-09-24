//! Codex sessions, read the way the Claude Code ones are: by looking.
//!
//! Savras's claim is that it shows the agents you have running, and until now
//! that meant Claude Code alone — a Codex session could post to a repository's
//! board while having no row on the panel it was talking through. Codex keeps
//! its own state under `~/.codex`, so the fix is another reader feeding the
//! same [`Job`], not a second panel.
//!
//! # What is read, and what is not
//!
//! Three things on disk, none of them documented by Codex:
//!
//! - `thread-writer-locks/<id>.lock` — one file per session the moment it
//!   starts, held open for as long as it runs. That is the liveness signal,
//!   and it is why **only running sessions are shown**: Codex keeps every
//!   rollout it has ever written, back months, and a panel that listed them
//!   all would bury today's work under February's.
//! - `sessions/<yyyy>/<mm>/<dd>/rollout-…-<id>.jsonl` — the session itself,
//!   appended per event. The first line says where it is working and when it
//!   started; the last few thousand bytes say what it is doing now.
//! - `session_index.jsonl` — the name the owner gave a thread, when they gave
//!   one.
//!
//! The rollout is read from **the end**: these files reach megabytes within an
//! hour, the panel re-reads them every tick, and everything a row needs was
//! said in the last few events. A session that has said nothing in its last
//! 64 KiB is a session mid-answer, and it keeps the name and age it already
//! had rather than blanking.
//!
//! # What it cannot say
//!
//! **Nothing here distinguishes "waiting for your approval" from "waiting for
//! your next prompt".** Codex writes no approval event, so both look the same:
//! no turn in flight. Savras's `Needs input` is a claim that a session asked
//! *you* something, and the ping and the alert mark are built on it — so an
//! idle Codex session is not put there. It is a finished turn, drawn `IDLE`,
//! and the word is the honest one.
//!
//! Being undocumented, every field here is read leniently: a Codex upgrade
//! that renames something costs a column, never the row.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::job::{Client, Job, Status};

/// How much of the end of a rollout is read. Big enough for a turn's worth of
/// events — a tool call and its output are the long ones — and small enough
/// that a megabyte file costs a seek and one read.
const TAIL: u64 = 64 * 1024;

/// How far into the tree `sessions/` is walked: year, month, day, file.
const DEPTH: usize = 4;

/// Where Codex keeps its state.
///
/// `~/.codex`, and not `ProjectDirs`: this is another program's directory,
/// and where that program puts it is not ours to decide.
pub fn default_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".codex"))
}

/// Every Codex session running on this machine, as panel rows.
///
/// No Codex, no directory, no sessions: an empty list, like a `jobs` directory
/// that is not there. This is the common case for anyone who does not use
/// Codex, and it must cost nothing and say nothing.
pub fn load(dir: &Path) -> Vec<Job> {
    let live = live_ids(dir);
    if live.is_empty() {
        return Vec::new();
    }
    let names = names(dir);
    let rollouts = rollouts(&dir.join("sessions"), &live);
    let held = hold(&dir.join("thread-writer-locks"), &live);
    live.iter()
        .map(|id| {
            let named = names.get(id).cloned();
            match rollouts.get(id) {
                Some(path) => read_one(id, path, named.clone()),
                None => None,
            }
            // A session that has not been asked anything yet has a lock and a
            // name and no rollout — Codex writes that on the first turn. It is
            // a session you can type into, so it is a row; it simply has
            // nothing to say yet.
            .unwrap_or_else(|| {
                let cwd = held.get(id).and_then(|holder| holder.cwd.clone());
                waiting_to_start(dir, id, named, cwd)
            })
        })
        .collect()
}

/// The sessions with a writer lock: the ones a Codex process is holding open.
///
/// A lock left behind by a process that died reads as a live session until
/// Codex cleans it up. That is the same trade the rest of the panel makes —
/// a `state.json` outlives its session too — and it fails towards showing a
/// row that can still be resumed.
fn live_ids(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(dir.join("thread-writer-locks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".lock")?;
            // Codex keeps a `.coordination.lock` of its own in here, and
            // whatever else it grows is not ours to draw either. A session id
            // is a uuid, and nothing else in this directory is one.
            is_uuid(id).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids
}

/// `01a0ccda-8ca8-7902-94df-5786dc86d974`, and nothing else.
fn is_uuid(name: &str) -> bool {
    name.len() == 36
        && name.chars().enumerate().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The name the owner gave each thread, latest line winning.
///
/// Append-only, one line per renaming, so the file is read whole and the last
/// answer for an id is the current one.
fn names(dir: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join("session_index.jsonl")) else {
        return names;
    };
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (str_at(&entry, "id"), str_at(&entry, "thread_name")) {
            names.insert(id.to_string(), name.to_string());
        }
    }
    names
}

/// Where each live session's rollout file is.
///
/// Walked rather than read from Codex's sqlite: the tree is one directory per
/// day and the walk stops at the sessions we are looking for, which is a few
/// dozen `stat`s against a database that another process is writing to.
fn rollouts(sessions: &Path, live: &[String]) -> HashMap<String, PathBuf> {
    let mut found = HashMap::new();
    let mut todo = vec![(sessions.to_path_buf(), 0usize)];
    while let Some((at, depth)) = todo.pop() {
        if depth >= DEPTH || found.len() == live.len() {
            continue;
        }
        for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                todo.push((path, depth + 1));
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = live.iter().find(|id| name.contains(id.as_str())) {
                found.insert(id.clone(), path);
            }
        }
    }
    found
}

/// A session that is open but has not had its first turn.
///
/// Its lock is the only file it has, so the lock is where its age comes from,
/// and where it is working is not written down anywhere until it answers
/// something — so it is asked of the process holding that lock, see
/// [`hold`]. When nothing can say, it is drawn with no repository rather
/// than a guessed one, and moves under its heading the moment it is used.
fn waiting_to_start(dir: &Path, id: &str, name: Option<String>, cwd: Option<PathBuf>) -> Job {
    let lock = dir.join("thread-writer-locks").join(format!("{id}.lock"));
    Job {
        short: short(id).to_string(),
        name: flatten(&name.unwrap_or_else(|| format!("codex {}", short(id))), 32),
        color: None,
        status: Status::Done,
        summary: "nothing asked yet".to_string(),
        cwd: cwd.unwrap_or_default(),
        session_id: id.to_string(),
        tokens: 0,
        updated_at: modified(&lock),
        links: Vec::new(),
        backend: None,
        daemon_short: None,
        machine: None,
        created_at: modified(&lock),
        model: None,
        context: None,
        context_window: None,
        failed: false,
        deploy: None,
        client: Client::Codex,
    }
}

/// Which process holds each session's lock, and where it stands — the only
/// thing that says where a session is before its first turn, and the only
/// thing that says which tab it is running in.
///
/// Codex writes the directory into the rollout, and the rollout on the first
/// turn: until then a session started in a tab beside the panel sat under "no
/// directory", away from the repository it was opened in. The lock is held
/// open from the moment the session starts, so whoever holds it is the
/// session, and its working directory is where it was started.
///
/// The holder's pid matters as much as its directory. Codex refuses `codex
/// resume` on a session another process holds — "This conversation is open
/// in another app" — so enter on the row has to know who has it, and
/// [`holders`] is how the panel finds the tab that does.
///
/// One pass for every session that needs it: `pgrep` for the Codex processes,
/// then one `lsof` over just those, which lists each one's locks and its
/// working directory together — tens of milliseconds, on the panel's own
/// thread. Asking `lsof` by file instead walks every process on the machine,
/// a quarter of a second per session. An answer is kept for as long as the
/// session's lock is there, since a holder neither moves nor changes pid; a
/// miss is asked again, but not more than every [`RETRY`].
fn hold(locks: &Path, live: &[String]) -> HashMap<String, Holder> {
    let Ok(mut known) = known().lock() else {
        return HashMap::new();
    };
    // A session that exited is forgotten, so the same id resumed later is
    // asked about again rather than handed its old holder.
    known.held.retain(|id, _| live.contains(id));
    let missing = live.iter().any(|id| !known.held.contains_key(id));
    if missing && known.asked.is_none_or(|at| at.elapsed() >= RETRY) {
        known.asked = Some(Instant::now());
        let found = held_by(&codex_pids(), locks);
        known.held.extend(found);
    }
    known.held.clone()
}

/// The pid holding each running session's lock, by session id, as [`load`]
/// last found it. Costs nothing: it is the answer `load` already paid for.
pub fn holders() -> HashMap<String, i32> {
    known()
        .lock()
        .map(|known| {
            known
                .held
                .iter()
                .map(|(id, holder)| (id.clone(), holder.pid))
                .collect()
        })
        .unwrap_or_default()
}

/// The pid holding this one session's lock, asked now rather than remembered.
///
/// For the moment enter is pressed on its row: a remembered holder can have
/// exited since, and the session been resumed somewhere else, in the two
/// seconds between refreshes. A keypress can afford the `lsof`.
pub fn holder_now(dir: &Path, id: &str) -> Option<i32> {
    let found = held_by(&codex_pids(), &dir.join("thread-writer-locks"));
    let pid = found.get(id).map(|holder| holder.pid);
    if let Ok(mut known) = known().lock() {
        known.held.remove(id);
        known.held.extend(found);
    }
    pid
}

/// Who holds a session's lock: the process, and where it stands when that
/// says anything.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    pid: i32,
    cwd: Option<PathBuf>,
}

struct Known {
    held: HashMap<String, Holder>,
    asked: Option<Instant>,
}

fn known() -> &'static Mutex<Known> {
    static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();
    KNOWN.get_or_init(|| {
        Mutex::new(Known {
            held: HashMap::new(),
            asked: None,
        })
    })
}

/// How long a session nobody could place waits before it is asked about again.
const RETRY: Duration = Duration::from_secs(10);

fn codex_pids() -> Vec<String> {
    run("pgrep", &["-x", "codex"])
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// For each session lock these processes hold, who holds it.
fn held_by(pids: &[String], locks: &Path) -> HashMap<String, Holder> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let pids = pids.join(",");
    // `lsof` names files by their resolved path, and `/var` on macOS — or a
    // symlinked home anywhere — is not one.
    let locks = locks.canonicalize().unwrap_or_else(|_| locks.to_path_buf());
    let locks = locks.as_path();
    parse_held(&run("lsof", &["-w", "-a", "-p", &pids, "-F", "fn"]), locks)
}

/// `lsof -F fn` output — `p<pid>`, then `f<fd>` and `n<name>` per open file —
/// as session id to the process holding it and where that process stands.
fn parse_held(text: &str, lock_dir: &Path) -> HashMap<String, Holder> {
    let mut held = HashMap::new();
    let mut pid: Option<i32> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut locks: Vec<String> = Vec::new();
    let mut fd = "";
    // One process's worth: its locks go to it. A daemon holding a lock
    // stands at `/`, which says nothing about where the session is — but it
    // is still the holder, and still why a resume would be refused.
    let mut flush = |pid: Option<i32>, cwd: &mut Option<PathBuf>, locks: &mut Vec<String>| {
        let at = cwd.take().filter(|at| at.parent().is_some());
        if let Some(pid) = pid {
            for id in locks.drain(..) {
                let cwd = at.clone();
                held.insert(id, Holder { pid, cwd });
            }
        }
        locks.clear();
    };
    for line in text.lines() {
        match line.split_at_checked(1) {
            Some(("p", next)) => {
                flush(pid, &mut cwd, &mut locks);
                pid = next.parse().ok();
            }
            Some(("f", f)) => fd = f,
            Some(("n", name)) if fd == "cwd" => cwd = Some(PathBuf::from(name)),
            Some(("n", name)) => {
                if let Some(id) = session_lock(Path::new(name), lock_dir) {
                    locks.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    flush(pid, &mut cwd, &mut locks);
    held
}

/// The session id a path names, when it is a session's writer lock in the
/// Codex directory being read — not one of another `CODEX_HOME`'s.
fn session_lock<'a>(path: &'a Path, locks: &Path) -> Option<&'a str> {
    let id = path.file_name()?.to_str()?.strip_suffix(".lock")?;
    (path.parent() == Some(locks) && is_uuid(id)).then_some(id)
}

/// A helper program's output, or nothing when it is not there.
fn run(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// One session, from the two ends of its rollout.
fn read_one(id: &str, path: &Path, name: Option<String>) -> Option<Job> {
    let start = head(path)?;
    let now = tail(path);

    // A thread the owner named, else the first thing they asked it, else the
    // id — something is always drawn, because a row with no name is a row
    // that cannot be talked about.
    let name = name
        .or_else(|| start.first_prompt.clone())
        .unwrap_or_else(|| format!("codex {}", short(id)));

    Some(Job {
        short: short(id).to_string(),
        name: flatten(&name, 32),
        color: None,
        status: if now.working {
            Status::Working
        } else {
            Status::Done
        },
        summary: now.said.map(|s| flatten(&s, 400)).unwrap_or_default(),
        cwd: start.cwd,
        session_id: id.to_string(),
        tokens: now.tokens,
        updated_at: modified(path),
        links: Vec::new(),
        backend: None,
        daemon_short: None,
        machine: None,
        created_at: start.at,
        model: now.model,
        context: Some(now.tokens),
        // Codex says its own window, per turn, so the panel does not have to
        // know what `gpt-6-astra` holds — and is not wrong when that changes.
        context_window: now.window,
        failed: false,
        deploy: None,
        client: Client::Codex,
    })
}

/// What the first line of a rollout says: where the session is working, and
/// when it started.
struct Start {
    cwd: PathBuf,
    at: Option<DateTime<Utc>>,
    first_prompt: Option<String>,
}

/// What the end of a rollout says: what the session is doing now.
#[derive(Default)]
struct Now {
    working: bool,
    said: Option<String>,
    tokens: u64,
    window: Option<u64>,
    model: Option<String>,
}

fn head(path: &Path) -> Option<Start> {
    // One line, which carries the whole system prompt and is therefore large.
    // Read as a chunk rather than by line so a rollout whose first line is a
    // megabyte costs a megabyte and not the file.
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 512 * 1024];
    let read = file.read(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer[..read]);
    let line = text.lines().next()?;
    let entry: Value = serde_json::from_str(line).ok()?;
    let payload = entry.get("payload")?;
    Some(Start {
        cwd: PathBuf::from(str_at(payload, "cwd")?),
        at: str_at(payload, "timestamp").and_then(parsed),
        first_prompt: None,
    })
}

fn tail(path: &Path) -> Now {
    let mut now = Now::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return now;
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let from = length.saturating_sub(TAIL);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return now;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return now;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // The first line of a seek into the middle of a file is half a line.
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }

    for line in lines {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        match str_at(payload, "type") {
            // A turn begins and ends; whichever came last is what the session
            // is doing. `task_started` carries the window this model was
            // given, which is the honest denominator for the percentage.
            Some("task_started") => {
                now.working = true;
                now.window = payload
                    .get("model_context_window")
                    .and_then(Value::as_u64)
                    .or(now.window);
            }
            Some("task_complete") => {
                now.working = false;
                now.said = str_at(payload, "last_agent_message").map(str::to_string);
            }
            Some("token_count") => {
                if let Some(total) = payload
                    .get("info")
                    .and_then(|i| i.get("total_token_usage"))
                    .and_then(|u| u.get("total_tokens"))
                    .and_then(Value::as_u64)
                {
                    now.tokens = total;
                }
                if let Some(window) = payload
                    .get("info")
                    .and_then(|i| i.get("model_context_window"))
                    .and_then(Value::as_u64)
                {
                    now.window = Some(window);
                }
            }
            Some("thread_settings_applied") => {
                now.model = payload
                    .get("thread_settings")
                    .and_then(|s| str_at(s, "model"))
                    .map(str::to_string)
                    .or(now.model);
            }
            _ => {}
        }
    }
    now
}

/// The eight characters of the session id the row is known by, which is also
/// what `codex resume` takes.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn parsed(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    let at = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(at))
}

/// One line, cut to `limit`.
///
/// Everything on a row is one line, and an agent's last message is prose with
/// newlines and markdown in it. The same rule the board applies to a message.
fn flatten(text: &str, limit: usize) -> String {
    let one: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= limit {
        return one;
    }
    one.chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `~/.codex` of the test's own.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "savras-codex-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("thread-writer-locks")).unwrap();
            Scratch(dir)
        }

        /// A session: its lock, its rollout under a day, and the events given.
        fn session(self, id: &str, events: &[&str]) -> Self {
            std::fs::write(
                self.0
                    .join("thread-writer-locks")
                    .join(format!("{id}.lock")),
                "",
            )
            .unwrap();
            let day = self.0.join("sessions").join("2026").join("09").join("23");
            std::fs::create_dir_all(&day).unwrap();
            std::fs::write(
                day.join(format!("rollout-2026-09-23T09-01-10-{id}.jsonl")),
                format!("{}\n", events.join("\n")),
            )
            .unwrap();
            self
        }

        fn named(self, id: &str, name: &str) -> Self {
            let line = format!(
                r#"{{"id":"{id}","thread_name":"{name}","updated_at":"2026-09-23T06:08:26Z"}}"#
            );
            let path = self.0.join("session_index.jsonl");
            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
            text.push_str(&line);
            text.push('\n');
            std::fs::write(path, text).unwrap();
            self
        }

        /// The lock is what says a session is running; dropping it is a Codex
        /// session exiting.
        fn exited(self, id: &str) -> Self {
            std::fs::remove_file(
                self.0
                    .join("thread-writer-locks")
                    .join(format!("{id}.lock")),
            )
            .unwrap();
            self
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ID: &str = "01a0ccda-8ca8-7902-94df-5786dc86d974";

    fn meta(cwd: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-09-23T06:04:11.682Z","ordinal":0,"type":"session_meta","payload":{{"session_id":"{ID}","timestamp":"2026-09-23T06:01:10.058Z","cwd":"{cwd}","originator":"codex-tui","cli_version":"0.156.1"}}}}"#
        )
    }

    const SETTINGS: &str = r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":"gpt-6-astra"}}}"#;
    const STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1","model_context_window":258400}}"#;
    const TOKENS: &str = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":129200},"model_context_window":258400}}}"#;
    const DONE: &str = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"Copied all three\ninto ~/.codex/skills"}}"#;

    #[test]
    fn a_running_session_is_a_row_with_what_it_is_doing() {
        let s = Scratch::new("running")
            .session(ID, &[&meta("/tmp/repo"), SETTINGS, STARTED, TOKENS])
            .named(ID, "CODEX SETUP");
        let jobs = load(&s.0);

        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "CODEX SETUP");
        assert_eq!(job.short, "01a0ccda");
        assert_eq!(job.session_id, ID);
        assert_eq!(job.cwd, PathBuf::from("/tmp/repo"));
        assert_eq!(job.status, Status::Working);
        assert_eq!(job.word(), "WORKING");
        assert_eq!(job.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(job.context(), 129_200);
        // Its own window, not the panel's guess: half of 258,400.
        assert_eq!(job.context_percent(), Some(50));
        assert_eq!(
            job.created_at.map(|at| at.to_rfc3339()),
            Some("2026-09-23T06:01:10.058+00:00".to_string())
        );
        assert_eq!(job.open_command(), ["codex", "resume", ID]);
    }

    #[test]
    fn a_finished_turn_is_idle_and_says_the_last_thing_it_said() {
        // Not `DONE`: the session is still running and can be resumed with a
        // word. And not `WAITING` either — see the module note.
        let s = Scratch::new("idle").session(ID, &[&meta("/tmp/repo"), STARTED, TOKENS, DONE]);
        let jobs = load(&s.0);

        assert_eq!(jobs[0].status, Status::Done);
        assert_eq!(jobs[0].word(), "IDLE");
        assert_eq!(
            jobs[0].summary, "Copied all three into ~/.codex/skills",
            "one line, whatever the message did"
        );
    }

    #[test]
    fn a_session_opened_but_not_yet_asked_anything_is_still_a_row() {
        // What the owner saw: a Codex session started in the pane beside the
        // panel, named, waiting at its prompt — and no row, because Codex
        // writes the rollout on the first turn and there was nothing to read.
        let s = Scratch::new("fresh");
        std::fs::write(
            s.0.join("thread-writer-locks").join(format!("{ID}.lock")),
            "",
        )
        .unwrap();
        let s = s.named(ID, "CODEX SETUP 2");

        let jobs = load(&s.0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "CODEX SETUP 2");
        assert_eq!(jobs[0].word(), "IDLE");
        assert_eq!(jobs[0].summary, "nothing asked yet");
        assert!(jobs[0].created_at.is_some(), "aged from its lock");
        // Nothing holds this lock, so nothing says where it is, and it is not
        // put anywhere.
        assert_eq!(jobs[0].repo(), "no directory");
        assert_eq!(jobs[0].open_command(), ["codex", "resume", ID]);
    }

    #[test]
    fn a_session_not_yet_asked_anything_stands_where_its_process_does() {
        // What the owner saw next: a Codex session opened in a tab under
        // savras, drawn under "no directory". Its process holds the lock and
        // stands in savras, and that is where it goes.
        let lsof = "p8048\nfcwd\nn/code/savras\nf3\nn/dev/ttys004\n\
                    f19\nn/h/.codex/thread-writer-locks/01a0cfa4-7b7e-7250-8008-6df63f7f61b0.lock\n\
                    f20\nn/h/.codex/thread-writer-locks/.coordination.lock\n\
                    p99\nfcwd\nn/\n\
                    f7\nn/h/.codex/thread-writer-locks/01a0ccda-8ca8-7902-94df-5786dc86d974.lock\n";
        let held = parse_held(lsof, Path::new("/h/.codex/thread-writer-locks"));
        assert_eq!(
            held.get("01a0cfa4-7b7e-7250-8008-6df63f7f61b0"),
            Some(&Holder {
                pid: 8048,
                cwd: Some(PathBuf::from("/code/savras"))
            })
        );
        assert_eq!(
            held.get(ID),
            Some(&Holder { pid: 99, cwd: None }),
            "a holder at / says nothing about where, but it is still who holds it"
        );
        assert_eq!(held.len(), 2, "a lock that is not a session is no session");
    }

    #[test]
    fn each_session_is_held_by_the_process_whose_locks_it_is_among() {
        // Two Codex processes, one session each: the pid that comes before a
        // lock is its holder, not the one before that. This is what enter on
        // the row joins to a tab with.
        let lsof = "p8048\nfcwd\nn/code/a\n\
                    f19\nn/h/locks/01a0cfa4-7b7e-7250-8008-6df63f7f61b0.lock\n\
                    p8101\nfcwd\nn/code/b\n\
                    f7\nn/h/locks/01a0ccda-8ca8-7902-94df-5786dc86d974.lock\n";
        let held = parse_held(lsof, Path::new("/h/locks"));
        assert_eq!(held["01a0cfa4-7b7e-7250-8008-6df63f7f61b0"].pid, 8048);
        assert_eq!(held[ID].pid, 8101);
        assert_eq!(held[ID].cwd, Some(PathBuf::from("/code/b")));
    }

    #[test]
    fn a_process_holding_a_lock_is_found_by_lsof() {
        // The real `lsof`, with this test as the holder.
        if Command::new("lsof").arg("-v").output().is_err() {
            return;
        }
        let s = Scratch::new("held");
        let lock = s.0.join("thread-writer-locks").join(format!("{ID}.lock"));
        std::fs::write(&lock, "").unwrap();
        let _held = std::fs::File::open(&lock).unwrap();
        let held = held_by(
            &[std::process::id().to_string()],
            &s.0.join("thread-writer-locks"),
        );
        assert_eq!(
            held.get(ID),
            Some(&Holder {
                pid: std::process::id() as i32,
                cwd: Some(std::env::current_dir().unwrap())
            })
        );
    }

    #[test]
    fn a_lock_that_is_not_a_session_is_not_a_row() {
        // Codex keeps a `.coordination.lock` in the same directory. Taken for
        // a session it would be a row named after a file.
        let s = Scratch::new("coordination").session(ID, &[&meta("/tmp/repo"), STARTED]);
        std::fs::write(
            s.0.join("thread-writer-locks").join(".coordination.lock"),
            "",
        )
        .unwrap();
        std::fs::write(s.0.join("thread-writer-locks").join("notes.lock"), "").unwrap();

        let jobs = load(&s.0);
        assert_eq!(jobs.len(), 1, "only the uuid is a session");
        assert_eq!(jobs[0].session_id, ID);
    }

    #[test]
    fn a_session_that_exited_is_not_a_row() {
        // Codex keeps every rollout it has ever written. Only the lock says
        // which of them is a session you could still talk to.
        let s = Scratch::new("exited")
            .session(ID, &[&meta("/tmp/repo"), STARTED])
            .exited(ID);
        assert!(load(&s.0).is_empty());
    }

    #[test]
    fn an_unnamed_thread_still_has_something_to_call_it() {
        let s = Scratch::new("unnamed").session(ID, &[&meta("/tmp/repo"), STARTED]);
        assert_eq!(load(&s.0)[0].name, "codex 01a0ccda");
    }

    #[test]
    fn no_codex_on_this_machine_is_no_rows_and_no_error() {
        assert!(load(Path::new("/nonexistent/savras/codex")).is_empty());
    }

    #[test]
    fn a_rollout_longer_than_the_tail_is_read_from_its_end() {
        // The real files reach megabytes within the hour. What matters is the
        // last turn, and the first line, which is where the session says what
        // it is and where.
        let filler: Vec<String> = (0..4000)
            .map(|n| {
                format!(
                    r#"{{"type":"response_item","payload":{{"type":"reasoning","id":"r{n}","text":"{}"}}}}"#,
                    "x".repeat(80)
                )
            })
            .collect();
        let mut events: Vec<&str> = vec![SETTINGS, STARTED];
        events.extend(filler.iter().map(String::as_str));
        events.push(TOKENS);
        events.push(DONE);
        let meta = meta("/tmp/repo");
        let mut all = vec![meta.as_str()];
        all.extend(events);

        let s = Scratch::new("long").session(ID, &all);
        let jobs = load(&s.0);
        assert_eq!(jobs[0].cwd, PathBuf::from("/tmp/repo"), "the head is read");
        assert_eq!(jobs[0].context(), 129_200, "and so is the end");
        assert_eq!(jobs[0].word(), "IDLE");
    }

    #[test]
    fn a_line_that_does_not_parse_does_not_lose_the_session() {
        // Another program's format, undocumented: a field we cannot read has
        // to cost a column, never the row.
        let s = Scratch::new("garbage").session(
            ID,
            &[
                &meta("/tmp/repo"),
                "{not json at all",
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            ],
        );
        let jobs = load(&s.0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, Status::Working);
        assert_eq!(jobs[0].context_percent(), Some(0), "nothing counted yet");
        assert_eq!(jobs[0].model, None);
    }
}
