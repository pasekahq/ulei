//! Files a daemon under test wrote.

use std::fs;
use std::path::{Path, PathBuf};

/// The file in `dir` whose content is `data`.
pub(crate) fn saved(dir: &Path, data: &[u8]) -> PathBuf {
    fs::read_dir(dir)
        .expect("attachments dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| fs::read(path).expect("saved") == data)
        .expect("the saved file")
}
