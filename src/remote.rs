//! Sessions on another machine, read over ssh.
//!
//! The panel's local source is `~/.claude/jobs/`, written by Claude Code's
//! daemon. A machine you only ever ssh into and work in by hand has no daemon
//! and no jobs directory at all — but it does have `~/.claude/sessions/<pid>.json`,
//! one file per live session, and *that* file carries the one thing the jobs
//! directory cannot: `tmux`, the window the session is running in. So the
//! remote source is a different file with a different shape, and this module
//! is the only place that knows it.
//!
//! Two decisions worth keeping.
//!
//! **One long-lived ssh per machine, not a poll.** A round trip to the box is
//! about two thirds of a second, which is slower than the panel redraws; and
//! sshd's `MaxSessions` is ten, so a connection per refresh runs out of them.
//! One connection runs a loop on the far side and streams a batch every couple
//! of seconds, which costs one channel and almost no CPU on a box that is
//! usually already busy.
//!
//! **Liveness is decided over there.** Nothing removes a session's json when
//! its process dies, so a naive read shows ghosts for as long as the machine
//! is up. The loop asks the kernel it belongs to — `kill -0` — and simply does
//! not send what is no longer running.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::job::{Job, Remote, Status};

/// The line the far side prints between batches.
const TICK: &str = "---savras---";

/// How long the far side waits between batches. Slower than the local
/// watcher, which is told by the filesystem rather than asking: this is a
/// question sent over a network to a machine with two cores.
const EVERY: &str = "2";

/// The loop that runs on the other machine.
///
/// Written for `sh` rather than bash, and with no tools beyond coreutils, so
/// it runs on a box nobody has prepared. Each session is flattened onto one
/// line — the files are pretty-printed, and one JSON document per line is what
/// makes the stream parseable without buffering rules.
fn script() -> String {
    format!(
        "while :; do \
           for f in \"$HOME\"/.claude/sessions/*.json; do \
             [ -e \"$f\" ] || continue; \
             pid=${{f##*/}}; pid=${{pid%.json}}; \
             kill -0 \"$pid\" 2>/dev/null || continue; \
             tr -d '\\n' < \"$f\"; echo; \
           done; \
           echo {TICK}; sleep {EVERY}; \
         done"
    )
}

/// Watch every machine, and hand back the batches as they arrive.
///
/// One thread and one ssh per machine. A machine that cannot be reached is
/// reported once and then retried, because the answer to a laptop that has
/// closed its lid is to keep the row rather than to give up on the box.
pub fn watch(hosts: &[String]) -> Receiver<(String, Vec<Job>)> {
    let (tx, rx) = mpsc::channel();
    for host in hosts {
        let host = host.clone();
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            let _ = stream(&host, &tx);
            // The connection died: the box rebooted, the link dropped, the
            // laptop shut. Wait long enough not to hammer it, then try again.
            std::thread::sleep(std::time::Duration::from_secs(10));
        });
    }
    rx
}

/// One ssh, read to its end.
fn stream(host: &str, tx: &mpsc::Sender<(String, Vec<Job>)>) -> std::io::Result<()> {
    let mut child = Command::new("ssh")
        // Never ask: a panel cannot answer a password prompt, and a session
        // that blocks on one looks like a machine that is simply slow.
        .args(["-o", "BatchMode=yes"])
        // Reuse one connection for the watch and for opening a session, so
        // both together cost one of the ten channels sshd allows.
        .args(["-o", "ControlMaster=auto"])
        .args(["-o", "ControlPath=~/.ssh/savras-%r@%h:%p"])
        .args(["-o", "ControlPersist=10m"])
        .args(["-o", "ServerAliveInterval=30"])
        .arg(host)
        .arg(script())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let out = child.stdout.take().expect("stdout was piped");
    let mut batch = Vec::new();
    for line in BufReader::new(out).lines() {
        let line = line?;
        if line.trim() == TICK {
            let jobs = std::mem::take(&mut batch);
            if tx.send((host.to_string(), jobs)).is_err() {
                break; // the panel has gone
            }
            continue;
        }
        if let Some(job) = read_one(host, &line) {
            batch.push(job);
        }
    }
    let _ = child.wait();
    Ok(())
}

