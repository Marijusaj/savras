//! Codex sessions, asked of Codex.
//!
//! Savras's claim is that it shows the agents you have running, and a Codex
//! session is one — it can post to a repository's board, so it should have a
//! row on the panel it was talking through. The rows are built from the same
//! [`Job`] the Claude Code ones are.
//!
//! # Asking, not inferring
//!
//! Since 0.157 every Codex window — the terminal UI and the desktop app alike
//! — is a client of one local **app-server daemon**, and the daemon answers
//! questions about its threads over a control socket in its own documented
//! protocol (`codex app-server generate-json-schema` prints it). So this
//! module asks it: which threads are loaded, and for each one its name,
//! directory, model, and **status** — `active`, `active` waiting on your
//! approval or your answer, `idle`, `notLoaded`, or `systemError`.
//!
//! It used to work that out instead: a writer lock meant running, the process
//! holding it meant the window, the last `task_started` in the rollout's tail
//! meant working. Every one of those was a guess about Codex's private files,
//! and 0.157 broke all of them at once — the daemon holds the locks, a long
//! turn's start scrolls out of any tail, a thread open in the desktop app has
//! no lock at all, and "waiting for your approval" was never in the files to
//! be found. A question Codex answers itself cannot drift from its answer.
//!
//! # What is still read from disk
//!
//! One thing: the rollout — `Thread::path` — for how much context the session
//! holds and the last thing it said. The protocol streams token usage to the
//! window driving a turn; nothing asks it after the fact. The rollout is read
//! from its end and leniently: a format change costs the percentage, never
//! the row.
//!
//! # Which threads are rows
//!
//! Every thread loaded in the daemon, and every other thread touched in the
//! last [`RECENT`] — so a session whose window you closed, or that a panel
//! restart took down with its tab, is still a row you can resume with enter.
//! One-shot `codex exec` runs and sub-agents are not: they are a command's
//! work, not a session you would go back to.
//!
//! No daemon, no rows: reading must not start one, and with no daemon nothing
//! is running. This is the common case for anyone who does not use Codex, and
//! it costs one failed `connect`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::job::{Client, Job, Status};

/// How far back a thread nobody has open is still a row.
const RECENT: chrono::Duration = chrono::Duration::hours(24);

/// How many of the most recently updated threads are looked through for the
/// recent ones. More than a day of sessions for anyone; the loaded ones are
/// asked for by id regardless.
const PAGE: u32 = 50;

/// How long the daemon has to answer before the panel stops waiting for it.
/// It answers a listing in a few milliseconds; this is for a daemon that is
/// wedged, which must cost the Codex rows and not the panel.
const PATIENCE: Duration = Duration::from_secs(1);

/// How much of the end of a rollout is read. Big enough for a turn's worth of
/// events — a tool call and its output are the long ones — and small enough
/// that a megabyte file costs a seek and one read.
const TAIL: u64 = 64 * 1024;

/// Where Codex keeps its state.
///
/// `~/.codex`, and not `ProjectDirs`: this is another program's directory,
/// and where that program puts it is not ours to decide.
pub fn default_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".codex"))
}

/// Every Codex session worth a row on this machine: the ones running, and the
/// ones touched in the last day. Nothing at all when Codex's daemon is not
/// running, or does not answer.
pub fn load(dir: &Path) -> Vec<Job> {
    let Ok(mut daemon) = Daemon::connect(dir) else {
        return Vec::new();
    };
    threads(&mut daemon, Utc::now())
        .unwrap_or_default()
        .into_iter()
        .map(row)
        .collect()
}

/// Whether a session has anything saved to resume. A thread opened and never
/// asked anything has no rollout, and `codex resume` would only say "No saved
/// session found" and exit.
pub fn saved(dir: &Path, id: &str) -> bool {
    Daemon::connect(dir)
        .and_then(|mut daemon| daemon.read(id))
        .is_ok_and(|thread| thread.path.is_some_and(|path| path.exists()))
}

