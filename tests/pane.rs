//! End-to-end: the working pane's own idea of its size.
//!
//! A program in the pane that believes it is wider than it is draws a line,
//! wraps it where the pane does not, and lands the tail on top of the row it
//! just wrote — the screen comes up interleaved with itself. So what the child
//! is told has to be exactly what it is given, and the only way to know is to
//! ask the child.

use std::io::Read;
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

/// A real `svr` hosting a command of the test's choosing, in a real pty.
struct Pane {
    dir: PathBuf,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Pane {
    fn start(name: &str, script: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "savras-pane-e2e-{}-{name}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_svr"));
        command.args(["--no-sound", "--width", &PANEL.to_string()]);
        command.arg("--jobs-dir");
        command.arg(&dir);
        command.args(["--", "sh", "-c", script]);
        command.env_remove("TMUX");
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

        Pane { dir, rx, child }
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
