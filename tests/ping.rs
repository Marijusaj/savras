//! End-to-end: run the real binary in a real pseudo-terminal, change a job on
//! disk, and read the notification off the terminal stream.
//!
//! The unit tests in `ping.rs` cover the rules; this covers the wiring — that
//! the watcher, the refresh and the escape sequence are actually joined up,
//! which is the part no pure test can see.

use std::io::Read;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const WORKING: &str = r#"{"state":"working","name":"RUN","detail":"compiling","sessionId":"s-1"}"#;
const ASKING: &str =
    r#"{"state":"working","name":"RUN","needs":"answer: which one?","sessionId":"s-1"}"#;

#[test]
fn a_session_that_starts_asking_notifies_the_terminal() {
    let dir = std::env::temp_dir().join(format!("savras-ping-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let job = dir.join("aaa");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::write(job.join("state.json"), WORKING).unwrap();

    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_svr"));
    command.args(["solo", "--no-sound", "--jobs-dir"]);
    command.arg(&dir);
    // The panel would otherwise wrap its escape sequence for tmux passthrough,
    // and this pty is not tmux.
    command.env_remove("TMUX");
    command.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(command).unwrap();
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

    // Let it draw once and learn the state of the world, which must be silent.
    std::thread::sleep(Duration::from_millis(700));
    let opening = drain(&rx, Duration::from_millis(100));
    assert!(
        !opening.contains("\x1b]9;"),
        "a session that was already there must not ping on startup"
    );

    std::fs::write(job.join("state.json"), ASKING).unwrap();

    // The panel re-reads on a filesystem event, and in any case within its
    // staleness window.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seen = String::new();
    while Instant::now() < deadline && !seen.contains("\x1b]9;") {
        seen.push_str(&drain(&rx, Duration::from_millis(250)));
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    let start = seen
        .find("\x1b]9;")
        .unwrap_or_else(|| panic!("no OSC 9 notification reached the terminal"));
    let body: String = seen[start + 4..]
        .chars()
        .take_while(|c| *c != '\x07')
        .collect();
    assert_eq!(body, "RUN needs you — answer: which one?");
}

fn drain(rx: &std::sync::mpsc::Receiver<Vec<u8>>, patience: Duration) -> String {
    let mut out = Vec::new();
    let deadline = Instant::now() + patience;
    while let Ok(chunk) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        out.extend_from_slice(&chunk);
        if Instant::now() >= deadline {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
