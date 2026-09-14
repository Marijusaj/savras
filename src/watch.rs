//! Watching the jobs directory, shared by both ways of running the panel.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecursiveMode, Watcher};

/// A live view of the jobs directory. Holds the watcher, since dropping it
/// stops the events.
pub struct Watch {
    _watcher: Option<notify::RecommendedWatcher>,
    rx: Receiver<()>,
    /// False when watching was unavailable and the caller is polling instead.
    pub live: bool,
}

impl Watch {
    pub fn start(jobs_dir: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let watcher = build(jobs_dir, tx);
        Watch {
            live: watcher.is_some(),
            _watcher: watcher,
            rx,
        }
    }

    /// Collapse a burst of events into a single "something changed". Claude
    /// Code rewrites state.json several times per update.
    pub fn changed(&self) -> bool {
        let mut any = false;
        loop {
            match self.rx.try_recv() {
                Ok(()) => any = true,
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return any,
            }
        }
    }
}

fn build(jobs_dir: &Path, tx: mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
    // Watching a directory that does not exist fails, so watch the parent and
    // the panel comes alive the moment Claude Code creates it.
    let target = if jobs_dir.exists() {
        jobs_dir.to_path_buf()
    } else {
        jobs_dir.parent()?.to_path_buf()
    };
    if !target.exists() {
        return None;
    }

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|event| is_a_change(&event.kind)) {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&target, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}

/// Whether an event means the jobs on disk may be different now.
///
/// **Looking at a file is not changing it.** inotify reports every open, and
/// the panel's own refresh opens every `state.json` it reads — so on Linux each
/// refresh announced another change, which refreshed again, and the panel
/// redrew thirty times a second for as long as it ran. macOS never reports an
/// open, which is why this only ever showed on a Linux runner, as a pane test
/// that waited for a quiet screen and never got one.
///
/// A close after writing is kept: it is the moment a writer finished, and the
/// most useful of all of them. Anything this does not recognise counts as a
/// change, because a refresh too many costs a read and one too few is a stale
/// row.
fn is_a_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    #[test]
    fn reading_is_not_a_change_and_writing_is() {
        assert!(!is_a_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_a_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_a_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));

        assert!(is_a_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_a_change(&EventKind::Create(CreateKind::File)));
        assert!(is_a_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_a_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_a_change(&EventKind::Any), "unknown counts as a change");
    }

    #[test]
    fn the_panel_reading_its_own_jobs_does_not_wake_it() {
        // The loop itself, end to end. On macOS this passed before the fix
        // too, since FSEvents reports no opens; on Linux it is the regression.
        let dir = std::env::temp_dir().join(format!("savras-watch-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("aaa")).unwrap();
        std::fs::write(dir.join("aaa/state.json"), r#"{"state":"working"}"#).unwrap();

        let watch = Watch::start(&dir);
        // Let anything from creating the fixture arrive, then forget it.
        std::thread::sleep(std::time::Duration::from_millis(300));
        watch.changed();

        for _ in 0..5 {
            let _ = std::fs::read_to_string(dir.join("aaa/state.json")).unwrap();
            let _ = std::fs::read_dir(&dir).unwrap().count();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!watch.changed(), "reading the jobs was taken for a change");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