/// Delete a session, by asking Codex to. Codex stops it if it is running and
/// removes its history; nothing is killed from here.
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    Daemon::connect(dir)
        .context("Codex's daemon is not running")?
        .call("thread/delete", json!({ "threadId": id }))
        .map(|_| ())
}

/// The threads worth a row, as Codex describes them.
fn threads(daemon: &mut Daemon, now: DateTime<Utc>) -> Result<Vec<Thread>> {
    let loaded: Vec<String> =
        serde_json::from_value(daemon.call("thread/loaded/list", json!({}))?["data"].take())
            .unwrap_or_default();
    let listed: Vec<Thread> = serde_json::from_value(
        daemon.call(
            "thread/list",
            json!({ "limit": PAGE, "sortKey": "updated_at" }),
        )?["data"]
            .take(),
    )
    .unwrap_or_default();

    let since = (now - RECENT).timestamp();
    let mut rows: Vec<Thread> = listed
        .into_iter()
        .filter(|t| loaded.contains(&t.id) || t.updated_at >= since)
        .collect();
    // A thread loaded but not yet written to disk is not in the listing.
    for id in &loaded {
        if !rows.iter().any(|t| &t.id == id) {
            if let Ok(thread) = daemon.read(id) {
                rows.push(thread);
            }
        }
    }
    rows.retain(Thread::is_a_session);
    Ok(rows)
}

/// A thread as Codex's protocol describes it — the fields a row needs. Every
/// one but the id is optional here, so a field Codex drops costs a column.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    #[serde(default)]
    name: Option<String>,
    /// Usually the first thing the owner asked it.
    #[serde(default)]
    preview: String,
    #[serde(default)]
    cwd: PathBuf,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    model: Option<String>,
    /// The rollout. Marked unstable in the protocol; used only for tokens.
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    status: Value,
    #[serde(default)]
    source: Value,
    #[serde(default)]
    ephemeral: bool,
    #[serde(default)]
    parent_thread_id: Option<String>,
}

impl Thread {
    /// A session you would come back to: not a sub-agent, not a one-shot
    /// `codex exec`, not a thread Codex never keeps.
    fn is_a_session(&self) -> bool {
        let exec = self.source.as_str() == Some("exec");
        let sub_agent = self.source.get("subAgent").is_some();
        !(self.ephemeral || exec || sub_agent || self.parent_thread_id.is_some())
    }

    fn run(&self) -> Run {
        let flags = |flag: &str| {
            self.status["activeFlags"]
                .as_array()
                .is_some_and(|flags| flags.iter().any(|f| f.as_str() == Some(flag)))
        };
        match self.status["type"].as_str() {
            Some("active") if flags("waitingOnApproval") => Run::Waiting("wants your approval"),
            Some("active") if flags("waitingOnUserInput") => Run::Waiting("asked you something"),
            Some("active") => Run::Working,
            Some("idle") => Run::Idle,
            Some("systemError") => Run::Broken,
            // `notLoaded`, or a status this panel has not heard of: not
            // running, as far as anyone can tell.
            _ => Run::Stopped,
        }
    }
}

/// What a thread is doing, in the terms the panel draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Working,
    /// In a turn, and stopped on you: the words say for what.
    Waiting(&'static str),
    /// Loaded, between turns: a window you can type into.
    Idle,
    /// Not loaded: resumable, and nothing more.
    Stopped,
    Broken,
}

