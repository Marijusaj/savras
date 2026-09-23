//! Panel state: what is on screen and what is selected.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use ratatui::widgets::ListState;

use crate::board::{self, Boards, Message, Owner};
use crate::codex;
use crate::job::{self, Job, Snapshot, Status};

/// How many messages the board screen holds. More than a hook injects, because
/// a person scrolling back is not a context window filling up.
const BOARD_WINDOW: usize = 200;

/// A visual line. Headings are drawn but never selected.
#[derive(Debug, Clone, Copy)]
pub enum Row {
    Heading(Status),
    /// A repository heading, indexing [`App::groups`]. Not a `String`, so a
    /// row stays a `Copy` handle to somewhere rather than a piece of text.
    Repo(usize),
    Job(usize),
    /// A pane of your own — the shell Savras started with, and any you have
    /// added since. They sit at the head of the list, or grouped by repository
    /// at the head of the repository they are standing in — flipping walks the
    /// rows, so it meets them wherever they are drawn.
    Shell(usize),
    /// A repository's board, first under its heading, indexing
    /// [`App::board_rows`]. Only when the repository has one, and only when
    /// grouped by repository: a status group has no repository to put it under.
    Board(usize),
    /// A blank line between groups. Drawn, never selected.
    Spacer,
}

/// A pane of your own, as the panel needs to draw it.
///
/// The name is what the pane is *running* rather than what Savras started
/// there — a shell you have ssh'd out of is not a shell any more, and a row
/// that still says so is a row that lies to you. The detail is the title the
/// program set, which for a login shell is usually the host and directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shell {
    pub name: String,
    pub detail: String,
    /// Where the program in the pane is standing, on this machine, once that
    /// has been asked. It is what puts the pane under a repository's heading;
    /// a pane on another machine, or one not asked yet, has none and sits at
    /// the top.
    pub cwd: Option<PathBuf>,
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
    /// A repository's board, by the repository's path.
    Board(String),
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
    /// The panes of your own, in the order they were opened. Empty means there
    /// is no working pane at all, which is the standalone panel.
    shells: Vec<Shell>,
    /// The local jobs, as last read. Kept apart from `snapshot` because the
    /// snapshot is the two sources *merged*, and a refresh of one must not
    /// drop the other.
    local: Vec<Job>,
    /// The last batch each machine sent, by ssh host.
    remote: std::collections::BTreeMap<String, Vec<Job>>,
    /// How the rows are grouped, and the headings that grouping produced.
    group_by: GroupBy,
    pub groups: Vec<String>,
    /// Which rows are agents drawn under their lead, by index into the
    /// snapshot. Worked out while the rows are built, because that is where
    /// the order is decided, and read back by the renderer for the indent.
    nested: HashSet<usize>,
    /// What to call the keys that flip between sessions. Only ever shown once
    /// there is a session to flip to: a key that would do nothing is worse
    /// than no key at all, because you try it and conclude it is broken.
    switch_label: Option<String>,
    /// The board, while it is on screen in place of the rows.
    pub board: Option<BoardView>,
    /// Where the boards are kept. A field of its own so a test can point it at
    /// boards of its own rather than the ones the machine's agents are reading.
    pub board_dir: PathBuf,
    /// Codex's own directory, read for the sessions it has running. A field
    /// for the same reason as the two above: a test must never read — or be
    /// at the mercy of — the Codex sessions on the machine running it.
    pub codex_dir: PathBuf,
    /// The boards drawn as rows, in the order they appear — rebuilt with the
    /// rows, since whether a repository has a board is asked of the disk then.
    pub board_rows: Vec<BoardRow>,
    /// Which boards are open as tabs in the working pane, by repository.
    open_boards: Vec<String>,
    /// Which repository a directory belongs to, worked out once per directory.
    /// See [`App::board_for`].
    topics: HashMap<PathBuf, String>,
    pub should_quit: bool,
}

/// A repository's board, as the panel shows it.
///
/// Reading it here moves nobody's cursor. Unread is what the hook uses to
/// decide what goes into an agent's turn, and the owner glancing at the panel
/// must not be what decides that an agent has already been told.
pub struct BoardView {
    dir: PathBuf,
    /// The repository of the session the board was opened on. `None` when no
    /// session of this machine's was under the cursor — a pane of your own, or
    /// a session on another machine, whose path means nothing here.
    pub repo: Option<String>,
    /// Whether that repository has a board. Making one is the owner's call, so
    /// the screen for a repository without one says so and offers to.
    pub exists: bool,
    /// Newest first, the way the screen shows them: what arrives lands at the
    /// top, next to the line you write in.
    pub messages: Vec<Message>,
    /// The message the cursor is on, by index into `messages`.
    pub selected: Option<usize>,
    /// A message being written, when one is.
    pub compose: Option<Compose>,
}

/// A message being written from the panel.
pub struct Compose {
    pub text: String,
    /// What it answers, when it answers something.
    pub re: Option<Message>,
}

impl BoardView {
    pub fn new(dir: PathBuf, repo: Option<String>) -> Self {
        let mut view = Self {
            dir,
            repo,
            exists: false,
            messages: Vec::new(),
            selected: None,
            compose: None,
        };
        view.reload();
        view
    }

    fn boards(&self) -> Boards {
        Boards::at(self.dir.clone())
    }

