//! Who is working in a repository on this machine — the sessions a board
//! message can be addressed to.
//!
//! One answer for every asker: `svr board who` lists them, `svr board post
//! --to` checks a name against them, the relay pings them, and the hook tells
//! a session who they are and when that changes. Asked separately, they would
//! drift, and a name `who` shows is exactly the name `--to` must accept.
//!
//! **Savras is the only thing that sees both clients.** Claude Code's
//! `ListAgents` does not list Codex, and Codex lists no Claude session, so
//! this list is the one an agent can address anybody from.
//!
//! **A session is its id, and its name is what it is called today.** The
//! owner renames sessions whenever they like; a message is addressed to the
//! id behind the name, so it still reaches the session after a rename, and a
//! rename is something to tell the others rather than a new session.

use serde::{Deserialize, Serialize};

use crate::board::{self, OWNER};
use crate::job::{self, Client, Job, Status};

/// Every session on this machine: Claude Code's jobs and Codex's threads.
pub fn all() -> Vec<Job> {
    let mut jobs = job::default_jobs_dir()
        .ok()
        .and_then(|dir| job::load(&dir).ok())
        .map(|s| s.jobs)
        .unwrap_or_default();
    if let Some(dir) = crate::codex::default_dir() {
        jobs.extend(crate::codex::load(&dir));
    }
    jobs
}

/// The sessions of `jobs` working in `repo` on this machine, a worktree's
/// included — a board belongs to the repository, not to one checkout of it.
pub fn in_repo<'a>(jobs: &'a [Job], repo: &str) -> Vec<&'a Job> {
    jobs.iter()
        .filter(|job| job.machine.is_none())
        .filter(|job| board::topic_of(&job.cwd) == repo)
        .collect()
}

/// Which program a session is, in words.
pub fn client_word(client: Client) -> &'static str {
    match client {
        Client::Claude => "Claude Code",
        Client::Codex => "Codex",
    }
}

/// What a session is doing, in words.
pub fn status_word(status: Status) -> &'static str {
    match status {
        Status::NeedsInput => "waiting for the owner",
        Status::Working => "working",
        Status::Done => "idle",
    }
}

/// The session `jobs` says is the one asking: by the session id a hook is
/// handed, or else by the job directory Claude Code gives a background
/// session. `None` for a shell of the owner's, or a session savras cannot see.
pub fn me<'a>(jobs: &'a [Job], session: Option<&str>) -> Option<&'a Job> {
    if let Some(id) = session.map(str::trim).filter(|id| !id.is_empty()) {
        if let Some(job) = jobs.iter().find(|job| job.session_id == id) {
            return Some(job);
        }
    }
    let dir = std::env::var("CLAUDE_JOB_DIR").ok()?;
    let dir = std::path::Path::new(&dir);
    // It points at the job's directory or at its `tmp`, depending on version.
    let short = [Some(dir), dir.parent()]
        .into_iter()
        .flatten()
        .filter_map(|d| d.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .find(|n| n != "tmp")?;
    jobs.iter()
        .find(|job| job.client == Client::Claude && job.short == short)
}

/// One line of a board's roster: who a session is, as the board last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub codex: bool,
}

/// The roster of `peers`, as it is now.
pub fn roster(peers: &[&Job]) -> Vec<Peer> {
    peers
        .iter()
        .map(|job| Peer {
            id: job.session_id.clone(),
            name: job.name.clone(),
            codex: job.client == Client::Codex,
        })
        .collect()
}