/// One thread, as a row.
fn row(thread: Thread) -> Job {
    let now = thread.path.as_deref().map(tail).unwrap_or_default();
    let run = thread.run();
    let short = short(&thread.id).to_string();
    let name = thread
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| (!thread.preview.trim().is_empty()).then(|| thread.preview.clone()))
        .unwrap_or_else(|| format!("codex {short}"));
    let summary = match run {
        Run::Waiting(why) => why.to_string(),
        _ => now.said.clone().unwrap_or_default(),
    };
    Job {
        short,
        name: flatten(&name, 32),
        color: None,
        status: match run {
            Run::Working => Status::Working,
            Run::Waiting(_) => Status::NeedsInput,
            Run::Idle | Run::Stopped | Run::Broken => Status::Done,
        },
        summary: flatten(&summary, 400),
        cwd: thread.cwd,
        session_id: thread.id,
        tokens: now.tokens,
        updated_at: at(thread.updated_at),
        links: Vec::new(),
        // Loaded in Codex's daemon, which is what makes a finished turn
        // `IDLE` — a window you can type into — rather than `DONE`.
        backend: (run != Run::Stopped).then(|| "daemon".to_string()),
        daemon_short: None,
        machine: None,
        created_at: at(thread.created_at),
        model: thread.model.or(now.model),
        context: Some(now.held),
        // Codex says its own window, per turn, so the panel does not have to
        // know what `gpt-6-astra` holds — and is not wrong when that changes.
        context_window: now.window,
        failed: run == Run::Broken,
        deploy: None,
        client: Client::Codex,
    }
}

fn at(seconds: i64) -> Option<DateTime<Utc>> {
    (seconds > 0)
        .then(|| DateTime::from_timestamp(seconds, 0))
        .flatten()
}

// --- the daemon ------------------------------------------------------------

/// What the daemon is reached over: its control socket, a Unix socket.
#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
/// Codex's daemon is reached over a Unix socket, so elsewhere there is none
/// to reach; the type is only here so the rest compiles, and never connects.
#[cfg(not(unix))]
type Stream = std::net::TcpStream;

/// A connection to Codex's app-server daemon: JSON-RPC over a websocket on its
/// control socket, which is how Codex's own windows talk to it.
struct Daemon {
    socket: tungstenite::WebSocket<Stream>,
    next: u64,
}

/// The control socket, opened.
#[cfg(unix)]
fn open(dir: &Path) -> Result<Stream> {
    let link = dir
        .join("app-server-control")
        .join("app-server-control.sock");
    // Codex links it into a short directory, and a Unix socket path has a
    // hard length limit that the link's own path can exceed.
    let path = std::fs::canonicalize(&link).unwrap_or(link);
    Stream::connect(&path).with_context(|| format!("connecting to {}", path.display()))
}

#[cfg(not(unix))]
fn open(_dir: &Path) -> Result<Stream> {
    anyhow::bail!("Codex's daemon is only reachable on Unix")
}

impl Daemon {
    /// Connect and introduce ourselves. Fails fast when there is no daemon.
    fn connect(dir: &Path) -> Result<Self> {
        let stream = open(dir)?;
        stream.set_read_timeout(Some(PATIENCE))?;
        stream.set_write_timeout(Some(PATIENCE))?;
        let (socket, _) = tungstenite::client("ws://localhost/", stream)
            .map_err(|e| anyhow::anyhow!("websocket handshake: {e}"))?;
        let mut daemon = Daemon { socket, next: 0 };
        daemon.call(
            "initialize",
            json!({ "clientInfo": {
                "name": "savras",
                "title": "Savras",
                "version": env!("CARGO_PKG_VERSION"),
            }}),
        )?;
        daemon.notify("initialized")?;
        Ok(daemon)
    }

    fn read(&mut self, id: &str) -> Result<Thread> {
        let mut answer = self.call("thread/read", json!({ "threadId": id }))?;
        serde_json::from_value(answer["thread"].take()).context("a thread in an unexpected shape")
    }