    /// Read the board again. A message you picked stays picked wherever the
    /// list moved; nothing picked stays nothing — the top, where what
    /// arrives is seen and where a new message is written.
    pub fn reload(&mut self) {
        let (exists, mut messages) = match &self.repo {
            Some(repo) => {
                let boards = self.boards();
                (boards.exists(repo), boards.read(repo, BOARD_WINDOW))
            }
            None => (false, Vec::new()),
        };
        messages.reverse();
        self.selected = self
            .selected
            .and_then(|i| self.messages.get(i))
            .and_then(|picked| messages.iter().position(|m| m.id == picked.id));
        self.messages = messages;
        self.exists = exists;
    }

    /// Which repository's board this is, as a name.
    pub fn name(&self) -> String {
        self.repo
            .as_deref()
            .map_or_else(|| "no repository".to_string(), board::topic_name)
    }

    /// Move the cursor one message up (`delta < 0`) or down. Down from nothing
    /// picks the newest; up past the newest lets go again, which is back to
    /// writing a new message rather than an answer. It stops at the oldest.
    pub fn step(&mut self, delta: isize) {
        let last = self.messages.len().checked_sub(1);
        self.selected = match (self.selected, delta < 0) {
            (None, true) => None,
            (None, false) => last.map(|_| 0),
            (Some(0), true) => None,
            (Some(at), true) => Some(at - 1),
            (Some(at), false) => Some(at.saturating_add(1).min(last.unwrap_or(0))),
        };
    }

    /// Back to the top with nothing picked, or the oldest message.
    pub fn jump(&mut self, to_end: bool) {
        self.selected = if to_end {
            self.messages.len().checked_sub(1)
        } else {
            None
        };
    }

    /// Start writing: a new message, or an answer — to the message picked, or
    /// the newest when none is. Where there is no board there is nowhere for it
    /// to go, so nothing starts.
    pub fn compose(&mut self, reply: bool) {
        if !self.exists {
            return;
        }
        let picked = self
            .selected
            .and_then(|i| self.messages.get(i))
            .or(self.messages.first());
        let re = match (reply, picked) {
            (false, _) => None,
            (true, Some(m)) => Some(m.clone()),
            // Nothing on the board to answer.
            (true, None) => return,
        };
        self.compose = Some(Compose {
            text: String::new(),
            re,
        });
    }

    /// Characters typed or pasted. A line break becomes a space — a message is
    /// read as one line wherever it is shown — and anything else unprintable
    /// is dropped.
    ///
    /// Writing starts with the first letter, where nothing has started it: an
    /// answer to the message picked, or a new message when none is. That is the
    /// board tab, where typing is the only way to write.
    pub fn type_text(&mut self, text: &str) {
        if self.compose.is_none() {
            if !self.exists {
                return;
            }
            self.compose = Some(Compose {
                text: String::new(),
                re: self.selected.and_then(|i| self.messages.get(i)).cloned(),
            });
        }
        let Some(compose) = self.compose.as_mut() else {
            return;
        };
        for c in text.chars() {
            if matches!(c, '\r' | '\n' | '\t') {
                compose.text.push(' ');
            } else if !c.is_control() {
                compose.text.push(c);
            }
        }
    }

    pub fn backspace(&mut self) {
        if let Some(compose) = self.compose.as_mut() {
            compose.text.pop();
        }
    }

    pub fn cancel(&mut self) {
        self.compose = None;
    }

    /// Post what has been written, as the owner. On failure the words are kept:
    /// a message you typed and lost to an error — an empty one, or a board
    /// deleted while you wrote — is one you type twice.
    pub fn send(&mut self) -> Result<Message> {
        let Some(compose) = self.compose.take() else {
            anyhow::bail!("nothing is being written");
        };
        let Some(repo) = self.repo.clone() else {
            self.compose = Some(compose);
            anyhow::bail!("there is no repository here to post to");
        };
        let re = compose.re.as_ref().map(|m| m.id.clone());
        match self.boards().post(board::OWNER, &repo, re, &compose.text) {
            Ok(message) => {
                // Onto what was just said, wherever the cursor had wandered.
                self.selected = None;
                self.reload();
                Ok(message)
            }
            Err(e) => {
                self.compose = Some(compose);
                Err(e)
            }
        }
    }

    /// Give this repository a board, as the owner — which the panel is. Returns
    /// the repository's name when one was made, and `None` when there was no
    /// repository to make it for or it already had one.
    pub fn create(&mut self) -> Result<Option<String>> {
        let Some(repo) = self.repo.clone() else {
            return Ok(None);
        };
        let made = self.boards().create(&Owner::at_the_panel(), &repo)?;
        self.reload();
        Ok(made.then(|| board::topic_name(&repo)))
    }
}

/// A repository's board, as a row under the repository's heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRow {
    /// The repository, by path — what the board is kept under.
    pub repo: String,
    /// The repository, as it is shown.
    pub name: String,
    /// How many messages the board holds, as of the last refresh.
    pub count: usize,
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
    Board(String),
}

