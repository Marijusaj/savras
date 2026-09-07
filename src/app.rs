//! Panel state: what is on screen and what is selected.

use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::widgets::ListState;

use crate::job::{self, Job, Snapshot, Status};

/// A visual line. Headings are drawn but never selected.
#[derive(Debug, Clone, Copy)]
pub enum Row {
    Heading(Status),
    /// A repository heading, indexing [`App::groups`]. Not a `String`, so a
    /// row stays a `Copy` handle to somewhere rather than a piece of text.
    Repo(usize),
    Job(usize),
    /// A pane of your own — the shell Savras started with, and any you have
    /// added since. They sit at the head of the list because that is where
    /// flipping already put them, and a stop you cannot see is a stop you
    /// cannot use.
    Shell(usize),
    /// A blank line between groups. Drawn, never selected.
    Spacer,
}

/// What the panel groups its rows by.
///
/// Status is what the panel has always done and answers "who needs me".
/// Repository answers "what is happening in this codebase", which is the
/// question you ask when several are in play at once — and with a session in
/// every repository you own, the status groups interleave them all and neither
/// question is easy to read off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Status,
    Repo,
}

impl GroupBy {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "status" | "state" => Some(GroupBy::Status),
            "repo" | "repository" => Some(GroupBy::Repo),
            _ => None,
        }
    }

    /// The other one, for the key that flips between them.
    pub fn other(self) -> Self {
        match self {
            GroupBy::Status => GroupBy::Repo,
            GroupBy::Repo => GroupBy::Status,
        }
    }
}

/// What the working pane is showing. The panel is told, so it can mark the
/// row you are on and name it in the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Front {
    /// The nth pane of your own, counted in the order they were opened.
    Shell(usize),
    Session(String),
}

pub struct App {
    pub jobs_dir: PathBuf,
    pub snapshot: Snapshot,
    pub rows: Vec<Row>,
    pub list_state: ListState,
    pub error: Option<String>,
    /// False when the filesystem watcher could not start and we are falling
    /// back to polling — worth telling the user, since latency changes.
    pub watching: bool,
    /// Jobs that have pinged and that you have not been to yet, by short id.
    ///
    /// The status groups already say *who* needs you; a ping says *someone
    /// just started to*, and without this the panel cannot tell you which of
    /// four waiting sessions made the sound.
    alerted: HashSet<String>,
    /// Sessions with a tab open, and which of them is in front. The panel is
    /// the only place that says so: a session in a background tab is running
    /// and unattended, which is exactly the thing worth being able to see.
    tabs: Vec<String>,
    front: Option<Front>,
    /// How many panes of your own are open — one at the very least, since
    /// Savras always starts with a shell. Zero means there is no working pane
    /// at all, which is the standalone panel.
    shells: usize,
    /// What to call them: the program Savras hosts, so `svr` says "shell" and
    /// `svr -- claude` says "claude". A new tab runs the same thing.
    shell_label: String,
    /// How the rows are grouped, and the headings that grouping produced.
    group_by: GroupBy,
    pub groups: Vec<String>,
    /// What to call the keys that flip between sessions. Only ever shown once
    /// there is a session to flip to: a key that would do nothing is worse
    /// than no key at all, because you try it and conclude it is broken.
    switch_label: Option<String>,
    pub should_quit: bool,
}

/// How a session relates to the working pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// In the pane right now.
    Front,
    /// Open in a tab behind the one you are looking at.
    Behind,
    None,
}

/// Where the cursor was, in terms that outlive a rebuild of the row list.
enum Anchor {
    Shell(usize),
    Job(String),
}

/// Rows you can land on. Headings and blank lines are drawn, never selected.
fn selectable(row: &Row) -> bool {
    matches!(row, Row::Job(_) | Row::Shell(_))
}

impl App {
    pub fn new(jobs_dir: PathBuf) -> Self {
        let mut app = Self {
            jobs_dir,
            snapshot: Snapshot::default(),
            rows: Vec::new(),
            list_state: ListState::default(),
            error: None,
            watching: false,
            alerted: HashSet::new(),
            tabs: Vec::new(),
            front: None,
            shells: 0,
            shell_label: "shell".to_string(),
            group_by: GroupBy::Status,
            groups: Vec::new(),
            switch_label: None,
            should_quit: false,
        };
        app.refresh();
        app
    }