/// One session's json, as the panel needs it.
///
/// Lenient in the same way the local reader is: a document we cannot
/// understand is skipped, so a Claude Code upgrade on the far side costs a row
/// rather than the connection.
fn read_one(host: &str, line: &str) -> Option<Job> {
    let raw: RawSession = serde_json::from_str(line.trim()).ok()?;
    let pid = raw.pid?;

    // `busy` is the session thinking; `idle` is it done and waiting for you to
    // type. Idle is therefore this file's version of "needs input" — and it is
    // the transition into it that the ping exists to announce.
    let status = match raw.status.as_deref() {
        Some("idle") => Status::NeedsInput,
        _ => Status::Working,
    };

    let cwd = raw.cwd.map(PathBuf::from).unwrap_or_default();
    Some(Job {
        // Unique across machines, and stable while the session lives: two
        // boxes can both have a pid 33629, and the panel keys everything —
        // tabs, the cursor, what has pinged — off this.
        short: format!("{host}:{pid}"),
        name: raw.name.unwrap_or_else(|| format!("{host}:{pid}")),
        color: None,
        status,
        summary: match status {
            Status::NeedsInput => "waiting at the prompt".to_string(),
            _ => "working".to_string(),
        },
        cwd,
        session_id: raw.session_id.unwrap_or_default(),
        // The file carries no token count. Zero renders as nothing, which is
        // honest: the panel would otherwise say a session had spent none.
        tokens: 0,
        updated_at: millis(raw.status_updated_at.or(raw.updated_at)),
        links: Vec::new(),
        backend: None,
        daemon_short: None,
        machine: Some(Remote {
            host: host.to_string(),
            tmux: raw.tmux,
        }),
    })
}

fn millis(at: Option<i64>) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(at?).single()
}

// --- on-disk shape -------------------------------------------------------
// Mirrors ~/.claude/sessions/<pid>.json as written by Claude Code 2.1.x. It
// shares almost no field names with the jobs directory: `status` here is
// `state` there, and there is no summary, token count or output at all.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    pid: Option<u32>,
    session_id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    status: Option<String>,
    updated_at: Option<i64>,
    status_updated_at: Option<i64>,
    /// tmux's own ids for where it is running: `session:@window.%pane`. Ids,
    /// not indexes, so it survives windows being reordered.
    tmux: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"pid":33629,"sessionId":"19092e9b-844b-4839-adfc-ca6ea52ab51e",
        "cwd":"/home/ubuntu/Code/autodad-assistant","startedAt":1788810611484,
        "version":"2.1.263","kind":"interactive","tmux":"autodad:@2.%2",
        "name":"autodad-assistant-9c","nameSource":"derived","status":"busy",
        "updatedAt":1788812639121,"statusUpdatedAt":1788812639121}"#;

    #[test]
    fn a_remote_session_becomes_a_row() {
        let job = read_one("claude-box", SAMPLE).unwrap();
        assert_eq!(job.name, "autodad-assistant-9c");
        assert_eq!(job.status, Status::Working);
        // Keyed by machine as well as pid: two boxes can hold the same number.
        assert_eq!(job.short, "claude-box:33629");
        assert_eq!(job.machine.as_ref().unwrap().host, "claude-box");
        assert_eq!(
            job.machine.as_ref().unwrap().tmux.as_deref(),
            Some("autodad:@2.%2")
        );
    }

    #[test]
    fn idle_over_there_is_waiting_for_you_over_here() {
        // A session you drive by hand has no "needs input" flag to read: it is
        // simply not thinking any more, which is the same news.
        let idle = SAMPLE.replace(r#""status":"busy""#, r#""status":"idle""#);
        let job = read_one("claude-box", &idle).unwrap();
        assert_eq!(job.status, Status::NeedsInput);
        assert_eq!(job.summary, "waiting at the prompt");
    }

    #[test]
    fn a_session_from_a_machine_is_grouped_under_that_machine() {
        // The user has a checkout of the same repository on both machines, and
        // two rows called `autodad-assistant` under one heading would say the
        // work was in one place when it is in two.
        let job = read_one("claude-box", SAMPLE).unwrap();
        assert_eq!(job.repo(), "claude-box:autodad-assistant");
    }

    #[test]
    fn nonsense_is_skipped_rather_than_fatal() {
        assert!(read_one("claude-box", "half a docum").is_none());
        // A document with no pid names nothing that can be reached.
        assert!(read_one("claude-box", r#"{"status":"idle"}"#).is_none());
    }

    #[test]
    fn the_far_side_only_reports_what_is_still_running() {
        // Nothing cleans these files up when a session exits, so the check has
        // to happen on the machine that owns the pids.
        let script = script();
        assert!(script.contains("kill -0"), "{script}");
        assert!(script.contains(".claude/sessions"), "{script}");
        // One line per document: the files are pretty-printed on disk.
        assert!(script.contains("tr -d"), "{script}");
    }
}