/// What changed between two looks at a board's roster, in words, for the
/// session `me` — which is told its own new name rather than about itself.
pub fn changes(before: &[Peer], now: &[Peer], me: Option<&str>) -> Vec<String> {
    let kind = |p: &Peer| if p.codex { "Codex" } else { "Claude Code" };
    let mut out = Vec::new();
    for p in now {
        let mine = me == Some(p.id.as_str());
        match before.iter().find(|b| b.id == p.id) {
            Some(b) if b.name != p.name && mine => out.push(format!(
                "you are now {:?} on the board — the name the others use for you",
                p.name
            )),
            Some(b) if b.name != p.name => {
                out.push(format!("{:?} is now called {:?}", b.name, p.name))
            }
            Some(_) => {}
            None if mine => {}
            None => out.push(format!("{:?} ({}) started working here", p.name, kind(p))),
        }
    }
    for b in before {
        if !now.iter().any(|p| p.id == b.id) && me != Some(b.id.as_str()) {
            out.push(format!("{:?} is no longer working here", b.name));
        }
    }
    out
}

/// Who is working in `repo`, one per line, as `who` and a refused post say
/// it: the name to use first, since the name is what the reader came for.
pub fn listing(peers: &[&Job], repo: &str, me: Option<&str>) -> String {
    if peers.is_empty() {
        return format!(
            "nobody is working in {} on this machine now — only `--to owner` or \
             `--everyone` reach anyone",
            board::topic_name(repo)
        );
    }
    let mut out = format!("working in {} now:", board::topic_name(repo));
    for job in peers {
        let you = if me.is_some_and(|me| me == job.session_id) {
            " (you)"
        } else {
            ""
        };
        let said = board::flatten(&job.summary);
        let said: String = said.chars().take(70).collect();
        out.push_str(&format!(
            "\n  {:?}{you} — {}, {}{}",
            job.name,
            client_word(job.client),
            status_word(job.status),
            if said.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", said.trim())
            }
        ));
    }
    out.push_str("\n  \"owner\" — the person at the keyboard");
    out
}

/// Somebody a message is for: the name it was addressed by, as the board
/// spells it, and the session behind it — none for the owner, who is a
/// person rather than a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub name: String,
    pub id: Option<String>,
}

impl Reader {
    fn of(job: &Job) -> Self {
        Reader {
            name: job.name.clone(),
            id: Some(job.session_id.clone()),
        }
    }

    fn owner() -> Self {
        Reader {
            name: OWNER.to_string(),
            id: None,
        }
    }
}

/// The reader `name` means: a session working here, whole name in any case,
/// or the owner. `None` when nobody here is called that.
pub fn resolve(peers: &[&Job], name: &str) -> Option<Reader> {
    let name = name.trim().trim_start_matches('@').trim();
    if name.eq_ignore_ascii_case(OWNER) {
        return Some(Reader::owner());
    }
    peers
        .iter()
        .find(|job| job.name.eq_ignore_ascii_case(name))
        .map(|job| Reader::of(job))
}

/// The readers `text` names with `@NAME`, among the sessions working here and
/// the owner. The longest name wins where one is the start of another, and
/// a word that names nobody here is left as words — `@decorator` is not a
/// session.
pub fn mentioned(text: &str, peers: &[&Job]) -> Vec<Reader> {
    let mut named: Vec<Reader> = peers.iter().map(|job| Reader::of(job)).collect();
    named.push(Reader::owner());
    named.sort_by_key(|r| std::cmp::Reverse(r.name.len()));
    let mut out: Vec<Reader> = Vec::new();
    for reader in named {
        if mentions(text, &reader.name) && !out.iter().any(|r| r.id == reader.id) {
            out.push(reader);
        }
    }
    out
}

