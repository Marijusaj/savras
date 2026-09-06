//! Parallel agents: a lead session, and the agents that take their work from it.
//!
//! Not subagents. A subagent is spawned by a model, lives inside one session's
//! turn and reports into it. A *parallel agent* is a session of its own, with
//! its own context and its own panel row, that happens to take its work from
//! another session. It keeps running between assignments, you can open it and
//! talk to it directly, and it can message its peers.
//!
//! There is nothing to configure, because the names already say it. Claude
//! Code names a second session with the same name `NAME-2`, a third `NAME-3`,
//! and that is exactly the shape of a group: the bare name leads, the numbered
//! ones follow. Savras reads the group out of the session list the same way it
//! reads everything else — by looking, not by being told.
//!
//! Deciding is separate from doing, as with the ping: everything here is pure
//! and tested without starting a process. [`start`] is the one part that runs
//! a command.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::job::{Job, Snapshot};

/// The lead a session name belongs to, and its number within that lead.
///
/// `AGENT` is `("AGENT", None)` — the lead itself. `AGENT-3` is
/// `("AGENT", Some(3))`. A trailing number is only read as a group number when
/// there is something in front of it: `-2` on its own names nothing.
pub fn split(name: &str) -> (&str, Option<u32>) {
    match name.rsplit_once('-') {
        Some((lead, tail)) if !lead.is_empty() && !tail.is_empty() => match tail.parse::<u32>() {
            Ok(n) => (lead, Some(n)),
            Err(_) => (name, None),
        },
        _ => (name, None),
    }
}

/// A lead and the parallel agents taking their work from it.
pub struct Group<'a> {
    pub lead: &'a Job,
    pub agents: Vec<&'a Job>,
}

impl Group<'_> {
    /// What the panel says about a session: its place in its own group.
    pub fn describe(&self, job: &Job) -> String {
        let names: Vec<&str> = self.agents.iter().map(|j| j.name.as_str()).collect();
        if job.name == self.lead.name {
            format!("leads {}", names.join(", "))
        } else {
            format!("parallel agent under {}", self.lead.name)
        }
    }
}

/// Every running session sharing a base name, lowest number first, with the
/// bare name — when there is one — at the head.
fn family<'a>(snapshot: &'a Snapshot, base: &str) -> Vec<&'a Job> {
    let mut found: Vec<(Option<u32>, &Job)> = snapshot
        .jobs
        .iter()
        .filter_map(|j| match split(&j.name) {
            (theirs, number) if theirs == base => Some((number, j)),
            _ => None,
        })
        .collect();
    // `None` is the bare name, and sorts first: it is the session the others
    // were named after.
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
    found.into_iter().map(|(_, j)| j).collect()
}

/// The session that leads `job`'s group — the bare name if it is running, and
/// otherwise the lowest-numbered agent.
///
/// It must be a session that actually exists, because the whole point of the
/// name is that the others can *message* it. Naming a lead that is not running
/// would hand every new agent an address that goes nowhere.
///
/// A session with no siblings leads itself: pressing "add an agent" on a lone
/// session is how a group starts.
pub fn lead_of<'a>(snapshot: &'a Snapshot, job: &'a Job) -> &'a Job {
    let (base, _) = split(&job.name);
    family(snapshot, base).first().copied().unwrap_or(job)
}

/// The group `job` belongs to, if it is in one.
///
/// Two sessions sharing a base name are a group; one is just a session. That
/// threshold is also what stops the panel inventing a hierarchy out of a
/// hyphen — a lone `PR-357` is nobody's parallel agent.
///
/// The lead does not have to be the bare name, and does not have to still be
/// running: `SAVRAS-2` and `SAVRAS-3` with no `SAVRAS` anywhere are a group
/// led by `SAVRAS-2`. A group that dissolved because one session exited would
/// be worse than one whose lead is numbered.
pub fn group_of<'a>(snapshot: &'a Snapshot, job: &Job) -> Option<Group<'a>> {
    let (base, _) = split(&job.name);
    let mut family = family(snapshot, base);
    if family.len() < 2 {
        return None;
    }
    let lead = family.remove(0);
    Some(Group {
        lead,
        agents: family,
    })
}

