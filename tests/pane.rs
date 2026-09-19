//! End-to-end: the working pane's own idea of its size.
//!
//! A program in the pane that believes it is wider than it is draws a line,
//! wraps it where the pane does not, and lands the tail on top of the row it
//! just wrote — the screen comes up interleaved with itself. So what the child
//! is told has to be exactly what it is given, and the only way to know is to
//! ask the child.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, NativePtySystem, PtySize, PtySystem};

/// The terminal the test runs Savras in, and the panel it asks for. What is
/// left for the pane is the terminal, less the panel, less the divider.
const COLS: u16 = 100;
const ROWS: u16 = 40;
const PANEL: u16 = 30;

#[test]
fn the_pane_tells_its_child_the_size_it_actually_has() {
    // `stty size` prints "<rows> <cols>" as the kernel has them for the pty —
    // which is what any program in the pane reads, and what Claude Code wraps
    // its output to. The space becomes an `x` so the answer survives being
    // drawn: the panel positions the cursor between cells, and a run of text
    // with a space in it does not arrive as one string.
    let want = format!("{ROWS}x{}", COLS - PANEL - 1);
    let pane = Pane::start("size", "stty size | tr ' ' 'x'; sleep 30");
    let seen = pane.wait_for(&want);
    assert!(
        seen.contains(&want),
        "the child was told the wrong size; wanted {want}, and the panel drew:\n{seen}"
    );
}

#[test]
fn a_second_savras_inside_a_pane_refuses_rather_than_nesting() {
    // Two panels in one terminal is two lists of the same sessions and two
    // sets of the keys, and an `attach` opened twice over one session if you
    // use both. The way to another pane is ctrl-t.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_svr"))
        .env("SAVRAS_PANE", "1234")
        .output()
        .unwrap();
    let why = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{why}");
    assert!(why.contains("already running"), "{why}");
    assert!(
        why.contains("ctrl-t"),
        "it has to say what to do instead: {why}"
    );
}

#[test]
fn the_marker_is_set_in_the_pane_so_the_second_one_can_tell() {
    // The refusal above is only as good as the marker reaching the shell you
    // would type `svr` into.
    let pane = Pane::start(
        "marker",
        "test -n \"$SAVRAS_PANE\" && echo marked; sleep 30",
    );
    let seen = pane.wait_for("marked");
    assert!(
        seen.contains("marked"),
        "the pane's child never saw the marker; the panel drew:\n{seen}"
    );
}

