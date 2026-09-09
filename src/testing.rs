//! Fixture helper shared by the unit tests: builds a throwaway jobs directory.

use std::path::PathBuf;

/// The jobs directory. It sits one level inside the fixture's own root, the
/// way `~/.claude/jobs` sits inside `~/.claude`, so the files Savras reads
/// *beside* it — the pull request cache — have somewhere real to be.
pub struct Fixture(pub PathBuf);

impl Fixture {
    pub fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "savras-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let jobs = root.join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        Fixture(jobs)
    }

    pub fn job(self, short: &str, json: &str) -> Self {
        let dir = self.0.join(short);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), json).unwrap();
        self
    }

    /// What Claude Code last knew about some pull requests, written where it
    /// writes it: beside the jobs directory, not inside it.
    pub fn pr_cache(self, json: &str) -> Self {
        std::fs::write(self.root().join("gh-pr-status-cache.json"), json).unwrap();
        self
    }

    fn root(&self) -> PathBuf {
        self.0.parent().map(PathBuf::from).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root());
    }
}