/// The name for the next parallel agent under `lead`.
///
/// The lowest free number, so closing `AGENT-2` and starting another gives you
/// `AGENT-2` again rather than climbing forever. Numbering starts at 2 because
/// the lead is 1: it is the session you already had.
pub fn next_name(snapshot: &Snapshot, lead: &str) -> String {
    let taken: Vec<u32> = snapshot
        .jobs
        .iter()
        .filter_map(|j| match split(&j.name) {
            (theirs, Some(n)) if theirs == lead => Some(n),
            _ => None,
        })
        .collect();
    let mut n = 2;
    while taken.contains(&n) {
        n += 1;
    }
    format!("{lead}-{n}")
}

/// What a new parallel agent is told, as its opening prompt.
///
/// The opening prompt is the whole trick. There is no supported way to put
/// words into a session that is already running — so instead of messaging an
/// agent after starting it, Savras starts it *with* what it needs to know.
/// That makes the briefing genuinely sent rather than drafted, and it arrives
/// before the agent has done anything.
///
/// The lead is told by the agent itself, in its first act. Savras never
/// interrupts a running session; Claude Code's own messaging does the rest.
pub fn briefing(lead: &str, name: &str) -> String {
    format!(
        "You are {name}, a parallel agent working under {lead}.\n\n\
         {lead} plans the work and hands it out; you do the piece you are given \
         and report back to it. You are a session in your own right, not a \
         subagent: you keep your context between assignments, and you keep \
         running when the work is done.\n\n\
         Do this first, before anything else: message {lead} to say you are up \
         and ask what it needs — SendMessage({{\"to\": \"{lead}\", \"message\": \
         \"{name} here, ready. What do you need?\"}}). Use ListAgents to see the \
         rest of the group; you can message the other agents directly when the \
         work calls for it.\n\n\
         Then wait for {lead} to tell you what to do. Do not pick work up on \
         your own — the point of the group is that {lead} decides who does what."
    )
}

