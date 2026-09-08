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
    /// The machine it is running on, when that is not this one.
    pub machine: Option<Remote>,
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
        ssh.push(self.host.clone());
        ssh.push(match self.tmux.as_deref().and_then(split_target) {
            // `\;` and not `;`: the remote shell has to hand tmux a literal
            // semicolon as an argument rather than end the command there.
            // Grouped and attached in one: the group gives the pane its own
            // size and its own selected window, and `destroy-unattached`
            // takes the group away when you leave.
            Some((session, window)) => [
                format!("tmux new-session -t '{session}'"),
                "set destroy-unattached on".to_string(),
                "set -w aggressive-resize on".to_string(),
                format!("select-window -t '{window}'"),
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
            Some(remote) => format!("{}:{}", remote.host, named(&self.cwd)),
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
/// The last component of a path, as a name — what a directory is called, with
/// nothing asked of any filesystem.
fn named(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string_lossy().to_string())
}

fn repo_of(cwd: &Path) -> String {
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSessionFile {
    job_id: Option<String>,
}

/// The panel's order, applied wherever jobs are put together: local ones as
/// they are read, and again once the machines' rows are mixed in.
pub fn sort(jobs: &mut [Job]) {
    // The id breaks ties last: two machines can hold sessions with the same
    // derived name, and a list that reordered itself between batches would be
    // the "slot machine" all over again.
    jobs.sort_by(|a, b| {
        a.status
            .cmp(&b.status)
            .then(a.name.cmp(&b.name))
            .then(a.short.cmp(&b.short))
    });
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
        machine: None,
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
    fn a_change_of_status_is_the_one_thing_that_moves_a_row() {
        let f = Fixture::new("job-status-moves")
            .job("a", r#"{"state":"working","name":"ALPHA"}"#)
            .job("b", r#"{"state":"working","name":"BETA"}"#);
        let names =
            |s: &Snapshot| -> Vec<String> { s.jobs.iter().map(|j| j.name.clone()).collect() };
        assert_eq!(names(&load(&f.0).unwrap()), ["ALPHA", "BETA"]);

        // BETA starts asking, and asking sorts above working — a move you
        // want to see, unlike a timestamp ticking.
        std::fs::write(
            f.0.join("b").join("state.json"),
            r#"{"state":"working","name":"BETA","needs":"answer: which?"}"#,
        )
        .unwrap();
        assert_eq!(names(&load(&f.0).unwrap()), ["BETA", "ALPHA"]);
    }

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
            tmux: Some("autodad:@2.%2".to_string()),
        });
        assert_eq!(
            job.open_command_line(),
            "ssh claude-box · tmux autodad:@2.%2"
        );
    }

    #[test]
    fn a_session_on_another_machine_is_opened_through_tmux() {
        // The box has no daemon and no jobs directory — `claude attach` has
        // nothing to attach to there. What it has is tmux, and the session's
        // own json says which window.
        let remote = Remote {
            host: "claude-box".to_string(),
            tmux: Some("autodad:@2.%2".to_string()),
        };
        let command = remote.open_command();

        assert_eq!(command[0], "ssh");
        // A tty, or tmux refuses to attach at all.
        assert!(command.contains(&"-t".to_string()), "{command:?}");
        assert!(command.contains(&"claude-box".to_string()), "{command:?}");

        let script = command.last().unwrap();
        // Grouped, not attached: the user is already attached from their own
        // terminal, and a second client would force both to the smaller size.
        assert!(script.contains("new-session -t 'autodad'"), "{script}");
        assert!(script.contains("select-window -t '@2'"), "{script}");
        // And it takes itself away when the tab closes, so the panel does not
        // litter the machine with grouped sessions.
        assert!(script.contains("destroy-unattached on"), "{script}");
        // Escaped, because the remote shell would otherwise end the command
        // at the semicolon instead of handing tmux one.
        assert!(script.contains("\\;"), "{script}");
    }

    #[test]
    fn a_machine_with_no_tmux_window_is_still_worth_opening() {
        let remote = Remote {
            host: "claude-box".to_string(),
            tmux: None,
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