    /// Mark the jobs a ping just fired for. They stay marked until you go to
    /// them — the sound is over in a second, and you may not be at the screen
    /// when it happens.
    pub fn alert(&mut self, shorts: impl IntoIterator<Item = String>) {
        self.alerted.extend(shorts);
    }

    /// Whether this job is still waiting for you to notice it.
    pub fn alerted(&self, job: &Job) -> bool {
        self.alerted.contains(&job.short)
    }

    /// Tell the panel which sessions are open in tabs, and which is in front.
    /// The standalone panel has no working pane, so it never calls this and
    /// every session stays [`Tab::None`].
    pub fn set_tabs(&mut self, front: Front, open: Vec<String>, shells: usize) {
        let was = self.shells;
        self.front = Some(front);
        self.tabs = open;
        self.shells = shells;
        if was != shells {
            // The rows changed shape, and the cursor has to survive it.
            let anchor = self.anchor();
            self.rebuild_rows();
            self.restore_selection(anchor);
        }
    }

    /// Name the panes of your own, after the program Savras hosts.
    pub fn set_shell_label(&mut self, label: String) {
        self.shell_label = label;
    }

    /// What the nth pane of your own is called. The first is unnumbered: with
    /// one shell there is no number to tell it from, and with several the
    /// numbers match the order you opened them.
    pub fn shell_name(&self, i: usize) -> String {
        if i == 0 {
            self.shell_label.clone()
        } else {
            format!("{} {}", self.shell_label, i + 1)
        }
    }

    pub fn shells(&self) -> usize {
        self.shells
    }

    /// Name the key that flips tabs, for the footer to offer.
    pub fn set_switch(&mut self, label: Option<String>) {
        self.switch_label = label;
    }

    /// The keys to advertise for flipping sessions — only once there is a
    /// session to flip to.
    pub fn switch_hint(&self) -> Option<&str> {
        if self.snapshot.is_empty() && self.shells < 2 {
            return None;
        }
        self.switch_label.as_deref()
    }

    /// The name of the session in the working pane, for the header to carry.
    ///
    /// Claude Code does not always draw its own name where you can see it, and
    /// a pane full of somebody else's output looks much like any other. The
    /// panel is the one place that always knows, so it is the one place that
    /// should always say.
    pub fn front_name(&self) -> Option<String> {
        match self.front.as_ref()? {
            Front::Session(short) => self
                .snapshot
                .jobs
                .iter()
                .find(|j| &j.short == short)
                .map(|j| j.name.clone()),
            // One shell needs no naming — an unnamed pane *is* the shell, and
            // that has been true since before there were tabs. Several do.
            Front::Shell(_) if self.shells < 2 => None,
            Front::Shell(i) => Some(self.shell_name(*i)),
        }
    }

    /// Whether the nth pane of your own is the one in the working pane.
    pub fn shell_tab(&self, i: usize) -> Tab {
        match self.front {
            Some(Front::Shell(front)) if front == i => Tab::Front,
            _ => Tab::Behind,
        }
    }

    pub fn tab(&self, job: &Job) -> Tab {
        if self.front == Some(Front::Session(job.short.clone())) {
            Tab::Front
        } else if self.tabs.iter().any(|short| short == &job.short) {
            Tab::Behind
        } else {
            Tab::None
        }
    }

    /// How many sessions are open behind the one you are looking at.
    pub fn behind_count(&self) -> usize {
        (self.shells + self.tabs.len()).saturating_sub(1)
    }

    pub fn alert_count(&self) -> usize {
        self.alerted.len()
    }

    /// You have been to it: opening it, or moving the cursor onto it, is
    /// enough to say you have seen which one it was.
    fn attend(&mut self) {
        if let Some(short) = self.selected_job().map(|j| j.short.clone()) {
            self.alerted.remove(&short);
        }
    }

    /// Put the cursor on a session, so that flipping to it in the pane moves
    /// the panel's highlight with you — the panel is meant to say where you
    /// are, and a cursor left three rows behind says the opposite.
    pub fn select(&mut self, short: &str) {
        let row = self.rows.iter().position(|r| match r {
            Row::Job(i) => self.snapshot.jobs[*i].short == short,
            _ => false,
        });
        if let Some(row) = row {
            self.list_state.select(Some(row));
        }
    }

    pub fn attend_to(&mut self, short: &str) {
        self.alerted.remove(short);
    }