/// Start a parallel agent: a real background session, named for its lead, with
/// the briefing as its opening prompt.
///
/// `claude --bg` returns as soon as the session is registered and prints its
/// id, but "as soon as" is still long enough to freeze a panel that is
/// redrawing sixty times a second, so callers run this off the event loop.
pub fn start(name: &str, briefing: &str, cwd: &Path) -> Result<String> {
    let mut command = Command::new("claude");
    command
        .arg("--bg")
        .arg("-n")
        .arg(name)
        .arg(briefing)
        .stdin(Stdio::null());
    if cwd.is_dir() {
        command.current_dir(cwd);
    }

    let out = command
        .output()
        .context("running `claude --bg` — is Claude Code on your PATH?")?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        anyhow::bail!(
            "claude --bg failed{}",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", first_line(why))
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Delete a session: stop it, then remove it.
///
/// Two commands because they mean different things to Claude Code. `stop`
/// ends the session and keeps its conversation; `rm` takes the session out of
/// the list for good, and its worktree with it where that is safe. A session
/// that has already exited has nothing to stop, so a failing `stop` is not an
/// error — the delete is what was asked for, and `rm` is the part that has to
/// work.
///
/// This is the second thing Savras does that is not looking, and like starting
/// an agent it goes through Claude Code's own commands rather than touching
/// `~/.claude/` itself. Which is the point: the daemon knows what a session is
/// and what deleting one entails, and a directory removed behind its back
/// leaves it believing otherwise.
pub fn delete(short: &str) -> Result<String> {
    let _ = Command::new("claude")
        .arg("stop")
        .arg(short)
        .stdin(Stdio::null())
        .output();

    let out = Command::new("claude")
        .arg("rm")
        .arg(short)
        .stdin(Stdio::null())
        .output()
        .context("running `claude rm` — is Claude Code on your PATH?")?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        anyhow::bail!(
            "claude rm failed{}",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", first_line(why))
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job;
    use crate::testing::Fixture;

    fn snap(f: &Fixture) -> Snapshot {
        job::load(&f.0).unwrap()
    }

    fn named(name: &str) -> String {
        format!(r#"{{"state":"working","name":"{name}","cwd":"/tmp/repo"}}"#)
    }

    fn job<'a>(snapshot: &'a Snapshot, name: &str) -> &'a Job {
        snapshot.jobs.iter().find(|j| j.name == name).unwrap()
    }

    #[test]
    fn a_name_says_which_group_it_is_in() {
        assert_eq!(split("AGENT"), ("AGENT", None));
        assert_eq!(split("AGENT-2"), ("AGENT", Some(2)));
        assert_eq!(split("SAVRAS-12"), ("SAVRAS", Some(12)));
        // A hyphen with no number is just a name.
        assert_eq!(split("auto-dad"), ("auto-dad", None));
        assert_eq!(split("PLAN-beta"), ("PLAN-beta", None));
        // Nothing in front of the number names nothing.
        assert_eq!(split("-2"), ("-2", None));
        assert_eq!(split(""), ("", None));
    }

    #[test]
    fn the_group_is_read_out_of_the_names() {
        let f = Fixture::new("agents-group")
            .job("a", &named("AGENT"))
            .job("b", &named("AGENT-2"))
            .job("c", &named("AGENT-3"))
            .job("d", &named("OTHER"));
        let s = snap(&f);

        let group = group_of(&s, job(&s, "AGENT")).unwrap();
        assert_eq!(group.lead.name, "AGENT");
        let mut names: Vec<&str> = group.agents.iter().map(|j| j.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["AGENT-2", "AGENT-3"]);

        // From a member, the same group.
        let from_member = group_of(&s, job(&s, "AGENT-3")).unwrap();
        assert_eq!(from_member.lead.name, "AGENT");
        assert_eq!(from_member.agents.len(), 2);

        // A session on its own is in no group.
        assert!(group_of(&s, job(&s, "OTHER")).is_none());
    }

    #[test]
    fn a_hyphenated_number_with_no_lead_running_is_not_a_group() {
        // `PR-357` must not conjure a lead called `PR` that does not exist.
        let f = Fixture::new("agents-nolead").job("a", &named("PR-357"));
        let s = snap(&f);
        assert!(group_of(&s, job(&s, "PR-357")).is_none());
    }

    #[test]
    fn a_group_survives_losing_the_session_it_was_named_after() {
        // Sessions are often named `X-2`, `X-3` with no bare `X` ever running,
        // and a lead that exited must not dissolve the group — the remaining
        // agents still need somewhere to report.
        let f = Fixture::new("agents-nobare")
            .job("b", &named("SAVRAS-2"))
            .job("c", &named("SAVRAS-3"));
        let s = snap(&f);

        let group = group_of(&s, job(&s, "SAVRAS-3")).unwrap();
        assert_eq!(group.lead.name, "SAVRAS-2", "the lowest number leads");
        assert_eq!(group.agents.len(), 1);
        assert_eq!(group.agents[0].name, "SAVRAS-3");
    }

    #[test]
    fn the_lead_is_always_a_session_you_could_message() {
        let f = Fixture::new("agents-lead")
            .job("a", &named("AGENT"))
            .job("b", &named("AGENT-2"))
            .job("c", &named("ALONE"));
        let s = snap(&f);

        assert_eq!(lead_of(&s, job(&s, "AGENT-2")).name, "AGENT");
        assert_eq!(lead_of(&s, job(&s, "AGENT")).name, "AGENT");
        // A session with no siblings leads itself: that is how a group starts.
        assert_eq!(lead_of(&s, job(&s, "ALONE")).name, "ALONE");
    }

    #[test]
    fn the_next_agent_takes_the_lowest_free_number() {
        let f = Fixture::new("agents-next")
            .job("a", &named("AGENT"))
            .job("b", &named("AGENT-2"))
            .job("c", &named("AGENT-4"));
        let s = snap(&f);
        // Numbering starts at 2: the lead is the one you already had.
        assert_eq!(next_name(&s, "AGENT"), "AGENT-3");
        assert_eq!(next_name(&s, "FRESH"), "FRESH-2");
    }

    #[test]
    fn the_panel_says_where_a_session_sits_in_its_group() {
        let f = Fixture::new("agents-describe")
            .job("a", &named("AGENT"))
            .job("b", &named("AGENT-2"));
        let s = snap(&f);
        let group = group_of(&s, job(&s, "AGENT")).unwrap();

        assert_eq!(group.describe(job(&s, "AGENT")), "leads AGENT-2");
        assert_eq!(
            group.describe(job(&s, "AGENT-2")),
            "parallel agent under AGENT"
        );
    }

    #[test]
    fn the_briefing_names_both_ends_and_says_to_report_in() {
        // It is the opening prompt of a real session, so it has to stand on its
        // own: the agent has no other way of learning any of this.
        let text = briefing("AGENT", "AGENT-3");
        assert!(text.starts_with("You are AGENT-3, a parallel agent working under AGENT."));
        assert!(text.contains(r#"SendMessage({"to": "AGENT", "message": "#));
        assert!(
            text.contains("not a subagent"),
            "the distinction is the whole point: {text}"
        );
        assert!(text.contains("ListAgents"), "it must be able to find peers");
        assert!(text.contains("wait for AGENT"));
    }
}
