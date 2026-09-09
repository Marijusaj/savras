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
    // carrying the job id, so the pane's own foreground process answers the
    // question. The script here writes one for itself, which is exactly what
    // starting `claude` in a tab does.
    let pane = Pane::start_watching(
        "adopted",
        "printf '{\"jobId\":\"abc12345\"}' > \"$SAVRAS_TEST_DIR\"/sessions/$$.json; sleep 30",
        true,
    );
    std::fs::create_dir_all(pane.dir.join("jobs/abc12345")).unwrap();
    std::fs::write(
        pane.dir.join("jobs/abc12345/state.json"),
        r#"{"state":"working","name":"LINUX-B","cwd":"/tmp"}"#,
    )
    .unwrap();

    // The marker, not just the name: the row exists either way, and what is
    // being tested is that the panel knows this pane *is* it.
    let seen = pane.wait_for("\u{25b6} LINUX-B");
    assert!(
        seen.contains("\u{25b6} LINUX-B"),
        "the pane is that session and the panel never marked it as one; it drew:\n{seen}"
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

/// A real `svr` hosting a command of the test's choosing, in a real pty.
struct Pane {
    dir: PathBuf,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn Child + Send + Sync>,
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
        }
    }

    /// Send keys, then read until the panel says the thing — or long enough to
    /// be sure it never will.
    fn press_for(&mut self, keys: &[u8], needle: &str) -> String {
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
        self.wait_for(needle)
    }

    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while Instant::now() < deadline && !seen.contains(needle) {
            while let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(250)) {
                seen.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        seen
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