/// Whether `text` says `@name`, whole — `@SAVRAS 1` is not `@SAVRAS 13`.
pub fn mentions(text: &str, name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let at = format!("@{}", name.to_lowercase());
    text.match_indices(&at).any(|(i, _)| {
        text[i + at.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(crate) fn job(name: &str, cwd: &str) -> Job {
        Job {
            short: name.to_lowercase(),
            name: name.into(),
            color: None,
            status: Status::Done,
            summary: String::new(),
            cwd: PathBuf::from(cwd),
            session_id: format!("{name}-id"),
            tokens: 0,
            updated_at: None,
            links: Vec::new(),
            backend: None,
            daemon_short: None,
            machine: None,
            created_at: None,
            model: None,
            context: None,
            context_window: None,
            failed: false,
            deploy: None,
            client: Client::Claude,
        }
    }

    #[test]
    fn only_this_machines_sessions_in_the_repository_are_peers() {
        let mut away = job("AWAY", "/code/web-app");
        away.machine = Some(crate::job::Remote {
            host: "box".into(),
            tmux: None,
            pid: None,
        });
        let jobs = vec![
            job("LEAD", "/code/web-app"),
            job("OTHER", "/code/other"),
            away,
        ];
        let names: Vec<_> = in_repo(&jobs, "/code/web-app")
            .iter()
            .map(|j| j.name.as_str())
            .collect();
        assert_eq!(names, ["LEAD"]);
    }

    #[test]
    fn a_name_resolves_whole_in_any_case_and_the_owner_is_always_there() {
        let jobs = vec![job("SAVRAS 13", "/r"), job("SAVRAS 1", "/r")];
        let peers = in_repo(&jobs, "/r");
        let r = resolve(&peers, "savras 13").unwrap();
        assert_eq!(
            (r.name.as_str(), r.id.as_deref()),
            ("SAVRAS 13", Some("SAVRAS 13-id"))
        );
        assert_eq!(resolve(&peers, "@SAVRAS 1").unwrap().name, "SAVRAS 1");
        assert_eq!(resolve(&peers, "Owner").unwrap(), Reader::owner());
        assert_eq!(resolve(&peers, "SAVRAS13"), None);
    }

    #[test]
    fn a_hook_finds_its_own_session_by_the_id_it_is_handed() {
        let jobs = vec![job("LEAD", "/r"), job("HELPER", "/r")];
        assert_eq!(me(&jobs, Some("HELPER-id")).unwrap().name, "HELPER");
        assert!(me(&jobs, Some("nobody")).is_none() || std::env::var("CLAUDE_JOB_DIR").is_ok());
    }

    fn peer(id: &str, name: &str, codex: bool) -> Peer {
        Peer {
            id: id.into(),
            name: name.into(),
            codex,
        }
    }

    #[test]
    fn the_roster_says_renames_arrivals_and_departures_and_tells_you_your_name() {
        let before = [
            peer("a", "95141db1", false),
            peer("b", "SAVRAS 13", false),
            peer("me", "SAVRAS 14", false),
        ];
        let now = [
            peer("a", "REVIEWER", false),
            peer("me", "LEAD", false),
            peer("c", "codex-x", true),
        ];
        assert_eq!(
            changes(&before, &now, Some("me")),
            [
                "\"95141db1\" is now called \"REVIEWER\"",
                "you are now \"LEAD\" on the board — the name the others use for you",
                "\"codex-x\" (Codex) started working here",
                "\"SAVRAS 13\" is no longer working here",
            ]
        );
        assert!(changes(&now, &now, Some("me")).is_empty());
    }

    #[test]
    fn mentions_are_whole_names_and_the_longer_name_is_not_also_the_shorter() {
        let jobs = vec![
            job("SAVRAS 1", "/r"),
            job("SAVRAS 13", "/r"),
            job("LEAD", "/r"),
        ];
        let peers = in_repo(&jobs, "/r");
        let names = |text: &str| -> Vec<String> {
            mentioned(text, &peers)
                .into_iter()
                .map(|r| r.name)
                .collect()
        };
        assert_eq!(
            names("@savras 13 and @owner — who holds main.rs?"),
            ["SAVRAS 13", "owner"]
        );
        assert!(names("@LEAD-2 and @decorator").is_empty());
        assert!(mentions("ping @SAVRAS 13, please", "SAVRAS 13"));
        assert!(!mentions("@SAVRAS 13 is on it", "SAVRAS 1"));
        assert!(!mentions("@", ""));
    }
}