    /// Re-read from disk, keeping the cursor on the same row where possible.
    pub fn refresh(&mut self) {
        let anchor = self.anchor();

        match job::load(&self.jobs_dir) {
            Ok(snapshot) => {
                self.snapshot = snapshot;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e}")),
        }

        self.rebuild_rows();
        self.restore_selection(anchor);
        self.forget_stale_alerts();
    }

    /// A mark is about a session waiting on you. Once it is gone, or back to
    /// working — you answered it in its own tab, say — there is nothing left
    /// to point at.
    fn forget_stale_alerts(&mut self) {
        let jobs = &self.snapshot.jobs;
        self.alerted.retain(|short| {
            jobs.iter()
                .any(|j| &j.short == short && j.status != Status::Working)
        });
    }

    /// What the cursor is on now, in terms that survive the list being rebuilt.
    fn anchor(&self) -> Option<Anchor> {
        match self.current_row() {
            Some(Row::Job(i)) => self
                .snapshot
                .jobs
                .get(i)
                .map(|j| Anchor::Job(j.short.clone())),
            Some(Row::Shell(i)) => Some(Anchor::Shell(i)),
            _ => None,
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        // Your own panes first, unheaded: one shell needs no group and several
        // read as a list on their own.
        for i in 0..self.shells {
            self.rows.push(Row::Shell(i));
        }
        match self.group_by {
            GroupBy::Status => self.rows_by_status(),
            GroupBy::Repo => self.rows_by_repo(),
        }
    }

    fn rows_by_status(&mut self) {
        self.groups.clear();
        for status in [Status::NeedsInput, Status::Working, Status::Done] {
            let group: Vec<usize> = self
                .snapshot
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| j.status == status)
                .map(|(i, _)| i)
                .collect();
            if group.is_empty() {
                continue;
            }
            if !self.rows.is_empty() {
                self.rows.push(Row::Spacer);
            }
            self.rows.push(Row::Heading(status));
            self.rows.extend(group.into_iter().map(Row::Job));
        }
    }