/// Rows you can land on. Headings and blank lines are drawn, never selected.
fn selectable(row: &Row) -> bool {
    matches!(row, Row::Job(_) | Row::Shell(_) | Row::Board(_))
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
            shells: Vec::new(),
            local: Vec::new(),
            remote: std::collections::BTreeMap::new(),
            group_by: GroupBy::Status,
            groups: Vec::new(),
            nested: HashSet::new(),
            switch_label: None,
            board: None,
            // The path only: opening the boards migrates the old log, and that
            // is for a panel starting up, not for every `App` a test builds.
            board_dir: board::default_dir().unwrap_or_default(),
            // Under test this is nowhere on purpose: a suite that read the
            // machine's real Codex sessions would pass or fail depending on
            // what the developer happened to have open.
            codex_dir: if cfg!(test) {
                PathBuf::new()
            } else {
                codex::default_dir().unwrap_or_default()
            },
            board_rows: Vec::new(),
            open_boards: Vec::new(),
            topics: HashMap::new(),
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
    pub fn set_tabs(&mut self, front: Front, open: Vec<String>, shells: Vec<Shell>) {
        self.front = Some(front);
        self.tabs = open;
        // A pane that moved to another directory may have moved repository,
        // so where each one stands is part of the shape — not only how many.
        let changed = self.shells.len() != shells.len()
            || self
                .shells
                .iter()
                .zip(&shells)
                .any(|(was, is)| was.cwd != is.cwd);
        self.shells = shells;
        if changed {
            // The rows changed shape, and the cursor has to survive it.
            let anchor = self.anchor();
            self.rebuild_rows();
            self.restore_selection(anchor);
        }
    }

    /// Tell the panel which boards are open as tabs. Kept apart from
    /// [`App::set_tabs`] because boards are not terminal tabs, and nothing
    /// about a shell or a session needs to know they exist.
    pub fn set_open_boards(&mut self, open: Vec<String>) {
        self.open_boards = open;
    }

    /// What the nth pane of your own is called.
    pub fn shell_name(&self, i: usize) -> String {
        match self.shells.get(i) {
            Some(shell) => shell.name.clone(),
            None => "shell".to_string(),
        }
    }

    /// What that pane is doing, when it has said so. The empty string when it
    /// has not: most programs never set a title, and an invented one would be
    /// worse than a blank.
    pub fn shell_detail(&self, i: usize) -> &str {
        self.shells.get(i).map_or("", |shell| shell.detail.as_str())
    }

    pub fn shells(&self) -> usize {
        self.shells.len()
    }

    /// Name the key that flips tabs, for the footer to offer.
    pub fn set_switch(&mut self, label: Option<String>) {
        self.switch_label = label;
    }

    /// The keys to advertise for flipping sessions — only once there is a
    /// session to flip to.
    pub fn switch_hint(&self) -> Option<&str> {
        if self.snapshot.is_empty() && self.shells.len() < 2 {
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
            Front::Shell(_) if self.shells.len() < 2 => None,
            Front::Shell(i) => Some(self.shell_name(*i)),
            Front::Board(repo) => Some(format!("board · {}", board::topic_name(repo))),
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

    /// Whether a repository's board is open as a tab, and whether it is the one
    /// in front — marked the way a session's tab is, because it is one.
    pub fn board_tab(&self, repo: &str) -> Tab {
        match &self.front {
            Some(Front::Board(front)) if front == repo => Tab::Front,
            _ if self.open_boards.iter().any(|open| open == repo) => Tab::Behind,
            _ => Tab::None,
        }
    }

    /// How many tabs are open behind the one you are looking at.
    pub fn behind_count(&self) -> usize {
        (self.shells.len() + self.tabs.len() + self.open_boards.len()).saturating_sub(1)
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
        match job::load(&self.jobs_dir) {
            Ok(snapshot) => {
                self.local = snapshot.jobs;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e}")),
        }
        // Codex's running sessions, beside Claude Code's and read the same
        // way. Not an error when there are none: most machines have no Codex
        // on them, and a panel that said so every tick would be noise.
        self.local.extend(codex::load(&self.codex_dir));
        self.merge();
        // The same tick as the rows: the board is a file too, and an answer
        // should not wait for a key to be seen.
        if let Some(view) = self.board.as_mut() {
            view.reload();
        }
    }

    /// Show the board of the repository the session under the cursor is in —
    /// or say that it has none.
    ///
    /// With no session of this machine's there — a pane of your own, or a
    /// session on another machine — there is no repository to show one for. A
    /// remote `cwd` is never asked about: see `Job::repo` for what that costs.
    pub fn open_board(&mut self) {
        let repo = self.selected_repo();
        self.board = Some(BoardView::new(self.board_dir.clone(), repo));
    }

    /// The repository the cursor is in: a board row's own, or the one a
    /// session of this machine's is working in. `None` for a pane of your own
    /// or a session on another machine, whose path is never looked up here.
    pub fn selected_repo(&self) -> Option<String> {
        if let Some(board) = self.selected_board() {
            return Some(board.repo.clone());
        }
        match self.selected_job() {
            Some(job) if job.machine.is_none() => Some(board::topic_of(&job.cwd)),
            _ => None,
        }
    }

    pub fn close_board(&mut self) {
        self.board = None;
    }

    /// Make a board for the repository on screen — the panel's half of
    /// `svr board create`, and the owner's to do because the panel is the owner.
    pub fn create_board(&mut self) {
        let Some(view) = self.board.as_mut() else {
            return;
        };
        match view.create() {
            Ok(Some(name)) => self.error = Some(format!("created a board for {name}")),
            Ok(None) => {}
            Err(e) => self.error = Some(format!("could not create the board: {e}")),
        }
    }

    /// Post the message being written, and say where it went — or why not.
    pub fn send_board(&mut self) {
        let Some(view) = self.board.as_mut() else {
            return;
        };
        self.error = Some(match view.send() {
            Ok(message) => format!("posted to {}", board::topic_name(&message.topic)),
            Err(e) => format!("could not post: {e}"),
        });
    }

    /// What a machine last said it was running.
    ///
    /// A batch replaces that machine's rows wholesale, which is what makes a
    /// session disappearing over there a row disappearing over here.
    pub fn set_remote(&mut self, host: String, jobs: Vec<Job>) {
        self.remote.insert(host, jobs);
        self.merge();
    }

    /// The two sources as one list, in the panel's own order.
    fn merge(&mut self) {
        let anchor = self.anchor();
        let mut jobs = self.local.clone();
        for machine in self.remote.values() {
            jobs.extend(machine.iter().cloned());
        }
        job::sort(&mut jobs);
        self.snapshot = Snapshot { jobs };
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
            Some(Row::Board(i)) => self
                .board_rows
                .get(i)
                .map(|board| Anchor::Board(board.repo.clone())),
            _ => None,
        }
    }

    /// Whether this session is drawn as an agent under its lead.
    pub fn under_lead(&self, at: usize) -> bool {
        self.nested.contains(&at)
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.nested.clear();
        self.board_rows.clear();
        // Your own panes first, unheaded: one shell needs no group and several
        // read as a list on their own. Grouped by repository, a pane that says
        // where it is standing goes under that repository instead, beside the
        // sessions working there — see `rows_by_repo`.
        for i in 0..self.shells.len() {
            if self.group_by == GroupBy::Status || self.shell_repo(i).is_none() {
                self.rows.push(Row::Shell(i));
            }
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

    /// One group per repository, the sessions inside it ordered exactly as
    /// the status grouping orders them: oldest first, by when each was
    /// started.
    ///
    /// **The repository you started in first is at the top.** A repository is
    /// as old as its oldest session, and nothing that happens afterwards
    /// moves it — what is asking for you is said in the row itself, in the
    /// column that is there to say it. Ordering by status meant a repository
    /// jumped the moment a session asked or stopped asking, which is exactly
    /// while you are reaching for one of its rows.
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
            let shells: Vec<usize> = (0..self.shells.len())
                .filter(|&i| self.shell_repo(i).as_deref() == Some(repo.as_str()))
                .collect();
            if group.is_empty() && shells.is_empty() {
                continue;
            }
            if !self.rows.is_empty() {
                self.rows.push(Row::Spacer);
            }
            self.rows.push(Row::Repo(g));
            // The repository's board, first under its heading — where it is
            // found before the sessions talking on it.
            if let Some(board) = self.board_for(&group, &shells) {
                self.rows.push(Row::Board(self.board_rows.len()));
                self.board_rows.push(board);
            }
            // Your own panes standing here, ahead of the sessions — as they are
            // ahead of everything when they sit at the top of the list.
            self.rows.extend(shells.into_iter().map(Row::Shell));
            // A lead, then its own agents under it. Recorded here rather than
            // asked again while drawing: it is the same derivation, and two of
            // them is how the row and the indent come to disagree.
            for (at, agent) in crate::agents::under_leads(&self.snapshot, &group) {
                if agent {
                    self.nested.insert(at);
                }
                self.rows.push(Row::Job(at));
            }
        }
    }

    /// The board of the repository these sessions are in, if it has one.
    ///
    /// Asked of a session of this machine's only: a remote `cwd` is a path on
    /// another machine, never looked up here (see `Job::repo`). Which
    /// repository a directory is in is a walk up for `.git`, so it is kept per
    /// directory — rows are rebuilt on every refresh, and that answer changes
    /// only when a repository is made or moved. Whether the board exists, and
    /// how much is on it, is asked every time: that is what changes.
    fn board_for(&mut self, group: &[usize], shells: &[usize]) -> Option<BoardRow> {
        let cwd = group
            .iter()
            .map(|&i| &self.snapshot.jobs[i])
            .find(|job| job.machine.is_none())
            .map(|job| job.cwd.clone())
            .or_else(|| shells.iter().find_map(|&i| self.shells[i].cwd.clone()))?;
        let repo = self
            .topics
            .entry(cwd)
            .or_insert_with_key(|cwd| board::topic_of(cwd))
            .clone();
        let boards = Boards::at(self.board_dir.clone());
        boards.exists(&repo).then(|| BoardRow {
            name: board::topic_name(&repo),
            count: boards.count(&repo),
            repo,
        })
    }

    /// The repository the nth pane of your own is standing in, if it has said
    /// where it is standing.
    ///
    /// Worked out on every rebuild rather than kept: it is a walk up a local
    /// path for `.git`, which costs microseconds, and rows are rebuilt on a
    /// refresh or when a pane moves — never once per frame.
    fn shell_repo(&self, i: usize) -> Option<String> {
        self.shells.get(i)?.cwd.as_deref().map(job::repo_of)
    }

    /// The repositories in play, the one whose work started first at the top.
    ///
    /// A repository is as old as its oldest session, so a repository you have
    /// been in all morning stays where you last saw it however its sessions
    /// come and go. A session whose start time could not be read dates its
    /// repository no better than not at all, and a repository holding nothing
    /// but panes of your own has no session to date it by: both sort after
    /// the dated ones, by name.
    fn repos_in_order(&self) -> Vec<String> {
        /// Dated sessions first, then undated ones, then panes alone.
        type Age = (u8, Option<chrono::DateTime<chrono::Utc>>);
        let mut repos: Vec<(Age, String)> = Vec::new();
        let sessions = self.snapshot.jobs.iter().map(|job| {
            let (undated, at) = job::started(job);
            ((u8::from(undated), at), job.repo())
        });
        let shells = (0..self.shells.len()).filter_map(|i| Some(((2, None), self.shell_repo(i)?)));
        for (age, repo) in sessions.chain(shells) {
            match repos.iter_mut().find(|(_, name)| name == &repo) {
                // The oldest session in the repository is what it sorts by:
                // the repository has been in play since that one started.
                Some((oldest, _)) if age < *oldest => *oldest = age,
                Some(_) => {}
                None => repos.push((age, repo)),
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
                    (Row::Board(i), Anchor::Board(repo)) => {
                        self.board_rows.get(*i).is_some_and(|b| &b.repo == repo)
                    }
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

    /// Where to land once this session is gone: the next session down in its
    /// own repository, else the one above it there, else the nearest session
    /// on the panel — below first, as a list closing up over a removed line.
    ///
    /// Asked before it goes, because afterwards there is no row to measure
    /// from. Left to itself the cursor kept the old row *number*, which after
    /// a delete is a different row, often in another repository — and the
    /// tab in front fell back to whichever one was opened before it. Both are
    /// orders you cannot see, so both looked random.
    pub fn successor(&self, short: &str) -> Option<String> {
        let sessions: Vec<(usize, &Job)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(at, row)| match row {
                Row::Job(i) => Some((at, self.snapshot.jobs.get(*i)?)),
                _ => None,
            })
            .collect();
        let (here, gone) = sessions.iter().find(|(_, job)| job.short == short)?;
        let repo = gone.repo();
        let others = || sessions.iter().filter(|(_, job)| job.short != short);
        let below = others()
            .filter(|(at, job)| at > here && job.repo() == repo)
            .map(|(_, job)| job)
            .next();
        let above = || {
            others()
                .filter(|(at, job)| at < here && job.repo() == repo)
                .map(|(_, job)| job)
                .next_back()
        };
        let nearest = || {
            others()
                .min_by_key(|(at, _)| (at.abs_diff(*here), *at < *here))
                .map(|(_, job)| job)
        };
        below
            .or_else(above)
            .or_else(nearest)
            .map(|job| job.short.clone())
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

    /// The board row the cursor is on, if it is on one.
    pub fn selected_board(&self) -> Option<&BoardRow> {
        match self.current_row() {
            Some(Row::Board(i)) => self.board_rows.get(i),
            _ => None,
        }
    }

    /// Put the cursor on a repository's board row, so flipping to the board
    /// moves the highlight with you.
    pub fn select_board(&mut self, repo: &str) {
        if let Some(row) = self.rows.iter().position(|r| match r {
            Row::Board(i) => self.board_rows.get(*i).is_some_and(|b| b.repo == repo),
            _ => false,
        }) {
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

    /// `n` plain panes of your own, named the way an untitled shell is.
    fn shells(n: usize) -> Vec<Shell> {
        (0..n)
            .map(|i| Shell {
                name: if i == 0 {
                    "shell".to_string()
                } else {
                    format!("shell {}", i + 1)
                },
                ..Default::default()
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
        app.set_tabs(Front::Shell(1), Vec::new(), shells(2));

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
        app.set_tabs(Front::Shell(0), Vec::new(), shells(2));
        app.select_shell(1);
        assert_eq!(app.selected_shell(), Some(1));
        app.refresh();
        assert_eq!(app.selected_shell(), Some(1));
    }

    /// Sessions in three directories, so grouping has something to group.
    /// The `cwd`s are outside any repository, so each is its own name.
    /// Four sessions across three repositories, started an hour apart: beta's
    /// RUN first, then gamma's ASK, then beta's FIN, then alpha's OLD. The
    /// statuses are deliberately at odds with that order — what is asking is
    /// neither the oldest nor in the oldest repository.
    fn across_repos() -> Fixture {
        Fixture::new("app-repos")
            .job(
                "aaa",
                r#"{"state":"working","name":"RUN","cwd":"/tmp/savras-test-repos/beta","createdAt":"2026-09-22T08:00:00Z"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"ASK","needs":"answer: ?","cwd":"/tmp/savras-test-repos/gamma","createdAt":"2026-09-22T09:00:00Z"}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"ok"},"cwd":"/tmp/savras-test-repos/beta","createdAt":"2026-09-22T10:00:00Z"}"#,
            )
            .job(
                "ddd",
                r#"{"state":"done","name":"OLD","output":{"result":"ok"},"cwd":"/tmp/savras-test-repos/alpha","createdAt":"2026-09-22T11:00:00Z"}"#,
            )
    }

    /// A `~/.codex` holding one running session, started at `at`, in `cwd`.
    fn with_a_codex_session(tag: &str, cwd: &str, at: &str) -> PathBuf {
        const ID: &str = "01a0ccda-8ca8-7902-94df-5786dc86d974";
        let dir =
            std::env::temp_dir().join(format!("savras-app-codex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("thread-writer-locks")).unwrap();
        std::fs::write(
            dir.join("thread-writer-locks").join(format!("{ID}.lock")),
            "",
        )
        .unwrap();
        let day = dir.join("sessions").join("2026").join("09").join("23");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join(format!("rollout-2026-09-23T09-01-10-{ID}.jsonl")),
            format!(
                "{}\n{}\n",
                format_args!(
                    r#"{{"type":"session_meta","payload":{{"session_id":"{ID}","timestamp":"{at}","cwd":"{cwd}"}}}}"#
                ),
                r#"{"type":"event_msg","payload":{"type":"task_started","model_context_window":258400}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("session_index.jsonl"),
            format!(r#"{{"id":"{ID}","thread_name":"CODEX SETUP"}}"#),
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_codex_session_sits_with_the_claude_ones_in_its_own_repository() {
        // The gap this closes: a Codex session could post to a repository's
        // board while having no row on the panel it was talking through.
        let f = across_repos();
        let codex = with_a_codex_session(
            "beside",
            "/tmp/savras-test-repos/beta",
            // Between beta's RUN (08:00) and its FIN (10:00), so the ordering
            // has to interleave the two sources rather than append one.
            "2026-09-22T09:30:00Z",
        );
        let mut app = App::new(f.0.clone());
        app.codex_dir = codex.clone();
        app.set_group_by(GroupBy::Repo);
        app.refresh();

        assert_eq!(
            names_in_order(&app),
            ["RUN", "CODEX SETUP", "FIN", "ASK", "OLD"],
            "one list, ordered by when each session started"
        );
        let job = app
            .snapshot
            .jobs
            .iter()
            .find(|j| j.name == "CODEX SETUP")
            .unwrap();
        assert_eq!(job.client, job::Client::Codex);
        assert_eq!(job.repo(), "beta", "grouped by the same rule as the rest");
        assert_eq!(job.open_command()[0], "codex");
        let _ = std::fs::remove_dir_all(codex);
    }

    #[test]
    fn grouping_by_repository_puts_the_one_you_started_in_first() {
        // A repository is as old as its oldest session, and nothing a session
        // does afterwards moves it: `gamma` holds the question and still sits
        // under `beta`, which was open an hour earlier.
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        app.set_group_by(GroupBy::Repo);

        assert_eq!(app.groups, ["beta", "gamma", "alpha"]);
        // Inside a repository, the panel's own order holds: oldest first.
        assert_eq!(
            names_in_order(&app),
            ["RUN", "FIN", "ASK", "OLD"],
            "sessions are ordered within their repository, not shuffled"
        );
    }

    #[test]
    fn a_pane_of_your_own_sits_under_the_repository_it_is_standing_in() {
        // Three panes: one in `beta`, where sessions already are; one in
        // `delta`, where none are; and one that has not said where it stands.
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        let at = |dir: &str| Some(PathBuf::from(format!("/tmp/savras-test-repos/{dir}")));
        let mut panes = shells(3);
        panes[0].cwd = at("beta");
        panes[1].cwd = at("delta");
        app.set_tabs(Front::Shell(0), Vec::new(), panes.clone());
        app.set_group_by(GroupBy::Repo);

        // Only the one that said nothing is left at the top, unheaded.
        assert!(matches!(app.rows[0], Row::Shell(2)));
        // `delta` has a heading for a pane alone, and sits after the others:
        // with no session in it there is nothing to date it by.
        assert_eq!(app.groups, ["beta", "gamma", "alpha", "delta"]);
        fn under(app: &App, shell: usize) -> Option<String> {
            let at = app
                .rows
                .iter()
                .position(|r| matches!(r, Row::Shell(i) if *i == shell));
            app.rows[..at?].iter().rev().find_map(|r| match r {
                Row::Repo(g) => Some(app.groups[*g].clone()),
                _ => None,
            })
        }
        assert_eq!(under(&app, 0).as_deref(), Some("beta"));
        assert_eq!(under(&app, 1).as_deref(), Some("delta"));

        // A pane that walks into another repository follows on the next draw.
        panes[1].cwd = at("gamma");
        app.set_tabs(Front::Shell(0), Vec::new(), panes);
        assert_eq!(under(&app, 1).as_deref(), Some("gamma"));
        assert_eq!(app.groups, ["beta", "gamma", "alpha"]);

        // Grouped by status there are no repositories to be under.
        app.set_group_by(GroupBy::Status);
        assert!(matches!(
            app.rows[..3],
            [Row::Shell(0), Row::Shell(1), Row::Shell(2)]
        ));
    }

    #[test]
    fn a_deleted_session_is_replaced_by_its_neighbour_in_the_repository() {
        // beta: RUN, FIN · gamma: ASK · alpha: OLD
        let f = across_repos();
        let mut app = App::new(f.0.clone());
        app.set_group_by(GroupBy::Repo);

        // The next one down in its repository, else the one above there.
        assert_eq!(app.successor("aaa").as_deref(), Some("ccc"));
        assert_eq!(app.successor("ccc").as_deref(), Some("aaa"));
        // Alone in its repository: the nearest session on the panel, below
        // before above when both are as near.
        assert_eq!(app.successor("bbb").as_deref(), Some("ddd"));
        assert_eq!(app.successor("ddd").as_deref(), Some("bbb"));
        assert_eq!(app.successor("zzz"), None);

        // Grouped by status the repository still decides first: RUN's
        // neighbour is FIN, a group away, not ASK right above it.
        app.set_group_by(GroupBy::Status);
        assert_eq!(app.successor("aaa").as_deref(), Some("ccc"));
    }

    #[test]
    fn the_last_session_has_nowhere_to_go() {
        let f = Fixture::new("app-last")
            .job("aaa", r#"{"state":"working","name":"ONLY","cwd":"/tmp"}"#);
        let app = App::new(f.0.clone());
        assert_eq!(app.successor("aaa"), None);
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
        app.set_tabs(Front::Shell(0), Vec::new(), shells(1));
        assert_eq!(app.front_name(), None);
        app.set_tabs(Front::Shell(1), Vec::new(), shells(2));
        assert_eq!(app.front_name().as_deref(), Some("shell 2"));
    }

    const BOOKS: &str = "/elsewhere/books";

    /// A session in a repository, and boards of the fixture's own: another
    /// repository's, holding a message, and — when `made` — this repository's,
    /// holding one too.
    fn with_a_board(tag: &str, made: bool) -> (Fixture, PathBuf, String) {
        let f = Fixture::new(tag);
        let root = f.0.parent().unwrap().to_path_buf();
        let repo = root.join("savras");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let f = f.job(
            "aaa",
            &format!(
                r#"{{"state":"working","name":"SAVRAS-8","cwd":"{}"}}"#,
                repo.display()
            ),
        );
        let dir = root.join("boards");
        let here = repo.to_string_lossy().to_string();
        let boards = Boards::at(dir.clone());
        let owner = Owner::at_the_panel();
        boards.create(&owner, BOOKS).unwrap();
        boards.post("BOOKS", BOOKS, None, "not for savras").unwrap();
        if made {
            boards.create(&owner, &here).unwrap();
            boards
                .post("ROADMAP", &here, None, "the ubuntu leg hangs")
                .unwrap();
        }
        (f, dir, here)
    }

    fn on_board(app: &App) -> Vec<String> {
        app.board
            .as_ref()
            .expect("the board is open")
            .messages
            .iter()
            .map(|m| m.text.clone())
            .collect()
    }

    #[test]
    fn the_board_is_the_selected_sessions_repositorys_and_nobody_elses() {
        let (f, dir, here) = with_a_board("board-open", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();

        let view = app.board.as_ref().unwrap();
        assert_eq!(view.repo.as_deref(), Some(here.as_str()));
        assert!(view.exists);
        assert_eq!(view.name(), "savras");
        assert_eq!(on_board(&app), ["the ubuntu leg hangs"]);
        // Nothing picked: the bottom, where the newest is and a new message is
        // written.
        assert_eq!(view.selected, None);
    }

    #[test]
    fn a_repository_without_a_board_is_offered_one_and_the_panel_makes_it() {
        let (f, dir, here) = with_a_board("board-make", false);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir.clone();
        app.open_board();

        assert!(!app.board.as_ref().unwrap().exists);
        assert!(on_board(&app).is_empty());
        app.board.as_mut().unwrap().compose(false);
        assert!(
            app.board.as_ref().unwrap().compose.is_none(),
            "there is nowhere for a message to go"
        );

        app.create_board();
        assert_eq!(app.error.as_deref(), Some("created a board for savras"));
        assert!(Boards::at(dir).exists(&here));
        assert!(app.board.as_ref().unwrap().exists);
    }

    #[test]
    fn with_no_session_of_this_machines_selected_there_is_no_board_to_show() {
        let (f, dir, _) = with_a_board("board-shell", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir.clone();
        app.set_tabs(Front::Shell(0), Vec::new(), shells(1));
        app.select_shell(0);
        app.open_board();

        let view = app.board.as_ref().unwrap();
        assert!(view.repo.is_none());
        assert!(!view.exists);
        assert!(
            view.messages.is_empty(),
            "no other repository's board shows"
        );
        // And there is nothing to make one for.
        app.create_board();
        assert!(app.error.is_none());
        assert_eq!(Boards::at(dir).list().len(), 2);
    }

    #[test]
    fn the_owner_posts_to_the_repository_they_are_reading() {
        let (f, dir, here) = with_a_board("board-post", true);
        let boards = Boards::at(dir.clone());
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();

        let view = app.board.as_mut().unwrap();
        view.compose(false);
        view.type_text("looks good\nship it");
        app.send_board();
        let posted = boards.read(&here, 10).pop().unwrap();
        assert_eq!(posted.from, board::OWNER);
        assert_eq!(posted.text, "looks good ship it");
        assert_eq!(posted.topic, here);
        assert_eq!(app.error.as_deref(), Some("posted to savras"));
        let view = app.board.as_ref().unwrap();
        assert_eq!(
            view.messages.first().unwrap().id,
            posted.id,
            "and it is on screen, at the top"
        );
        assert_eq!(view.selected, None, "back at the top");

        // An answer carries what it answers, on the same board.
        let first = boards.read(&here, 10)[0].id.clone();
        let view = app.board.as_mut().unwrap();
        view.jump(true);
        view.compose(true);
        view.type_text("on it");
        app.send_board();
        let answer = boards.read(&here, 10).pop().unwrap();
        assert_eq!(answer.re.as_deref(), Some(first.as_str()));
        assert_eq!(boards.count(BOOKS), 1, "nothing reached another repository");
    }

    #[test]
    fn a_message_that_cannot_be_posted_keeps_its_words() {
        let (f, dir, here) = with_a_board("board-unsent", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir.clone();
        app.open_board();
        app.board.as_mut().unwrap().compose(false);
        app.board.as_mut().unwrap().type_text("   ");
        app.send_board();

        assert!(
            app.board.as_ref().unwrap().compose.is_some(),
            "still writing"
        );
        assert!(app.error.as_deref().unwrap().starts_with("could not post"));
        assert_eq!(
            Boards::at(dir.clone()).count(&here),
            1,
            "nothing was posted"
        );

        // A board deleted while you wrote takes nothing you typed with it.
        let view = app.board.as_mut().unwrap();
        view.cancel();
        view.compose(false);
        view.type_text("still here");
        Boards::at(dir)
            .delete(&Owner::at_the_panel(), &here)
            .unwrap();
        app.send_board();
        let kept = app.board.as_ref().unwrap().compose.as_ref().unwrap();
        assert_eq!(kept.text, "still here");
        assert!(app.error.as_deref().unwrap().contains("has no board"));
    }

    #[test]
    fn a_message_arriving_while_you_read_is_shown_at_the_top() {
        let (f, dir, here) = with_a_board("board-arrive", true);
        let boards = Boards::at(dir.clone());
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();

        boards.post("ROADMAP", &here, None, "green now").unwrap();
        app.refresh();
        let view = app.board.as_ref().unwrap();
        assert_eq!(view.messages.first().unwrap().text, "green now");
        assert_eq!(view.selected, None, "still at the top, where it arrived");

        // Scrolled down to read something, the cursor stays on that message
        // while a new one pushes it along.
        let view = app.board.as_mut().unwrap();
        view.jump(true);
        let picked = view.messages.last().unwrap().id.clone();
        boards.post("ROADMAP", &here, None, "and again").unwrap();
        app.refresh();
        let view = app.board.as_ref().unwrap();
        assert_eq!(view.messages[view.selected.unwrap()].id, picked);
    }

    #[test]
    fn reading_the_board_writes_nothing() {
        // Unread belongs to the agents' hook. The panel looking must not be
        // what tells it an agent has already seen something.
        fn tree(dir: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            let mut todo = vec![dir.to_path_buf()];
            while let Some(at) = todo.pop() {
                for entry in std::fs::read_dir(&at).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        todo.push(path);
                    } else {
                        let bytes = std::fs::read(&path).unwrap();
                        files.push((path, bytes));
                    }
                }
            }
            files.sort();
            files
        }

        let (f, dir, _) = with_a_board("board-looking", true);
        let before = tree(&dir);

        let mut app = App::new(f.0.clone());
        app.board_dir = dir.clone();
        app.open_board();
        let view = app.board.as_mut().unwrap();
        view.step(-1);
        view.jump(true);
        app.refresh();
        app.close_board();

        assert_eq!(tree(&dir), before, "the boards are exactly as they were");
    }

    #[test]
    fn a_repositorys_board_is_a_row_under_its_heading_while_it_exists() {
        let (f, dir, here) = with_a_board("board-row", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir.clone();
        app.set_group_by(GroupBy::Repo);

        let heading = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Repo(_)))
            .unwrap();
        assert!(
            matches!(app.rows[heading + 1], Row::Board(0)),
            "first under its heading: {:?}",
            app.rows
        );
        assert_eq!(
            app.board_rows,
            [BoardRow {
                repo: here.clone(),
                name: "savras".to_string(),
                count: 1
            }]
        );

        // Grouped by status there is no heading to put it under.
        app.set_group_by(GroupBy::Status);
        assert!(!app.rows.iter().any(|r| matches!(r, Row::Board(_))));
        assert!(app.board_rows.is_empty());

        // Deleted, it is gone on the next refresh.
        app.set_group_by(GroupBy::Repo);
        Boards::at(dir)
            .delete(&Owner::at_the_panel(), &here)
            .unwrap();
        app.refresh();
        assert!(app.board_rows.is_empty());
    }

    #[test]
    fn the_cursor_lands_on_a_board_row_and_keeps_to_it() {
        let (f, dir, here) = with_a_board("board-cursor", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.set_group_by(GroupBy::Repo);

        app.jump(false);
        assert_eq!(
            app.selected_board().map(|b| b.repo.as_str()),
            Some(here.as_str()),
            "the first row you can land on"
        );
        assert_eq!(app.selected_repo().as_deref(), Some(here.as_str()));
        app.step(1);
        assert_eq!(
            app.selected_job().map(|j| j.name.as_str()),
            Some("SAVRAS-8")
        );

        app.select_board(&here);
        app.refresh();
        assert!(
            app.selected_board().is_some(),
            "a refresh keeps the cursor on the board"
        );
    }

    #[test]
    fn a_board_open_as_a_tab_is_marked_and_named_like_one() {
        let (f, dir, here) = with_a_board("board-tab-mark", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.set_group_by(GroupBy::Repo);

        assert_eq!(app.board_tab(&here), Tab::None);
        app.set_open_boards(vec![here.clone()]);
        assert_eq!(app.board_tab(&here), Tab::Behind);
        app.set_tabs(Front::Board(here.clone()), Vec::new(), shells(1));
        assert_eq!(app.board_tab(&here), Tab::Front);
        assert_eq!(app.front_name().as_deref(), Some("board · savras"));
        assert_eq!(app.behind_count(), 1, "the shell underneath it");
    }

    #[test]
    fn nothing_picked_is_a_new_message_and_a_picked_one_is_answered() {
        let (_f, dir, here) = with_a_board("board-pick", true);
        Boards::at(dir.clone())
            .post("RESEARCH", &here, None, "second")
            .unwrap();
        let mut view = BoardView::new(dir, Some(here));

        assert_eq!(view.selected, None);
        view.step(-1);
        assert_eq!(view.selected, None, "nothing above the top");
        view.step(1);
        assert_eq!(view.selected, Some(0), "down from the top is the newest");
        assert_eq!(view.messages[0].text, "second");
        view.step(1);
        view.step(1);
        assert_eq!(view.selected, Some(1), "and it stops at the oldest");
        view.step(-1);
        view.step(-1);
        assert_eq!(view.selected, None, "up past the newest lets go");

        view.type_text("new");
        assert!(view.compose.as_ref().unwrap().re.is_none());
        view.cancel();
        view.step(1);
        view.type_text("on it");
        let answering = view.compose.as_ref().unwrap().re.as_ref().unwrap();
        assert_eq!(answering.text, "second");
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