#[test]
fn keys_says_what_arrived_and_what_savras_makes_of_it() {
    // The question "does my terminal send this chord" has no answer you can
    // reach by staring at the screen, so there is a mode that answers it.
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_svr"))
        .arg("--keys")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\x1b[1;6A\x03")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);

    assert!(said.contains("\\e[1;6A"), "the bytes as sent: {said}");
    assert!(said.contains("flip back a tab"), "what it means: {said}");
}

#[test]
fn a_pane_is_named_after_what_it_is_running() {
    // The row used to say "shell" for the whole life of the pane, whatever you
    // did in it — so a shell you had ssh'd out of still read as a shell. The
    // name is now the program in the foreground, and the title it set, if it
    // set one, follows it across the row.
    //
    // `exec` is the point of the script: the pane was started as `sh`, and
    // what it says a moment later has to be `sleep`, or the name is still
    // reporting what Savras spawned rather than what is there now.
    let pane = Pane::start_watching(
        "named",
        "printf '\\033]0;on-the-vm\\007'; exec sleep 30",
        true,
    );
    let seen = pane.wait_for("sleep");
    assert!(
        seen.contains("sleep"),
        "the row still names what Savras started, not what is running; the panel drew:\n{seen}"
    );
    assert!(
        seen.contains("on-the-vm"),
        "the title the pane set never reached the row; the panel drew:\n{seen}"
    );
}

#[test]
fn a_pane_you_start_a_session_in_becomes_that_session_s_tab() {
    // The panel used to show this twice: an anonymous `shell 2` you were
    // looking at, and the session's own row, with nothing saying they were the
    // same thing — and enter on the row attached a second time to a session
    // already in front of you.
    //
    // Claude Code writes ~/.claude/sessions/<pid>.json for every live session,
    // carrying the job id. Here the pane's own foreground process writes one
    // for itself, which is the simple shape: the process the terminal has in
    // front is the one the file is named after.
    let pane = Pane::start_watching(
        "adopted",
        "printf '{\"jobId\":\"abc12345\"}' > \"$SAVRAS_TEST_DIR\"/sessions/$$.json; sleep 30",
        true,
    );
    pane.seed_job("abc12345", "LINUX-B");

    // The marker, not just the name: the row exists either way, and what is
    // being tested is that the panel knows this pane *is* it.
    let seen = pane.wait_for("\u{25b6} LINUX-B");
    assert!(
        seen.contains("\u{25b6} LINUX-B"),
        "the pane is that session and the panel never marked it as one; it drew:\n{seen}"
    );
}

#[test]
fn a_session_owned_by_a_child_of_the_foreground_process_is_still_this_pane_s() {
    // And here is the shape a real `claude` has, which the simple one above
    // hid: the process the terminal has in front is a launcher, and it forks
    // the session as a *child in the same process group*. The child writes the
    // session file; the leader has none and never will.
    //
    // So asking `sessions/<leader>.json` could only ever miss, and every tab
    // you started a session in stayed a row called `claude` sitting above the
    // row for the very session inside it — two rows for one session, which is
    // exactly what M3.6 was supposed to have ended.
    //
    // The script is that shape and nothing else: `sh` stays in front and
    // waits, and the file is named after the child it started. `sh -c` runs
    // without job control, so the child is in the shell's group — which is the
    // whole point.
    let pane = Pane::start_watching(
        "adopted-child",
        "sleep 30 & printf '{\"jobId\":\"abc12345\"}' > \"$SAVRAS_TEST_DIR\"/sessions/$!.json; wait",
        true,
    );
    pane.seed_job("abc12345", "LINUX-B");

    let seen = pane.wait_for("\u{25b6} LINUX-B");
    assert!(
        seen.contains("\u{25b6} LINUX-B"),
        "the session is a child of the pane's foreground process and the panel \
         never joined them; it drew:\n{seen}"
    );
}

#[test]
fn ctrl_t_asks_and_is_answered_from_the_working_pane() {
    // With machines written down, ctrl-t asks where the tab should open. It is
    // pressed from the working pane — that is the whole reason it exists as a
    // chord — so the question has to be *shown* there and the answer *read*
    // there. Neither was true: the panel drew its background hint instead of
    // the question, and the digit went to the child, cancelling the question
    // with the very keystroke meant to answer it. ctrl-t did nothing, twice.
    let mut pane = Pane::start_on("ctrl-t", "cat", &["a-box"]);
    pane.wait_for("ctrl-g focus");

    let asked = pane.press_for(b"\x14", "new tab");
    assert!(
        asked.contains("1 here") && asked.contains("2 a-box"),
        "ctrl-t from the pane asked nothing you could see; the panel drew:\n{asked}"
    );

    // `1` is here. The proof it was read as an answer rather than typed into
    // the child is a second pane of your own — the header's count of what is
    // behind the one in front, which is 0 and undrawn until there are two.
    // The count and not the new row's name: a pane is named after the program
    // running in it, and for the first moment of its life that is the shell it
    // is about to exec out of.
    let opened = pane.press_for(b"1", "\u{25b7} 1");
    assert!(
        opened.contains("\u{25b7} 1"),
        "the answer went to the child instead of the question; the panel drew:\n{opened}"
    );
}

#[test]
fn a_pane_showing_a_daemon_session_is_that_session_s_tab() {
    // The shape that actually reaches the panel, and the one both earlier
    // fixes missed. Every session Savras lists runs with `backend: daemon`:
    // the work happens in a process under the Claude Code daemon, which is
    // what writes `sessions/<pid>.json`, and your pane holds a client of it.
    // The two share no pid, no process group, and nothing on disk — verified
    // on a live session, whose file belonged to a `kind: bg` process three
    // parents away from any terminal.
    //
    // What the pane does say is its title, which Claude Code sets to the
    // session's name. That is the join, and this is the whole test: a title
    // and a job by that name, with no session file written anywhere.
    let pane = Pane::start_watching(
        "daemon",
        "printf '\\033]0;\\342\\234\\263 ROADMAP\\007'; sleep 30",
        true,
    );
    pane.seed_job("cc76108a", "ROADMAP");

    let seen = pane.wait_for("\u{25b6} ROADMAP");
    assert!(
        seen.contains("\u{25b6} ROADMAP"),
        "the pane is showing that session and the panel kept it as a shell row \
         beside the session's own; it drew:\n{seen}"
    );
}

#[test]
fn a_title_two_sessions_answer_to_joins_neither() {
    // A guess here is worse than no answer: the pane would be marked as one
    // session while `enter` on the row went to the other.
    let pane = Pane::start_watching(
        "ambiguous",
        "printf '\\033]0;\\342\\234\\263 ROADMAP\\007'; sleep 30",
        true,
    );
    pane.seed_job("cc76108a", "ROADMAP");
    pane.seed_job("dd881190", "ROADMAP");

    // Both rows are drawn, and then a moment longer than adoption would have
    // taken. A pane that had been joined says so in the header, the way the
    // test above reads it; this one must not.
    let mut seen = pane.wait_for("ROADMAP");
    seen.push_str(&pane.read_for(3));
    assert!(
        !seen.contains("\u{25b6} ROADMAP"),
        "an ambiguous title was guessed at rather than left alone; it drew:\n{seen}"
    );
}

#[test]
fn ctrl_l_paints_the_screen_again_rather_than_waiting_for_an_answer() {
    // `Terminal::clear` asks the terminal where the cursor is and waits for the
    // reply. Savras reads stdin itself, so the reply never reaches the asker:
    // ctrl-l stalled for the length of the timeout and then failed outright,
    // tearing the screen down with "the cursor position could not be read".
    let mut pane = Pane::start("ctrl-l", "cat");
    pane.wait_for("ctrl-g focus");

    // ctrl-g first: ctrl-l is the panel's, and the pane has the keys until it
    // is asked for them.
    pane.press_for(b"\x07", "enter open");
    let seen = pane.press_for(b"\x0c", "SAVRAS");

    assert!(
        !seen.contains("cursor position"),
        "ctrl-l asked the terminal a question nobody was left to answer:\n{seen}"
    );
    assert!(
        seen.contains("SAVRAS") && seen.contains("No Claude Code sessions."),
        "ctrl-l should have painted the whole panel again; it drew:\n{seen}"
    );
}

#[test]
fn a_board_opens_as_a_tab_and_what_is_typed_there_is_posted() {
    // The unit tests have every piece; this is the loop they are wired into.
    // A row that appears when the owner makes a board, enter putting the board
    // in the working pane rather than a terminal, and the keys going to it
    // rather than to the program underneath — `j` and `q` included, which the
    // panel would otherwise take.
    let mut pane = Pane::start_watching("board-tab", "cat", true);
    pane.wait_for("OTHER");

    // `--repo`, not the working directory: on macOS `/tmp` is a link, and the
    // directory a process is started in comes back as `/private/tmp`, which is
    // not the `/tmp` the session says it is in.
    let made = pane.board(&["create", "--repo", "/tmp"]);
    assert!(made.contains("created a board for tmp"), "{made}");
    let row = pane.wait_for("\u{2261} board");
    assert!(
        row.contains("\u{2261} board"),
        "the board never got a row under its repository; the panel drew:\n{row}"
    );

    pane.press_for(b"\x07", "enter open");
    // The board is the first row: the pane of your own is standing in its
    // scratch directory, so it has gone under a heading of its own, last.
    let on_row = pane.press_for(b"g", "c clean");
    assert!(
        on_row.contains("c clean"),
        "the cursor never reached the board row; the panel drew:\n{on_row}"
    );

    let tab = pane.press_for(b"\r", "type to post");
    assert!(
        tab.contains("type to post"),
        "enter on the board row should have opened it in the pane; it drew:\n{tab}"
    );

    pane.press_for(b"just a quick note", "just a quick note");
    let sent = pane.press_for(b"\r", "posted to tmp");
    assert!(sent.contains("posted to tmp"), "{sent}");

    let read = pane.board(&["read", "--all-topics"]);
    assert!(read.contains("owner: just a quick note"), "{read}");
}

/// A real `svr` hosting a command of the test's choosing, in a real pty.
struct Pane {
    dir: PathBuf,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// What a person looking at this terminal would see.
    ///
    /// The renderer sends only the cells that changed, so a phrase whose space
    /// was already a space arrives as two words with a cursor move between
    /// them — `c`, then `ESC[40;86H`, then `clean` — and is never in the byte
    /// stream whole. Needles are looked for here as well, where it is.
    screen: std::cell::RefCell<vt100::Parser>,
}

impl Pane {
    fn start(name: &str, script: &str) -> Self {
        Pane::start_watching(name, script, false)
    }

    /// With machines named, which is what gives ctrl-t more than one answer.
    /// They are never reached: the ssh that watches them fails, says so, and
    /// retries — which is itself worth having under the test, since an error
    /// in the footer must not be what hides the question.
    fn start_on(name: &str, script: &str, machines: &[&str]) -> Self {
        Pane::start_with(name, script, false, machines)
    }

    /// `sessions` seeds the jobs directory with one, for the tests that need
    /// the panel to draw its list: with nothing to watch the panel says so
    /// instead, and your own panes are not listed either — one shell and no
    /// sessions is a list with nothing in it to tell apart.
    fn start_watching(name: &str, script: &str, sessions: bool) -> Self {
        Pane::start_with(name, script, sessions, &[])
    }

    fn start_with(name: &str, script: &str, sessions: bool, machines: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "savras-pane-e2e-{}-{name}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // `jobs` and `sessions` side by side, as Claude Code keeps them: the
        // panel finds the second by looking next to the first.
        std::fs::create_dir_all(dir.join("jobs")).unwrap();
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        if sessions {
            std::fs::create_dir_all(dir.join("jobs/aaaaaaaa")).unwrap();
            std::fs::write(
                dir.join("jobs/aaaaaaaa/state.json"),
                r#"{"state":"working","name":"OTHER","cwd":"/tmp"}"#,
            )
            .unwrap();
        }

        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_svr"));
        // Nothing but this fixture: not the machines the developer watches.
        command.arg("--no-sound");
        if machines.is_empty() {
            command.args(["--machine", "off"]);
        } else {
            for host in machines {
                command.args(["--machine", host]);
            }
        }
        command.args(["--width", &PANEL.to_string()]);
        command.arg("--jobs-dir");
        command.arg(dir.join("jobs"));
        command.args(["--", "sh", "-c", script]);
        command.env("SAVRAS_TEST_DIR", &dir);
        // A home of its own. A panel starting up opens the boards in its config
        // directory — and the first time, migrates them — so a test run under
        // the developer's `HOME` was a test that could rewrite their boards.
        command.env("HOME", dir.join("home"));
        command.env_remove("XDG_CONFIG_HOME");
        command.env_remove("TMUX");
        // Savras refuses to run inside its own pane, and these tests are run
        // from one as often as not — the panel is where the work happens. The
        // marker is inherited, so it has to be taken off the child or every
        // one of these fails with "already running in this terminal", which
        // says nothing about the code under test.
        command.env_remove("SAVRAS_PANE");
        command.env("TERM", "xterm-256color");
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 || tx.send(buffer[..n].to_vec()).is_err() {
                    return;
                }
            }
        });

        let writer = pty.master.take_writer().unwrap();
        Pane {
            dir,
            rx,
            writer,
            child,
            screen: std::cell::RefCell::new(vt100::Parser::new(ROWS, COLS, 0)),
        }
    }

    /// A job for the panel to have a row for, which is what a pane gets joined
    /// to. Written after the pane is up, as a real one appears while you watch.
    fn seed_job(&self, short: &str, name: &str) {
        std::fs::create_dir_all(self.dir.join("jobs").join(short)).unwrap();
        std::fs::write(
            self.dir.join("jobs").join(short).join("state.json"),
            format!(r#"{{"state":"working","name":"{name}","cwd":"/tmp"}}"#),
        )
        .unwrap();
    }

    /// Send keys, then read until the panel says the thing — or long enough to
    /// be sure it never will.
    fn press_for(&mut self, keys: &[u8], needle: &str) -> String {
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
        self.wait_for(needle)
    }

    /// Send keys and let the screen settle, for a key whose effect has nothing
    /// new to say. The panel reads one key per read, so two keys written back
    /// to back can arrive as one read and be taken for neither.
    fn press(&mut self, keys: &[u8]) {
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
        self.read_for(1);
    }

    /// `svr board …`, as the owner would run it in a terminal of their own —
    /// no agent's marks in the environment — against this pane's boards.
    fn board(&self, args: &[&str]) -> String {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_svr"))
            .arg("board")
            .args(args)
            .current_dir(&self.dir)
            .env("HOME", self.dir.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_AGENT")
            .env_remove("CLAUDE_JOB_DIR")
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// Keep reading for a while longer, for the tests that assert the panel
    /// does *not* draw something: absence needs a settled screen, not the
    /// first frame that happens to arrive.
    fn read_for(&self, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut seen = String::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(250)) {
                self.take(&chunk, &mut seen);
            }
        }
        self.said(seen)
    }

    /// A chunk of output: onto the screen, and onto what was received.
    fn take(&self, chunk: &[u8], seen: &mut String) {
        self.screen.borrow_mut().process(chunk);
        seen.push_str(&String::from_utf8_lossy(chunk));
    }

    /// Whether the terminal is showing `needle` right now.
    fn on_screen(&self, needle: &str) -> bool {
        self.screen.borrow().screen().contents().contains(needle)
    }

    /// The screen as it stands, then the bytes that built it: the first is
    /// what an assertion should read, and the second is what explains it.
    fn said(&self, seen: String) -> String {
        format!("{}\n{seen}", self.screen.borrow().screen().contents())
    }

    /// Read until the panel draws `needle`, or ten seconds pass.
    ///
    /// Every read is bounded by the deadline, and the needle is looked for after
    /// each one. The first cut drained the channel until it had been quiet for a
    /// quarter of a second and only then looked at either — so a panel that
    /// never went quiet for that long held the test for as long as it kept
    /// drawing, which on a Linux runner was the six hours CI allows.
    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        // A needle the screen was showing before anything arrived is not an
        // answer: `new tab` sits in the footer all along, and would be found
        // before the key meant to draw the question had done anything. Then
        // only a fresh arrival counts.
        let already = self.on_screen(needle);
        while !(seen.contains(needle) || (!already && self.on_screen(needle))) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if let Ok(chunk) = self.rx.recv_timeout(left.min(Duration::from_millis(250))) {
                self.take(&chunk, &mut seen);
            }
        }
        self.said(seen)
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
