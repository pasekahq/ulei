//! The runner's state of one connector in `<connectors.state>/<name>`: `runner.json` with its
//! cursor, last start and `not_before`, replaced through a rename, and the lock a run holds.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::json_fault;

/// The state file's name in `<connectors.state>/<name>`.
pub(super) const STATE: &str = "runner.json";
const STATE_TEMP: &str = "runner.json.tmp";
/// The file whose exclusive `flock` a run holds, in any process.
const LOCK: &str = "lock";

/// What `runner.json` holds; times are Unix seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<f64>,
}

/// The state in `dir`; the default when the file is absent.
///
/// # Errors
/// The file cannot be read or is not the state; the text names it.
pub(super) fn read(dir: &Path) -> Result<State, String> {
    let path = dir.join(STATE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<State>(&bytes)
            .map_err(|e| format!("{}: {}", path.display(), json_fault(&e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Replaces the state in `dir`, created 0700, with `state`.
///
/// # Errors
/// Creating, writing, syncing or renaming failed.
pub(super) fn write(dir: &Path, state: &State) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut bytes = serde_json::to_vec(state)?;
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

/// The run lock of the connector whose state is `dir`, created 0700; none while another run
/// holds it. Dropping the file releases it.
///
/// # Errors
/// The directory or lock file cannot be created or locked.
pub(super) fn lock(dir: &Path) -> io::Result<Option<File>> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Drops the stored cursor of `name` under `state` and keeps its schedule; false when it has
/// no state file.
///
/// # Errors
/// `name` is not a connector name, or the file cannot be read or replaced; the text says which.
pub fn reset(state: &Path, name: &str) -> Result<bool, String> {
    if !super::manifest::is_name(name) {
        return Err(format!("{name:?} is not a connector name"));
    }
    let dir = state.join(name);
    match fs::symlink_metadata(dir.join(STATE)) {
        Ok(_) => {
            let kept = State {
                cursor: None,
                ..read(&dir)?
            };
            write(&dir, &kept)
                .map(|()| true)
                .map_err(|e| format!("{}: {e}", dir.join(STATE).display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", dir.join(STATE).display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    #[test]
    fn a_second_lock_of_one_connector_waits_for_the_first_to_drop() {
        let scratch = Scratch::new("lock");
        let dir = scratch.0.join("test");
        let held = lock(&dir).expect("lock").expect("free");
        assert!(lock(&dir).expect("lock").is_none());
        assert!(lock(&scratch.0.join("other")).expect("lock").is_some());
        drop(held);
        assert!(lock(&dir).expect("lock").is_some());
    }

    #[test]
    fn the_schedule_survives_a_rewrite_and_reset_drops_only_the_cursor() {
        let scratch = Scratch::new("state");
        let dir = scratch.0.join("test");
        assert_eq!(read(&dir), Ok(State::default()));
        let state = State {
            cursor: Some("7".to_owned()),
            last_start: Some(1_700_000_000.25),
            not_before: Some(1_700_000_090.5),
        };
        write(&dir, &state).expect("write");
        assert_eq!(read(&dir), Ok(state.clone()));
        assert_eq!(reset(&scratch.0, "test"), Ok(true));
        assert_eq!(
            read(&dir),
            Ok(State {
                cursor: None,
                ..state
            })
        );
        fs::write(dir.join(STATE), "{\"cursor\":\"3\"}\n").expect("cursor only");
        assert_eq!(read(&dir).expect("read").cursor.as_deref(), Some("3"));
    }
}