    /// One group per repository, the sessions inside it ordered exactly as the
    /// status grouping orders them: waiting first, then working, then done,
    /// and by name within each.
    ///
    /// **A repository with a session waiting on you sorts to the top.** The
    /// panel's job is to surface what is waiting, and grouping must not bury
    /// it — a repository is a place, and a question does not stop being a
    /// question because of where it was asked. Repositories with nothing but
    /// finished sessions sink, and ties are broken by name, so the list only
    /// moves when a session changes status.
    fn rows_by_repo(&mut self) {
        self.groups = self.repos_in_order();
        // Indices are into the snapshot, which is already sorted by status and
        // then name — so filtering preserves that order and nothing else has
        // to sort anything.
        for (g, repo) in self.groups.clone().into_iter().enumerate() {
            let group: Vec<usize> = self
                .snapshot
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| j.repo() == repo)
                .map(|(i, _)| i)
                .collect();
            if group.is_empty() {
                continue;
            }
            if !self.rows.is_empty() {
                self.rows.push(Row::Spacer);
            }
            self.rows.push(Row::Repo(g));
            self.rows.extend(group.into_iter().map(Row::Job));
        }
    }

    /// The repositories in play, most-waiting first and then by name.
    fn repos_in_order(&self) -> Vec<String> {
        let mut repos: Vec<(Status, String)> = Vec::new();
        for job in &self.snapshot.jobs {
            let repo = job.repo();
            match repos.iter_mut().find(|(_, name)| name == &repo) {
                // The most demanding status in the repository is what it sorts
                // by: one session asking is enough to bring its repository up.
                Some((best, _)) if job.status < *best => *best = job.status,
                Some(_) => {}
                None => repos.push((job.status, repo)),
            }
        }
        repos.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        repos.into_iter().map(|(_, name)| name).collect()
    }

    /// Group by repository or by status, and say which it is now.
    pub fn regroup(&mut self) -> GroupBy {
        self.group_by = self.group_by.other();
        let anchor = self.anchor();
        self.rebuild_rows();
        self.restore_selection(anchor);
        self.group_by
    }

    pub fn set_group_by(&mut self, how: GroupBy) {
        self.group_by = how;
        let anchor = self.anchor();
        self.rebuild_rows();
        self.restore_selection(anchor);
    }

    /// Put the cursor back on the job it was on. If that job is gone, fall back
    /// to the same position, then to the first job.
    fn restore_selection(&mut self, anchor: Option<Anchor>) {
        let previous = self.list_state.selected();

        let target = anchor
            .and_then(|anchor| {
                self.rows.iter().position(|r| match (r, &anchor) {
                    (Row::Job(i), Anchor::Job(short)) => &self.snapshot.jobs[*i].short == short,
                    (Row::Shell(i), Anchor::Shell(j)) => i == j,
                    _ => false,
                })
            })
            .or_else(|| previous.filter(|i| *i < self.rows.len()))
            .or_else(|| self.first_selectable_row());

        self.list_state.select(target);
        // The fallback may have landed on a heading or a blank line.
        if !self.on_selectable() {
            self.step(1);
        }
    }

    fn first_selectable_row(&self) -> Option<usize> {
        self.rows.iter().position(selectable)
    }

    fn on_selectable(&self) -> bool {
        self.current_row().as_ref().is_some_and(selectable)
    }

    fn current_row(&self) -> Option<Row> {
        self.list_state
            .selected()
            .and_then(|i| self.rows.get(i))
            .copied()
    }

    pub fn selected_job(&self) -> Option<&Job> {
        match self.current_row() {
            Some(Row::Job(i)) => self.snapshot.jobs.get(i),
            _ => None,
        }
    }

    /// The session at the top of the list — Needs input before Working before
    /// Completed, so it is the one most likely to be why you opened Savras.
    pub fn first_job(&self) -> Option<&Job> {
        self.rows.iter().find_map(|r| match r {
            Row::Job(i) => self.snapshot.jobs.get(*i),
            _ => None,
        })
    }

    /// A session by name, as the panel spells it. Case-insensitive, because
    /// the names are shouted and nobody wants to hold shift to say so.
    pub fn job_named(&self, name: &str) -> Option<&Job> {
        self.snapshot
            .jobs
            .iter()
            .find(|j| j.name.eq_ignore_ascii_case(name))
    }

    /// Which of your own panes the cursor is on, if it is on one at all.
    pub fn selected_shell(&self) -> Option<usize> {
        match self.current_row() {
            Some(Row::Shell(i)) => Some(i),
            _ => None,
        }
    }

    /// Put the cursor on one of your own panes, so flipping to it moves the
    /// highlight with you, exactly as flipping to a session does.
    pub fn select_shell(&mut self, i: usize) {
        if let Some(row) = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Shell(j) if *j == i))
        {
            self.list_state.select(Some(row));
        }
    }

    /// Move the cursor by `delta` job rows, skipping headings and stopping at
    /// the ends rather than wrapping.
    pub fn step(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let start = self.list_state.selected().unwrap_or(0) as isize;
        let mut i = start;
        loop {
            i += delta;
            if i < 0 || i as usize >= self.rows.len() {
                return; // no job that way; leave the cursor where it was
            }
            if selectable(&self.rows[i as usize]) {
                self.list_state.select(Some(i as usize));
                self.attend();
                return;
            }
        }
    }

    pub fn jump(&mut self, to_end: bool) {
        if self.rows.is_empty() {
            return;
        }
        if to_end {
            self.list_state.select(Some(self.rows.len() - 1));
            if !self.on_selectable() {
                self.step(-1);
            }
        } else {
            self.list_state.select(self.first_selectable_row());
        }
        self.attend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    fn fixture() -> Fixture {
        Fixture::new("app")
            .job(
                "aaa",
                r#"{"state":"working","name":"ASK","needs":"answer: ?"}"#,
            )
            .job("bbb", r#"{"state":"working","name":"RUN","detail":"d"}"#)
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"ok"}}"#,
            )
    }

    fn names_in_order(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                Row::Job(i) => Some(app.snapshot.jobs[*i].name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rows_interleave_headings_with_their_group() {
        let f = fixture();
        let app = App::new(f.0.clone());
        // 3 headings + 3 jobs + a blank line between groups.
        assert_eq!(app.rows.len(), 8);
        assert!(matches!(app.rows[0], Row::Heading(Status::NeedsInput)));
        assert!(matches!(app.rows[2], Row::Spacer));
        assert!(matches!(app.rows[3], Row::Heading(Status::Working)));
        assert!(matches!(app.rows[5], Row::Spacer));
        assert!(matches!(app.rows[6], Row::Heading(Status::Done)));
        assert_eq!(names_in_order(&app), ["ASK", "RUN", "FIN"]);
    }

    #[test]
    fn selection_starts_on_the_job_that_needs_you() {
        let f = fixture();
        let app = App::new(f.0.clone());
        assert_eq!(app.selected_job().unwrap().name, "ASK");
    }

    #[test]
    fn moving_skips_headings() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.step(1);
        assert_eq!(app.selected_job().unwrap().name, "RUN");
        app.step(1);
        assert_eq!(app.selected_job().unwrap().name, "FIN");
        app.step(-1);
        assert_eq!(app.selected_job().unwrap().name, "RUN");
    }

    #[test]
    fn moving_stops_at_the_ends_rather_than_wrapping() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        for _ in 0..10 {
            app.step(-1);
        }
        assert_eq!(app.selected_job().unwrap().name, "ASK");
        for _ in 0..10 {
            app.step(1);
        }
        assert_eq!(app.selected_job().unwrap().name, "FIN");
    }

    #[test]
    fn jump_lands_on_jobs_not_headings() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.jump(true);
        assert_eq!(app.selected_job().unwrap().name, "FIN");
        app.jump(false);
        assert_eq!(app.selected_job().unwrap().name, "ASK");
    }

    #[test]
    fn the_cursor_follows_its_job_when_the_list_reorders() {
        // The panel refreshes constantly; a job answering its question moves
        // from the top group to another. The cursor must go with it, not sit
        // on whatever row happens to take its place.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.step(1);
        assert_eq!(app.selected_job().unwrap().name, "RUN");

        std::fs::write(
            f.0.join("bbb").join("state.json"),
            r#"{"state":"done","name":"RUN","output":{"result":"finished"}}"#,
        )
        .unwrap();
        app.refresh();

        assert_eq!(app.selected_job().unwrap().name, "RUN");
        assert_eq!(app.selected_job().unwrap().status, Status::Done);
    }

    #[test]
    fn the_cursor_survives_its_job_disappearing() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.jump(true);
        assert_eq!(app.selected_job().unwrap().name, "FIN");

        std::fs::remove_dir_all(f.0.join("ccc")).unwrap();
        app.refresh();

        assert!(app.selected_job().is_some(), "cursor must land on a job");
        assert!(app.error.is_none());
    }

    #[test]
    fn a_ping_marks_its_session_until_you_go_to_it() {
        // The sound says someone wants you; the mark says which one, and has
        // to survive you being in another application when it happened.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.alert(["aaa".to_string()]);

        let asking = app.snapshot.jobs.iter().find(|j| j.name == "ASK").unwrap();
        assert!(app.alerted(asking));
        assert_eq!(app.alert_count(), 1);

        // A refresh must not lose it: the panel re-reads every couple of
        // seconds, and you may not be back yet.
        app.refresh();
        assert_eq!(app.alert_count(), 1);
    }

    #[test]
    fn moving_the_cursor_onto_a_marked_session_is_seeing_it() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.alert(["bbb".to_string()]);
        assert_eq!(app.alert_count(), 1);

        app.step(1); // onto RUN, which is job bbb
        assert_eq!(app.selected_job().unwrap().name, "RUN");
        assert_eq!(app.alert_count(), 0, "you looked straight at it");
    }

    #[test]
    fn a_session_that_stops_asking_stops_being_marked() {
        // You answered it in its own tab. There is nothing left to point at,
        // and a mark that outlives its reason teaches you to ignore marks.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.alert(["aaa".to_string()]);

        std::fs::write(
            f.0.join("aaa").join("state.json"),
            r#"{"state":"working","name":"ASK","detail":"back to work"}"#,
        )
        .unwrap();
        app.refresh();
        assert_eq!(app.alert_count(), 0);
    }

    #[test]
    fn a_marked_session_that_disappears_is_forgotten() {
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.alert(["ccc".to_string()]);
        assert_eq!(app.alert_count(), 1);

        std::fs::remove_dir_all(f.0.join("ccc")).unwrap();
        app.refresh();
        assert_eq!(app.alert_count(), 0);
    }

    #[test]
    fn your_own_panes_head_the_list_and_take_the_cursor() {
        // They are flip stops, so they have to be rows: a stop you cannot see
        // is one you walk past without knowing where you went.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.set_tabs(Front::Shell(1), Vec::new(), 2);

        assert!(matches!(app.rows[0], Row::Shell(0)));
        assert!(matches!(app.rows[1], Row::Shell(1)));
        assert_eq!(app.shell_name(0), "shell");
        assert_eq!(app.shell_name(1), "shell 2");
        assert_eq!(app.shell_tab(1), Tab::Front);
        assert_eq!(app.shell_tab(0), Tab::Behind);

        // The cursor lands on them and comes back off.
        app.jump(false);
        assert_eq!(app.selected_shell(), Some(0));
        assert!(app.selected_job().is_none());
        app.step(1);
        assert_eq!(app.selected_shell(), Some(1));
        app.step(1);
        assert!(app.selected_job().is_some(), "on to the sessions");
    }

    #[test]
    fn the_cursor_stays_on_the_pane_it_was_on_across_a_refresh() {
        // Rows are rebuilt every couple of seconds; a cursor that slid off
        // your own pane onto a session would make the keys unusable.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.set_tabs(Front::Shell(0), Vec::new(), 2);
        app.select_shell(1);
        assert_eq!(app.selected_shell(), Some(1));
        app.refresh();
        assert_eq!(app.selected_shell(), Some(1));
    }

    /// Sessions in three directories, so grouping has something to group.
    /// The `cwd`s are outside any repository, so each is its own name.
    fn across_repos() -> Fixture {
        Fixture::new("app-repos")
            .job(
                "aaa",
                r#"{"state":"working","name":"RUN","cwd":"/tmp/savras-test-repos/beta"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"ASK","needs":"answer: ?","cwd":"/tmp/savras-test-repos/gamma"}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"ok"},"cwd":"/tmp/savras-test-repos/beta"}"#,
            )
            .job(
                "ddd",
                r#"{"state":"done","name":"OLD","output":{"result":"ok"},"cwd":"/tmp/savras-test-repos/alpha"}"#,
            )
    }

    #[test]
    fn grouping_by_repository_puts_the_one_that_needs_you_first() {
        // A repository is a place, and a question does not stop being a
        // question because of where it was asked — so grouping must not bury
        // it. `alpha` sorts last despite its name: nothing there is waiting.
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        app.set_group_by(GroupBy::Repo);

        assert_eq!(app.groups, ["gamma", "beta", "alpha"]);
        // Inside a repository, the panel's own order holds: waiting, then
        // working, then done, and by name within each.
        assert_eq!(
            names_in_order(&app),
            ["ASK", "RUN", "FIN", "OLD"],
            "sessions are ordered within their repository, not shuffled"
        );
    }

    #[test]
    fn regrouping_keeps_the_cursor_on_the_session_it_was_on() {
        // The list is rebuilt underneath you; landing somewhere else would
        // make the key useless for comparing the two views.
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        app.select("ccc");
        assert_eq!(app.selected_job().map(|j| j.short.as_str()), Some("ccc"));

        assert_eq!(app.regroup(), GroupBy::Repo);
        assert_eq!(app.selected_job().map(|j| j.short.as_str()), Some("ccc"));
        assert_eq!(app.regroup(), GroupBy::Status);
        assert_eq!(app.selected_job().map(|j| j.short.as_str()), Some("ccc"));
    }

    #[test]
    fn a_repository_heading_is_drawn_but_never_landed_on() {
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        app.set_group_by(GroupBy::Repo);
        app.jump(false);
        for _ in 0..10 {
            assert!(
                app.selected_job().is_some(),
                "the cursor stopped on a heading"
            );
            app.step(1);
        }
    }

    #[test]
    fn the_top_of_the_list_is_the_session_that_wants_you_most() {
        // `--open top` is only worth having if "top" means what the panel
        // shows: Needs input first, whatever the ages say.
        let f = fixture();
        let app = App::new(f.0.clone());
        assert_eq!(app.first_job().map(|j| j.name.as_str()), Some("ASK"));
        assert_eq!(app.job_named("fin").map(|j| j.short.as_str()), Some("ccc"));
        assert!(app.job_named("nobody").is_none());
    }

    #[test]
    fn one_shell_needs_no_naming_but_several_do() {
        // An unnamed pane has meant "the shell" since before there were tabs.
        let f = fixture();
        let mut app = App::new(f.0.clone());
        app.set_tabs(Front::Shell(0), Vec::new(), 1);
        assert_eq!(app.front_name(), None);
        app.set_tabs(Front::Shell(1), Vec::new(), 2);
        assert_eq!(app.front_name().as_deref(), Some("shell 2"));
    }

    #[test]
    fn an_empty_panel_has_no_selection_and_does_not_panic() {
        let f = Fixture::new("app-empty");
        let mut app = App::new(f.0.clone());
        assert!(app.selected_job().is_none());
        app.step(1);
        app.step(-1);
        app.jump(true);
        assert!(app.selected_job().is_none());
    }
}
