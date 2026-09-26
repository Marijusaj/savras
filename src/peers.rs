//! Who is working in a repository on this machine — the sessions a board
//! message can be addressed to.
//!
//! One answer for three askers: `svr board who` lists them, `svr board post
//! --to` checks a name against them, and the relay pings them. Asked
//! separately, the three would drift, and a name `who` shows is exactly the
//! name `--to` must accept.

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
        let you = if me.is_some_and(|me| me.eq_ignore_ascii_case(&job.name)) {
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

/// The reader `name` means, spelled the way the board spells it: a session
/// working here, whole name in any case, or the owner. `None` when nobody
/// here is called that.
pub fn resolve(peers: &[&Job], name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('@').trim();
    if name.eq_ignore_ascii_case(OWNER) {
        return Some(OWNER.to_string());
    }
    peers
        .iter()
        .find(|job| job.name.eq_ignore_ascii_case(name))
        .map(|job| job.name.clone())
}

/// The readers `text` names with `@NAME`, among the sessions working here and
/// the owner. The longest name wins where one is the start of another, and
/// a word that names nobody here is left as words — `@decorator` is not a
/// session.
pub fn mentioned(text: &str, peers: &[&Job]) -> Vec<String> {
    let mut names: Vec<&str> = peers.iter().map(|job| job.name.as_str()).collect();
    names.push(OWNER);
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if mentions(text, name) && !out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            out.push(name.to_string());
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
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn job(name: &str, cwd: &str) -> Job {
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
        assert_eq!(resolve(&peers, "savras 13").as_deref(), Some("SAVRAS 13"));
        assert_eq!(resolve(&peers, "@SAVRAS 1").as_deref(), Some("SAVRAS 1"));
        assert_eq!(resolve(&peers, "Owner").as_deref(), Some("owner"));
        assert_eq!(resolve(&peers, "SAVRAS13"), None);
    }

    #[test]
    fn mentions_are_whole_names_and_the_longer_name_is_not_also_the_shorter() {
        let jobs = vec![
            job("SAVRAS 1", "/r"),
            job("SAVRAS 13", "/r"),
            job("LEAD", "/r"),
        ];
        let peers = in_repo(&jobs, "/r");
        assert_eq!(
            mentioned("@savras 13 and @owner — who holds main.rs?", &peers),
            ["SAVRAS 13", "owner"]
        );
        assert_eq!(
            mentioned("@LEAD-2 and @decorator", &peers),
            Vec::<String>::new()
        );
        assert!(mentions("ping @SAVRAS 13, please", "SAVRAS 13"));
        assert!(!mentions("@SAVRAS 13 is on it", "SAVRAS 1"));
        assert!(!mentions("@", ""));
    }
}