    /// Ask one thing and wait for its answer, passing over whatever the
    /// daemon announces in between.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let message = self
                .socket
                .read()
                .map_err(|e| anyhow::anyhow!("{method}: {e}"))?;
            let tungstenite::Message::Text(text) = message else {
                continue;
            };
            let Ok(mut answer) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            // Notifications, and requests the daemon makes of its clients,
            // carry a method; an answer to us carries our id and no method.
            if answer.get("method").is_some() || answer["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                anyhow::bail!(
                    "{method}: {}",
                    error["message"]
                        .as_str()
                        .unwrap_or("an error with no words")
                );
            }
            return Ok(answer["result"].take());
        }
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method }))
    }

    fn send(&mut self, message: Value) -> Result<()> {
        self.socket
            .send(tungstenite::Message::text(message.to_string()))
            .map_err(|e| anyhow::anyhow!("sending to Codex: {e}"))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

// --- the rollout ------------------------------------------------------------

/// What the end of a rollout says: how full the context is, and the last
/// thing the session said.
#[derive(Default)]
struct Now {
    said: Option<String>,
    /// Every token the session has spent, turn after turn — what it cost.
    tokens: u64,
    /// What the context holds now: the last request's whole size. Not
    /// `tokens`, which counts the same history again on every turn and read
    /// 39% for a session Codex itself said was at 4%.
    held: u64,
    window: Option<u64>,
    model: Option<String>,
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
            Some("task_started") => {
                now.window = payload
                    .get("model_context_window")
                    .and_then(Value::as_u64)
                    .or(now.window);
            }
            Some("task_complete") => {
                now.said = str_at(payload, "last_agent_message").map(str::to_string);
            }
            Some("token_count") => {
                let usage = |key: &str| {
                    payload
                        .get("info")
                        .and_then(|i| i.get(key))
                        .and_then(|u| u.get("total_tokens"))
                        .and_then(Value::as_u64)
                };
                if let Some(total) = usage("total_token_usage") {
                    now.tokens = total;
                }
                if let Some(last) = usage("last_token_usage") {
                    now.held = last;
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

/// The eight characters of the session id the row is known by.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// One line, for a row: Codex's words come with newlines and markdown in
/// them. The same rule the board applies to a message.
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

#[cfg(all(test, unix))]
pub mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    /// A `~/.codex` with a fake daemon behind its control socket, answering
    /// from a table of threads — the part of the protocol this module speaks.
    pub struct FakeCodex {
        pub dir: PathBuf,
        socket: PathBuf,
        /// What `thread/delete` was asked to delete.
        pub deleted: Arc<Mutex<Vec<String>>>,
    }

    impl FakeCodex {
        /// `loaded` are the ids the daemon has loaded; `threads` are what
        /// `thread/list` and `thread/read` return, as JSON.
        pub fn start(tag: &str, loaded: &[&str], threads: Vec<Value>) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "savras-codex-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("app-server-control")).unwrap();
            // Short, like Codex's own: a socket path has a length limit.
            let socket = PathBuf::from(format!("/tmp/svr-{}-{tag}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            std::os::unix::fs::symlink(
                &socket,
                dir.join("app-server-control")
                    .join("app-server-control.sock"),
            )
            .unwrap();

            let loaded: Vec<String> = loaded.iter().map(|s| s.to_string()).collect();
            let deleted = Arc::new(Mutex::new(Vec::new()));
            let told = deleted.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let Ok(mut ws) = tungstenite::accept(stream) else {
                        continue;
                    };
                    // Something to pass over, as the real one does.
                    let _ = ws.send(tungstenite::Message::text(
                        r#"{"method":"account/updated","params":{}}"#,
                    ));
                    while let Ok(message) = ws.read() {
                        let tungstenite::Message::Text(text) = message else {
                            continue;
                        };
                        let asked: Value = serde_json::from_str(text.as_str()).unwrap();
                        let Some(id) = asked.get("id").cloned() else {
                            continue;
                        };
                        let result = match asked["method"].as_str().unwrap() {
                            "initialize" => json!({ "userAgent": "fake" }),
                            "thread/loaded/list" => json!({ "data": loaded }),
                            "thread/list" => json!({ "data": threads }),
                            "thread/read" => {
                                let want = asked["params"]["threadId"].clone();
                                match threads.iter().find(|t| t["id"] == want) {
                                    Some(thread) => json!({ "thread": thread }),
                                    None => {
                                        let _ = ws.send(tungstenite::Message::text(
                                            json!({ "id": id, "error": { "message": "no such thread" } })
                                                .to_string(),
                                        ));
                                        continue;
                                    }
                                }
                            }
                            "thread/delete" => {
                                told.lock()
                                    .unwrap()
                                    .push(asked["params"]["threadId"].as_str().unwrap().into());
                                json!({})
                            }
                            other => panic!("the fake does not speak {other}"),
                        };
                        let _ = ws.send(tungstenite::Message::text(
                            json!({ "id": id, "result": result }).to_string(),
                        ));
                    }
                }
            });
            FakeCodex {
                dir,
                socket,
                deleted,
            }
        }
    }

    impl Drop for FakeCodex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    pub const ID: &str = "01a0ccda-8ca8-7902-94df-5786dc86d974";
    const OTHER: &str = "01a0d957-e55e-7971-bbf8-adea1dcba9b7";

    /// A thread as the protocol describes one, updated `ago` seconds ago.
    pub fn thread(id: &str, name: &str, status: Value, ago: i64) -> Value {
        let now = Utc::now().timestamp();
        json!({
            "id": id,
            "name": name,
            "preview": "set up the mail client",
            "cwd": "/tmp/repo",
            "createdAt": now - ago - 60,
            "updatedAt": now - ago,
            "model": "gpt-6-astra",
            "path": null,
            "status": status,
            "source": "cli",
            "ephemeral": false,
            "parentThreadId": null,
        })
    }

    fn active(flags: &[&str]) -> Value {
        json!({ "type": "active", "activeFlags": flags })
    }

    fn only(codex: &FakeCodex) -> Job {
        let jobs = load(&codex.dir);
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        jobs.into_iter().next().unwrap()
    }

    #[test]
    fn a_running_turn_is_working_whatever_its_rollout_says() {
        let codex = FakeCodex::start("working", &[ID], vec![thread(ID, "EMAIL", active(&[]), 5)]);
        let job = only(&codex);
        assert_eq!(job.name, "EMAIL");
        assert_eq!(job.short, "01a0ccda");
        assert_eq!(job.cwd, PathBuf::from("/tmp/repo"));
        assert_eq!(job.word(), "WORKING");
        assert_eq!(job.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(job.open_command(), ["codex", "resume", ID]);
    }

    #[test]
    fn a_turn_stopped_on_you_is_waiting_and_says_for_what() {
        // What the rollouts never held: Codex says so itself.
        let codex = FakeCodex::start(
            "waiting",
            &[ID],
            vec![thread(ID, "EMAIL", active(&["waitingOnApproval"]), 5)],
        );
        let job = only(&codex);
        assert_eq!(job.word(), "WAITING");
        assert_eq!(job.status, Status::NeedsInput);
        assert_eq!(job.summary, "wants your approval");
    }

    #[test]
    fn loaded_between_turns_is_idle_and_closed_is_done() {
        let codex = FakeCodex::start(
            "idle-done",
            &[ID],
            vec![
                thread(ID, "CODE REVIEW", json!({ "type": "idle" }), 60),
                thread(OTHER, "EMAIL", json!({ "type": "notLoaded" }), 600),
            ],
        );
        let jobs = load(&codex.dir);
        let word = |name: &str| jobs.iter().find(|j| j.name == name).map(Job::word);
        assert_eq!(word("CODE REVIEW"), Some("IDLE"));
        // Closed an hour ago — a panel restart took its tab — and still a
        // row to resume.
        assert_eq!(word("EMAIL"), Some("DONE"));
    }

    #[test]
    fn a_closed_thread_older_than_a_day_is_not_a_row_and_a_loaded_one_always_is() {
        let day = 24 * 3600;
        let codex = FakeCodex::start(
            "old",
            &[ID],
            vec![
                thread(ID, "LONG RUNNING", json!({ "type": "idle" }), 3 * day),
                thread(OTHER, "LAST WEEK", json!({ "type": "notLoaded" }), 7 * day),
            ],
        );
        assert_eq!(only(&codex).name, "LONG RUNNING");
    }

    #[test]
    fn exec_runs_and_sub_agents_are_not_sessions() {
        let mut exec = thread(ID, "one-shot", json!({ "type": "notLoaded" }), 60);
        exec["source"] = json!("exec");
        let mut sub = thread(OTHER, "helper", active(&[]), 60);
        sub["source"] = json!({ "subAgent": { "thread_spawn": {} } });
        let codex = FakeCodex::start("sources", &[OTHER], vec![exec, sub]);
        assert!(load(&codex.dir).is_empty());
    }

    #[test]
    fn a_broken_thread_is_failed() {
        let codex = FakeCodex::start(
            "broken",
            &[ID],
            vec![thread(ID, "EMAIL", json!({ "type": "systemError" }), 5)],
        );
        assert_eq!(only(&codex).word(), "FAILED");
    }

    #[test]
    fn no_daemon_is_no_rows_and_no_error() {
        assert!(load(Path::new("/nonexistent/savras/codex")).is_empty());
        assert!(!saved(Path::new("/nonexistent/savras/codex"), ID));
        assert!(delete(Path::new("/nonexistent/savras/codex"), ID).is_err());
    }

    #[test]
    fn the_context_is_what_the_rollout_says_the_last_request_held() {
        // From a real session: Codex said "Context 4% used" when its last
        // request was 20,744 tokens, and the row said 39% — the whole
        // session's spend, 100,938, over the window.
        let codex = FakeCodex::start("tokens", &[ID], Vec::new());
        let rollout = codex.dir.join("rollout.jsonl");
        std::fs::write(
            &rollout,
            [
                r#"{"type":"session_meta","payload":{"cwd":"/tmp/repo"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started","model_context_window":258400}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100938},"last_token_usage":{"total_tokens":20744},"model_context_window":258400}}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Installed\nThunderbird"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let mut t = thread(ID, "EMAIL", json!({ "type": "idle" }), 5);
        t["path"] = json!(rollout);
        let job = row(serde_json::from_value(t).unwrap());
        assert_eq!(job.context(), 20_744);
        assert_eq!(job.tokens, 100_938);
        assert_eq!(job.context_percent(), Some(4));
        assert_eq!(job.summary, "Installed Thunderbird");
    }

    #[test]
    fn saved_is_whether_codex_wrote_it_down_and_delete_asks_codex() {
        let codex = FakeCodex::start("saved", &[ID, OTHER], Vec::new());
        assert!(!saved(&codex.dir, ID), "no such thread");

        let rollout = codex.dir.join("rollout.jsonl");
        std::fs::write(&rollout, "").unwrap();
        let mut asked = thread(ID, "EMAIL", json!({ "type": "idle" }), 5);
        asked["path"] = json!(rollout);
        let never = thread(OTHER, "NEW", json!({ "type": "idle" }), 5);
        let codex = FakeCodex::start("saved2", &[ID, OTHER], vec![asked, never]);
        assert!(saved(&codex.dir, ID));
        assert!(!saved(&codex.dir, OTHER), "opened, never asked: no rollout");

        delete(&codex.dir, ID).unwrap();
        assert_eq!(*codex.deleted.lock().unwrap(), [ID]);
    }

    #[test]
    fn an_unnamed_thread_is_called_by_what_it_was_asked_or_its_id() {
        let mut t = thread(ID, "", json!({ "type": "idle" }), 5);
        assert_eq!(
            row(serde_json::from_value(t.clone()).unwrap()).name,
            "set up the mail client"
        );
        t["preview"] = json!("");
        assert_eq!(
            row(serde_json::from_value(t).unwrap()).name,
            "codex 01a0ccda"
        );
    }
}
