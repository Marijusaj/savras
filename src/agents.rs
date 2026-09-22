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
use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::app::App;
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
    /// Whether `lead` really is the session the others were named after — the
    /// bare name. When it is not, the group has lost its lead and what is
    /// left are siblings.
    pub led: bool,
}

impl Group<'_> {
    /// What the panel says about a session: its place in its own group.
    ///
    /// With the lead gone, nobody is promoted into its place: `STAGING-2` was
    /// never `STAGING-3`'s parent, and saying so would invent a chain of
    /// command out of a deleted row.
    pub fn describe(&self, job: &Job) -> String {
        let named = |others: Vec<&Job>| -> String {
            others
                .iter()
                .map(|j| j.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !self.led {
            let (base, _) = split(&job.name);
            let others = std::iter::once(self.lead)
                .chain(self.agents.iter().copied())
                .filter(|j| j.name != job.name)
                .collect();
            return format!(
                "started alongside {} · no {base} to lead them",
                named(others)
            );
        }
        if job.name == self.lead.name {
            format!("leads {}", named(self.agents.clone()))
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
/// The group survives its lead, but nothing takes the lead's place:
/// `SAVRAS-2` and `SAVRAS-3` with no `SAVRAS` anywhere are still one group —
/// they were started together and share a name — and `led` is false, so the
/// panel says they stand alongside each other rather than promoting the
/// lowest number to parent.
pub fn group_of<'a>(snapshot: &'a Snapshot, job: &Job) -> Option<Group<'a>> {
    let (base, _) = split(&job.name);
    let mut family = family(snapshot, base);
    if family.len() < 2 {
        return None;
    }
    let lead = family.remove(0);
    Some(Group {
        led: lead.name == base,
        lead,
        agents: family,
    })
}

/// The name for the next parallel agent under `lead`, which is the lead's own
/// *name* — `AGENT` or `AGENT-4` — not its base.
///
/// The lowest free number above the lead's, so closing `AGENT-2` and starting
/// another gives you `AGENT-2` again rather than climbing forever. Numbering
/// starts one above the lead because [`lead_of`] hands the group to its lowest
/// number: an agent numbered below the lead would be told to report to a
/// session that the panel no longer calls the lead. With `SAVRAS-4` leading
/// `SAVRAS-5`, the free `2` is left alone and the next agent is `SAVRAS-6`.
///
/// A bare lead counts as 1 — it is the session you already had.
pub fn next_name(snapshot: &Snapshot, lead: &str) -> String {
    let (base, number) = split(lead);
    let taken: Vec<u32> = snapshot
        .jobs
        .iter()
        .filter_map(|j| match split(&j.name) {
            (theirs, Some(n)) if theirs == base => Some(n),
            _ => None,
        })
        .collect();
    let mut n = number.unwrap_or(1) + 1;
    while taken.contains(&n) {
        n += 1;
    }
    format!("{base}-{n}")
}

/// One heading's sessions, ordered so that every lead is followed by its own
/// agents, with `true` against the ones that are agents.
///
/// The list has to **hold still** — the flip keys walk it, and a row that
/// moves while you are looking at it makes them a lottery (M2.2). So a family
/// is placed by two things that do not change while it runs: when its oldest
/// session was started, and its lead's name. An agent asking a question or
/// finishing one does not reshuffle the group; what it is doing is said in
/// its own row.
///
/// Inside a family the order is the lead, then its agents by number. A session
/// with no siblings *here* is a family of one and comes out exactly as it went
/// in: `group_of` needs two to call it a group, and a lone `PR-357` is a row,
/// not an orphan indented under nothing. "Here" matters — the lead has to be
/// in this same heading, or the indent would point at a row that is not on
/// screen.
///
/// **No agent is promoted to lead.** The lead is the session with the bare
/// name and nothing else is: delete `STAGING` and `STAGING-2` does not become
/// the parent of `STAGING-3` — it never was one, and nothing on disk changed.
/// Its agents outlive it as ordinary rows, side by side, because an indent
/// under a session that is gone claims a relationship that no longer exists.
pub fn under_leads(snapshot: &Snapshot, group: &[usize]) -> Vec<(usize, bool)> {
    // Families, keyed by the base name they share, in this heading only.
    let mut families: Vec<(String, Vec<usize>)> = Vec::new();
    for &at in group {
        let (base, _) = split(&snapshot.jobs[at].name);
        match families.iter_mut().find(|(had, _)| had == base) {
            Some((_, members)) => members.push(at),
            None => families.push((base.to_string(), vec![at])),
        }
    }

    for (_, members) in families.iter_mut() {
        // The bare name first — it is the session the others were named after
        // — then by number, so the order is the one the names already imply.
        members.sort_by_key(|&at| {
            let (_, number) = split(&snapshot.jobs[at].name);
            (number, snapshot.jobs[at].name.clone())
        });
    }

    // A family is as old as its oldest member — the lead, usually, since its
    // agents were started from it. Ordering by that and not by status is what
    // keeps a row still while you are reaching for it.
    families.sort_by(|a, b| {
        let age = |members: &Vec<usize>| {
            members
                .iter()
                .map(|&at| crate::job::started(&snapshot.jobs[at]))
                .min()
                .unwrap_or((true, None))
        };
        age(&a.1)
            .cmp(&age(&b.1))
            .then_with(|| snapshot.jobs[a.1[0]].name.cmp(&snapshot.jobs[b.1[0]].name))
    });

    families
        .into_iter()
        .flat_map(|(base, members)| {
            // Only the bare name leads. Without it — it was deleted, or it is
            // under another heading — these are siblings, not a family, and
            // none of them is indented under any of the others.
            let led = snapshot.jobs[members[0]].name == base;
            let alone = members.len() < 2;
            members
                .into_iter()
                .enumerate()
                .map(move |(n, at)| (at, led && !alone && n > 0))
        })
        .collect()
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
/// Stop a session on another machine.
///
/// There is no daemon over there to ask — a box you ssh into runs `claude` as
/// an ordinary process — so the session is stopped by signalling it, with the
/// same `TERM` that closing its terminal would send. Nothing is removed: the
/// far side leaves `sessions/<pid>.json` behind whatever happens, and the
/// watcher already refuses to show a pid that no longer answers `kill -0`, so
/// the row goes when the process does and not before.
///
/// This is the read-only rule bending a second time, and in the same shape as
/// the first: Savras still writes nothing and still speaks no private
/// protocol. It signals a process, which is what `d` already does here.
pub fn stop_remote(host: &str, pid: u32) -> Result<String> {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes"])
        // See `remote::watch`: a host name is not allowed to be an option.
        .arg("--")
        .arg(host)
        .arg("kill")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running ssh {host} kill {pid}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        anyhow::bail!(
            "could not stop it on {host}{}",
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", first_line(why))
            }
        );
    }
    Ok(String::new())
}

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

/// Start a parallel agent under the panel's selected session, off the main
/// thread, and say so on the panel while it happens.
///
/// The lead is the selected session's, so pressing this on `AGENT-3` adds a
/// fourth agent under `AGENT` rather than starting a group beneath a group. It
/// runs in the lead's own repository, because an agent that cannot see the
/// code is no use.
///
/// This is the one thing Savras does that is not looking: it *starts*
/// sessions. It still never writes to `~/.claude/`, and it still never
/// interrupts a session that is already running — the new agent introduces
/// itself to its lead, through Claude Code's own messaging, as its first act.
///
/// Both panels press this key — the side panel and `svr solo` — so it lives
/// here rather than in either of their loops.
pub fn add(app: &mut App, outcome: &mpsc::Sender<Result<String>>) {
    let Some(job) = app.selected_job() else {
        return;
    };
    // An agent is a `claude --bg` started *here*, in the lead's own directory.
    // For a session on another machine that directory is on that machine, so
    // there is nothing to start it in and nothing for it to read — it would
    // come up somewhere arbitrary on this box and be told to report to a
    // session it cannot reach. Refusing is the honest answer, and saying which
    // machine is what makes it obvious rather than mysterious.
    if let Some(remote) = &job.machine {
        app.error = Some(format!(
            "{} is on {}; an agent starts on this machine",
            job.name, remote.host
        ));
        return;
    }
    // The lead has to be a session that is actually running, because the new
    // agent is told to message it by name. The numbering follows the *lead*,
    // not the selected session and not the bare base name: a group led by
    // `SAVRAS-4` adds `SAVRAS-6` next, leaving the free `2` alone rather than
    // handing the newcomer a number that would make it the lead.
    let leader = lead_of(&app.snapshot, job);
    let lead = leader.name.clone();
    // Its own repository: an agent that cannot see the code is no use.
    let cwd = leader.cwd.clone();
    let name = next_name(&app.snapshot, &lead);

    app.error = Some(format!("starting {name}…"));
    let outcome = outcome.clone();
    std::thread::spawn(move || {
        let briefing = briefing(&lead, &name);
        let _ = outcome.send(start(&name, &briefing, &cwd).context("could not start an agent"));
    });
}

/// Take whatever the errand threads have finished with, and say only what went
/// wrong. `true` when the screen needs redrawing.
///
/// Success is silent on purpose: a session that started writes its own
/// `state.json` within a moment and the panel is watching that directory, so
/// the row appears by itself — and a deleted one disappears the same way.
pub fn settle(app: &mut App, outcome: &mpsc::Receiver<Result<String>>) -> bool {
    let mut news = false;
    while let Ok(done) = outcome.try_recv() {
        match done {
            Ok(_) => app.error = None,
            // Already worded where it was raised: one channel carries more
            // than one kind of errand.
            Err(e) => app.error = Some(format!("{e:#}")),
        }
        news = true;
    }
    news
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
        assert_eq!(group.agents.len(), 1);
        assert!(!group.led, "with no bare SAVRAS, nobody leads");
        assert_eq!(
            group.describe(job(&s, "SAVRAS-3")),
            "started alongside SAVRAS-2 · no SAVRAS to lead them",
            "and the panel says so rather than naming a parent"
        );
    }

    #[test]
    fn deleting_the_lead_does_not_promote_one_of_its_agents() {
        // The bug the owner saw: delete `STAGING` and the panel drew
        // `STAGING-2` as the parent of the others. Nothing on disk says a
        // session has a parent — the indent is read out of the names — so the
        // agents outlive their lead as ordinary rows, side by side.
        let led = Fixture::new("agents-lead-there")
            .job("a", &named("STAGING"))
            .job("b", &named("STAGING-2"))
            .job("c", &named("STAGING-3"));
        assert_eq!(
            laid_out(&snap(&led)),
            ["STAGING", "└ STAGING-2", "└ STAGING-3"]
        );

        let orphaned = Fixture::new("agents-lead-gone")
            .job("b", &named("STAGING-2"))
            .job("c", &named("STAGING-3"));
        assert_eq!(
            laid_out(&snap(&orphaned)),
            ["STAGING-2", "STAGING-3"],
            "no indent, and no session promoted into the empty place"
        );
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
    fn a_numbered_lead_keeps_its_lead() {
        // `SAVRAS-4` leading `SAVRAS-5`, with no bare `SAVRAS` running: the
        // free `2` is tempting and wrong. An agent called `SAVRAS-2` would be
        // briefed to report to `SAVRAS-4` and then, being the lowest number,
        // be shown by the panel as the lead of the session that briefed it.
        let f = Fixture::new("agents-numbered-lead")
            .job("a", &named("SAVRAS-4"))
            .job("b", &named("SAVRAS-5"));
        let s = snap(&f);

        let lead = lead_of(&s, job(&s, "SAVRAS-5"));
        assert_eq!(lead.name, "SAVRAS-4");
        assert_eq!(next_name(&s, &lead.name), "SAVRAS-6");

        // And the group the new name would join still has the same lead.
        let f = f.job("c", &named("SAVRAS-6"));
        let s = snap(&f);
        assert_eq!(lead_of(&s, job(&s, "SAVRAS-6")).name, "SAVRAS-4");
    }

    #[test]
    fn a_number_is_reused_when_the_agent_that_had_it_is_gone() {
        // Reuse still holds above the lead: closing `AGENT-2` and starting
        // another gives `AGENT-2` back rather than climbing forever.
        let f = Fixture::new("agents-reuse")
            .job("a", &named("AGENT"))
            .job("b", &named("AGENT-3"));
        let s = snap(&f);
        assert_eq!(next_name(&s, "AGENT"), "AGENT-2");
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

    /// The names, in the order they come out, with agents marked.
    fn laid_out(s: &Snapshot) -> Vec<String> {
        let all: Vec<usize> = (0..s.jobs.len()).collect();
        under_leads(s, &all)
            .into_iter()
            .map(|(at, agent)| {
                format!(
                    "{}{}",
                    if agent { "└ " } else { "" },
                    s.jobs[at].name.clone()
                )
            })
            .collect()
    }

    #[test]
    fn agents_come_out_under_their_lead_in_number_order() {
        let f = Fixture::new("agents-under")
            .job("a", &named("BOOKS-4"))
            .job("b", &named("BOOKS"))
            .job("c", &named("BOOKS-2"));
        assert_eq!(
            laid_out(&snap(&f)),
            ["BOOKS", "└ BOOKS-2", "└ BOOKS-4"],
            "the lead first, then its agents by number"
        );
    }

    #[test]
    fn a_session_with_no_siblings_is_a_row_and_not_an_orphan() {
        // Two make a group; one is a session. This is also what stops a
        // hierarchy being invented out of a hyphen.
        let f = Fixture::new("agents-alone")
            .job("a", &named("PR-357"))
            .job("b", &named("BOOKS"));
        assert_eq!(laid_out(&snap(&f)), ["BOOKS", "PR-357"]);
    }

    #[test]
    fn a_name_that_merely_ends_in_a_word_is_not_an_agent() {
        // `BOOKS-LEG3` splits to itself, not to `BOOKS` — the tail has to be a
        // number. Otherwise every hyphenated name in a repository would be
        // filed under the first one alphabetically.
        let f = Fixture::new("agents-legs")
            .job("a", &named("BOOKS"))
            .job("b", &named("BOOKS-LEG3"))
            .job("c", &named("BOOKS-LEG4"));
        assert_eq!(laid_out(&snap(&f)), ["BOOKS", "BOOKS-LEG3", "BOOKS-LEG4"]);
    }

    #[test]
    fn a_family_is_placed_by_the_oldest_session_in_it() {
        // An agent asking a question does not move its family: where a family
        // sits is settled by when its work started, and the question is said
        // in the row itself. The agent stays under the lead it belongs to.
        let f = Fixture::new("agents-waiting")
            .job(
                "a",
                r#"{"state":"working","name":"ALPHA","cwd":"/tmp/repo",
                    "createdAt":"2026-09-22T08:00:00Z"}"#,
            )
            .job(
                "b",
                r#"{"state":"done","name":"BOOKS","cwd":"/tmp/repo",
                    "createdAt":"2026-09-22T09:00:00Z"}"#,
            )
            .job(
                "c",
                r#"{"state":"working","name":"BOOKS-2","cwd":"/tmp/repo",
                    "needs":"answer: which one?","createdAt":"2026-09-22T10:00:00Z"}"#,
            );
        assert_eq!(
            laid_out(&snap(&f)),
            ["ALPHA", "BOOKS", "└ BOOKS-2"],
            "the older session first, and the family stays together"
        );
    }

    #[test]
    fn the_order_holds_still_when_an_agent_changes_what_it_is_doing() {
        // The flip keys walk this list. Rows that trade places while you are
        // looking at them make those keys a lottery (M2.2), so nothing here
        // may be ordered by anything that changes minute to minute.
        let working = Fixture::new("agents-still-a")
            .job("a", &named("BOOKS"))
            .job("b", &named("BOOKS-2"))
            .job("c", &named("BOOKS-3"));
        let one_done = Fixture::new("agents-still-b")
            .job("a", &named("BOOKS"))
            .job(
                "b",
                r#"{"state":"done","name":"BOOKS-2","cwd":"/tmp/repo"}"#,
            )
            .job("c", &named("BOOKS-3"));
        assert_eq!(laid_out(&snap(&working)), laid_out(&snap(&one_done)));
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

    #[test]
    fn an_agent_is_refused_for_a_session_on_another_machine() {
        // `a` starts `claude --bg` on *this* machine, in the lead's own
        // directory. That directory is on the other box, so the agent would
        // come up somewhere arbitrary here, with none of the code it was
        // briefed about, and be told to report to a session it cannot reach.
        let f = Fixture::new("agent-remote");
        let mut app = App::new(f.0.clone());
        let job = Job {
            short: "claude-box:4242".to_string(),
            name: "BOX".to_string(),
            color: None,
            status: crate::job::Status::Working,
            summary: String::new(),
            cwd: std::path::PathBuf::from("/home/ubuntu/Code/thing"),
            session_id: "s".to_string(),
            tokens: 0,
            updated_at: None,
            links: Vec::new(),
            backend: None,
            daemon_short: None,
            machine: Some(crate::job::Remote {
                host: "claude-box".to_string(),
                tmux: Some("a:@1.%1".to_string()),
                pid: Some(4242),
            }),
            created_at: None,
            model: None,
            context: None,
            failed: false,
            deploy: None,
        };
        let short = job.short.clone();
        app.set_remote("claude-box".to_string(), vec![job]);
        app.select(&short);

        let (tx, rx) = mpsc::channel();
        add(&mut app, &tx);

        let said = app.error.clone().unwrap_or_default();
        assert!(said.contains("claude-box"), "say which machine: {said}");
        assert!(!said.contains("starting"), "nothing was started: {said}");
        // And nothing was handed to the errand thread to do.
        assert!(rx.try_recv().is_err(), "an errand was queued anyway");
    }
}
