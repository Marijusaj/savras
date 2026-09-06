//! Reads Claude Code's job state from `~/.claude/jobs/<short>/state.json`.
//!
//! This is the only module that knows Claude Code's on-disk format. Everything
//! above it sees `Job` and `Snapshot`. Parsing is deliberately lenient: every
//! field is optional, and a job we cannot understand is skipped rather than
//! fatal, so a Claude Code upgrade degrades the panel instead of breaking it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

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
}

impl Job {
    /// How to open this session in a terminal.
    ///
    /// A session running in Claude Code's daemon must be attached: asking to
    /// resume it is refused with "is running as a background session ... run
    /// `claude attach` to open it". Attaching is also the gentler of the two —
    /// the session keeps running whether you attach to it or not.
    pub fn open_command(&self) -> Vec<String> {
        match (&self.backend, &self.daemon_short) {
            (Some(backend), Some(short)) if backend == "daemon" => {
                vec!["claude".into(), "attach".into(), short.clone()]
            }
            _ => vec!["claude".into(), "--resume".into(), self.session_id.clone()],
        }
    }

    /// The same command, as one line for the panel to show.
    pub fn open_command_line(&self) -> String {
        self.open_command().join(" ")
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

    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue; // pins.json and friends
        }
        let short = entry.file_name().to_string_lossy().to_string();
        if let Some(job) = read_one(&entry.path(), &short) {
            jobs.push(job);
        }
    }

    // Group first, then freshest within each group: the rows most likely to
    // need you are the ones nearest the top.
    jobs.sort_by(|a, b| {
        a.status
            .cmp(&b.status)
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.name.cmp(&b.name))
    });

    Ok(Snapshot { jobs })
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

fn read_one(dir: &Path, short: &str) -> Option<Job> {
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
        backend: raw.backend,
        daemon_short: raw.daemon_short,
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
    fn groups_by_needs_then_state() {
        let f = Fixture::new("groups")
            .job(
                "aaa",
                r#"{"state":"working","name":"WORK","detail":"building"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"ASK","detail":"d","needs":"answer: which one?"}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"shipped"}}"#,
            );
        let snap = load(&f.0).unwrap();

        assert_eq!(snap.jobs.len(), 3);
        // Needs-input first, then working, then done.
        assert_eq!(snap.jobs[0].name, "ASK");
        assert_eq!(snap.jobs[0].status, Status::NeedsInput);
        assert_eq!(snap.jobs[1].status, Status::Working);
        assert_eq!(snap.jobs[2].status, Status::Done);
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
