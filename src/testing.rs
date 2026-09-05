//! Fixture helper shared by the unit tests: builds a throwaway jobs directory.

use std::path::PathBuf;

pub struct Fixture(pub PathBuf);

impl Fixture {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "savras-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fixture(dir)
    }

    pub fn job(self, short: &str, json: &str) -> Self {
        let dir = self.0.join(short);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), json).unwrap();
        self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
