//! Panel state: what is on screen and what is selected.

use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::widgets::ListState;

use crate::job::{self, Job, Snapshot, Status};

/// A visual line. Headings are drawn but never selected.
#[derive(Debug, Clone, Copy)]
pub enum Row {
    Heading(Status),
    Job(usize),
    /// A blank line between groups. Drawn, never selected.
    Spacer,
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
    pub should_quit: bool,
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

    pub fn attend_to(&mut self, short: &str) {
        self.alerted.remove(short);
    }

    /// Re-read from disk, keeping the cursor on the same job where possible.
    pub fn refresh(&mut self) {
        let anchor = self.selected_job().map(|j| j.short.clone());

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

    fn rebuild_rows(&mut self) {
        self.rows.clear();
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

    /// Put the cursor back on the job it was on. If that job is gone, fall back
    /// to the same position, then to the first job.
    fn restore_selection(&mut self, anchor: Option<String>) {
        let previous = self.list_state.selected();

        let target = anchor
            .and_then(|short| {
                self.rows.iter().position(|r| match r {
                    Row::Job(i) => self.snapshot.jobs[*i].short == short,
                    _ => false,
                })
            })
            .or_else(|| previous.filter(|i| *i < self.rows.len()))
            .or_else(|| self.first_job_row());

        self.list_state.select(target);
        // The fallback may have landed on a heading or a blank line.
        if !matches!(self.current_row(), Some(Row::Job(_))) {
            self.step(1);
        }
    }

    fn first_job_row(&self) -> Option<usize> {
        self.rows.iter().position(|r| matches!(r, Row::Job(_)))
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
            if matches!(self.rows[i as usize], Row::Job(_)) {
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
            if !matches!(self.current_row(), Some(Row::Job(_))) {
                self.step(-1);
            }
        } else {
            self.list_state.select(self.first_job_row());
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
