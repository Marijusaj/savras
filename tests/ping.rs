//! End-to-end: run the real binary in a real pseudo-terminal, change a job on
//! disk, and read the notification off the terminal stream.
//!
//! The unit tests in `ping.rs` cover the rules; this covers the wiring — that
//! the watcher, the refresh and the escape sequence are actually joined up,
//! which is the part no pure test can see.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, NativePtySystem, PtySize, PtySystem};

const WORKING: &str = r#"{"state":"working","name":"RUN","detail":"compiling","sessionId":"s-1"}"#;
const ASKING: &str =
    r#"{"state":"working","name":"RUN","needs":"answer: which one?","sessionId":"s-1"}"#;
const WORKING_2: &str =
    r#"{"state":"working","name":"OTHER","detail":"compiling","sessionId":"s-2"}"#;
const ASKING_2: &str =
    r#"{"state":"working","name":"OTHER","needs":"answer: the other one?","sessionId":"s-2"}"#;

#[test]
fn a_session_that_starts_asking_notifies_the_terminal() {
    let panel = Panel::start("starts-asking", &[("aaa", WORKING)], &[]);

    // Let it draw once and learn the state of the world, which must be silent.
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !panel.drain(Duration::from_millis(100)).contains("\x1b]9;"),
        "a session that was already there must not ping on startup"
    );

    panel.set("aaa", ASKING);
    let seen = panel.wait_for_notification();

    assert_eq!(
        notification(&seen).as_deref(),
        Some("RUN needs you — answer: which one?")
    );
    // And the panel says which session it was: the sound is over in a second,
    // and you may have been in another application when it happened.
    assert!(
        seen.contains('●'),
        "the pinged session is not marked on screen"
    );
}

#[test]
fn a_question_arriving_inside_the_quiet_period_is_still_announced() {
    // The regression: news that landed while the panel was staying quiet used
    // to be consumed and dropped, so the second session to ask got no sound
    // and no mark — ever. A one-second quiet period keeps this test short;
    // twenty seconds is the default.
    let panel = Panel::start(
        "held-through-quiet",
        &[("aaa", WORKING), ("bbb", WORKING_2)],
        &["--quiet", "1"],
    );
    std::thread::sleep(Duration::from_millis(700));
    panel.drain(Duration::from_millis(100));

    panel.set("aaa", ASKING);
    let first = panel.wait_for_notification();
    assert_eq!(
        notification(&first).as_deref(),
        Some("RUN needs you — answer: which one?")
    );

    // Immediately, while the panel is still holding its tongue.
    panel.set("bbb", ASKING_2);
    let second = panel.wait_for_notification();
    assert_eq!(
        notification(&second).as_deref(),
        Some("OTHER needs you — answer: the other one?"),
        "the question asked inside the quiet period was never announced"
    );
}

/// The first OSC 9 notification in a stream of terminal output.
fn notification(seen: &str) -> Option<String> {
    let start = seen.find("\x1b]9;")?;
    Some(
        seen[start + 4..]
            .chars()
            .take_while(|c| *c != '\x07')
            .collect(),
    )
}

/// A real `svr solo` in a real pseudo-terminal, over a jobs directory this
/// test owns.
struct Panel {
    dir: PathBuf,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Panel {
    fn start(name: &str, jobs: &[(&str, &str)], extra: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "savras-ping-e2e-{}-{name}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (short, state) in jobs {
            std::fs::create_dir_all(dir.join(short)).unwrap();
            std::fs::write(dir.join(short).join("state.json"), state).unwrap();
        }

        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_svr"));
        // Nothing but this fixture: not the machines the developer watches,
        // whose sessions would be counted alongside these and change what the
        // ping says.
        command.args(["solo", "--no-sound", "--machine", "off"]);
        command.args(extra);
        command.arg("--jobs-dir");
        command.arg(&dir);
        // The panel would otherwise wrap its escape sequence for tmux
        // passthrough, and this pty is not tmux.
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

        Panel { dir, rx, child }
    }

    fn set(&self, short: &str, state: &str) {
        std::fs::write(self.dir.join(short).join("state.json"), state).unwrap();
    }

    /// Everything the panel wrote in the next `patience`.
    fn drain(&self, patience: Duration) -> String {
        let mut out = Vec::new();
        let deadline = Instant::now() + patience;
        while let Ok(chunk) = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            out.extend_from_slice(&chunk);
            if Instant::now() >= deadline {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Read until a notification arrives, or give up. Generous, because the
    /// panel re-reads on a filesystem event and in any case within its
    /// staleness window, and a held ping waits out the quiet period on top.
    fn wait_for_notification(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while Instant::now() < deadline && !seen.contains("\x1b]9;") {
            seen.push_str(&self.drain(Duration::from_millis(250)));
        }
        assert!(
            seen.contains("\x1b]9;"),
            "no OSC 9 notification reached the terminal"
        );
        // The sound and the mark are not the same event: the notification goes
        // out as the job is read, and the mark appears in the frame drawn
        // after it. On a cold binary that frame is late enough to miss, which
        // made this look flaky when it was only early.
        seen.push_str(&self.drain(Duration::from_millis(400)));
        seen
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
