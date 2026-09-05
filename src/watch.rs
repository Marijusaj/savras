//! Watching the jobs directory, shared by both ways of running the panel.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

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
        if res.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&target, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}
