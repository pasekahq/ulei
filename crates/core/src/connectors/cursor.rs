//! The runner's state of one connector, `<connectors.state>/<name>/runner.json`: its cursor,
//! replaced through a rename.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::json_fault;

/// The state file's name in `<connectors.state>/<name>`.
pub(super) const STATE: &str = "runner.json";
const STATE_TEMP: &str = "runner.json.tmp";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    cursor: Option<String>,
}

/// The stored cursor in `dir`; none when the file is absent or holds null.
///
/// # Errors
/// The file cannot be read or is not the state; the text names it.
pub(super) fn read(dir: &Path) -> Result<Option<String>, String> {
    let path = dir.join(STATE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<State>(&bytes)
            .map(|state| state.cursor)
            .map_err(|e| format!("{}: {}", path.display(), json_fault(&e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replaces the state in `dir`, created 0700, with `cursor`.
///
/// # Errors
/// Creating, writing, syncing or renaming failed.
pub(super) fn write(dir: &Path, cursor: Option<&str>) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut bytes = serde_json::to_vec(&State {
        cursor: cursor.map(str::to_owned),
    })?;
    bytes.push(b'\n');
    let temp = dir.join(STATE_TEMP);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    // Synced so a crash leaves the old or the new file; a lost rename only repeats items, which
    // C10 answers `duplicate`.
    file.sync_all()?;
    fs::rename(&temp, dir.join(STATE))
}

/// Drops the stored cursor of `name` under `state`; false when it has no state file.
///
/// # Errors
/// `name` is not a connector name, or the file cannot be replaced; the text says which.
pub fn reset(state: &Path, name: &str) -> Result<bool, String> {
    if !super::manifest::is_name(name) {
        return Err(format!("{name:?} is not a connector name"));
    }
    let dir = state.join(name);
    match fs::symlink_metadata(dir.join(STATE)) {
        Ok(_) => write(&dir, None)
            .map(|()| true)
            .map_err(|e| format!("{}: {e}", dir.join(STATE).display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", dir.join(STATE).display())),
    }
}
