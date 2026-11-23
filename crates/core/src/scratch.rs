//! Scratch directories of the crate's unit tests.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

/// A fresh directory under the system temp dir, removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("ulei-core-unit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
