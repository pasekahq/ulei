//! The night's state directory (ADR-0078 section 5): the cursor of the history walk in
//! `cursor.json`, replaced whole through a temporary file, `fsync` and `rename`, and one report
//! of JSON lines per run in `runs/`.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The cursor's file in the state directory.
pub const CURSOR: &str = "cursor.json";
/// The directory of the run reports in the state directory.
pub const RUNS: &str = "runs";
/// The version of the cursor's format.
const VERSION: u64 = 1;

/// Where the walk of one archive source stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    /// Every entry with a `seq` up to this one is done.
    pub after: u64,
    /// The last `seq` of the window being walked, read again on a resume; `None` between windows.
    pub window_end: Option<u64>,
    /// The sessions and parts of the window already sent.
    pub done: Vec<String>,
    /// The sessions and parts of the window that failed, with the number of runs they failed in.
    pub failed: Vec<(String, u32)>,
    /// Closed windows whose failed sessions and parts are tried again on later runs.
    pub retry: Vec<Retry>,
}

/// A closed window of a source with the sessions and parts that failed in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retry {
    /// The window holds the entries after this `seq`.
    pub after: u64,
    /// The last `seq` of the window.
    pub end: u64,
    /// Each failed session or part with the number of runs it failed in.
    pub failed: Vec<(String, u32)>,
}

impl Source {
    /// Whether `key` of the current window is done.
    #[must_use]
    pub fn is_done(&self, key: &str) -> bool {
        self.done.iter().any(|d| d == key)
    }

    /// The number of runs `key` failed in.
    #[must_use]
    pub fn failures(&self, key: &str) -> u32 {
        self.failed
            .iter()
            .find(|(k, _)| k == key)
            .map_or(0, |(_, n)| *n)
    }

    /// Counts one more failed run of `key`; the new count.
    pub fn fail(&mut self, key: &str) -> u32 {
        if let Some((_, n)) = self.failed.iter_mut().find(|(k, _)| k == key) {
            *n += 1;
            return *n;
        }
        self.failed.push((key.to_owned(), 1));
        1
    }

    /// Starts the window ending at `end`.
    pub fn open(&mut self, end: u64) {
        self.window_end = Some(end);
        self.done.clear();
        self.failed.clear();
    }

    /// Ends the current window: everything up to its end is done, but for its failed sessions
    /// and parts not given up, which move to [`Self::retry`].
    pub fn close(&mut self) {
        if let Some(end) = self.window_end.take() {
            let failed: Vec<(String, u32)> = self
                .failed
                .iter()
                .filter(|(key, _)| !self.is_done(key))
                .cloned()
                .collect();
            if !failed.is_empty() {
                self.retry.push(Retry {
                    after: self.after,
                    end,
                    failed,
                });
            }
            self.after = end;
        }
        self.done.clear();
        self.failed.clear();
    }

    /// Records one more try of `key` of the closed window ending at `end`: dropped when `over`
    /// (done or given up), else one more failed run; a window with no key left is dropped.
    pub fn retried(&mut self, end: u64, key: &str, over: bool) {
        let Some(at) = self.retry.iter().position(|r| r.end == end) else {
            return;
        };
        let failed = &mut self.retry[at].failed;
        if over {
            failed.retain(|(k, _)| k != key);
        } else if let Some((_, n)) = failed.iter_mut().find(|(k, _)| k == key) {
            *n += 1;
        }
        if failed.is_empty() {
            self.retry.remove(at);
        }
    }

    /// The sessions and parts waiting for a later run: those of closed windows and the failed
    /// ones of the current window.
    #[must_use]
    pub fn waiting(&self) -> u64 {
        let open = self.failed.iter().filter(|(k, _)| !self.is_done(k)).count();
        let closed: usize = self.retry.iter().map(|r| r.failed.len()).sum();
        (open + closed) as u64
    }
}

/// The cursor of every source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Each source by name, in the order first walked.
    pub sources: Vec<(String, Source)>,
}

/// A cursor file that is not the night's format.
fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{CURSOR}: {what}"))
}

/// The `[key, count]` pairs of a failed list.
fn pairs(value: &Value) -> io::Result<Vec<(String, u32)>> {
    value
        .as_array()
        .ok_or_else(|| invalid("failed is no list"))?
        .iter()
        .map(|v| {
            let key = v.get(0).and_then(Value::as_str);
            let n = v.get(1).and_then(Value::as_u64).map(u32::try_from);
            match (key, n) {
                (Some(key), Some(Ok(n))) => Ok((key.to_owned(), n)),
                _ => Err(invalid("a failed entry is not [key, count]")),
            }
        })
        .collect()
}

impl Cursor {
    /// The state of `name`, a fresh one when the source is new.
    pub fn source(&mut self, name: &str) -> &mut Source {
        let at = if let Some(at) = self.sources.iter().position(|(n, _)| n == name) {
            at
        } else {
            self.sources.push((name.to_owned(), Source::default()));
            self.sources.len() - 1
        };
        &mut self.sources[at].1
    }

    /// The state of `name` as stored, without adding it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// The cursor as JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let sources: serde_json::Map<String, Value> = self
            .sources
            .iter()
            .map(|(name, s)| {
                let pairs = |failed: &[(String, u32)]| -> Vec<Value> {
                    failed.iter().map(|(k, n)| json!([k, n])).collect()
                };
                let retry: Vec<Value> = s
                    .retry
                    .iter()
                    .map(|r| json!({"after": r.after, "end": r.end, "failed": pairs(&r.failed)}))
                    .collect();
                (
                    name.clone(),
                    json!({
                        "after": s.after,
                        "window_end": s.window_end,
                        "done": s.done,
                        "failed": pairs(&s.failed),
                        "retry": retry,
                    }),
                )
            })
            .collect();
        json!({"version": VERSION, "sources": sources})
    }

    /// The cursor of `value`.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidData`] when it is not the night's format.
    pub fn from_json(value: &Value) -> io::Result<Self> {
        if value.get("version").and_then(Value::as_u64) != Some(VERSION) {
            return Err(invalid("unknown version"));
        }
        let sources = value
            .get("sources")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("no sources"))?;
        let mut cursor = Self::default();
        for (name, s) in sources {
            let texts = |key: &str| -> io::Result<Vec<String>> {
                s.get(key)
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("a source lacks a list"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| invalid("a list holds no text"))
                    })
                    .collect()
            };
            let failed = pairs(
                s.get("failed")
                    .ok_or_else(|| invalid("a source lacks failed"))?,
            )?;
            // A cursor written before retries were kept has none.
            let retry = match s.get("retry") {
                None => Vec::new(),
                Some(list) => list
                    .as_array()
                    .ok_or_else(|| invalid("retry is no list"))?
                    .iter()
                    .map(|r| {
                        let seq = |key: &str| {
                            r.get(key)
                                .and_then(Value::as_u64)
                                .ok_or_else(|| invalid("a retry lacks its window"))
                        };
                        Ok(Retry {
                            after: seq("after")?,
                            end: seq("end")?,
                            failed: pairs(
                                r.get("failed")
                                    .ok_or_else(|| invalid("a retry lacks failed"))?,
                            )?,
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()?,
            };
            let window_end = match s.get("window_end") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or_else(|| invalid("window_end"))?),
            };
            cursor.sources.push((
                name.clone(),
                Source {
                    after: s
                        .get("after")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| invalid("after"))?,
                    window_end,
                    done: texts("done")?,
                    failed,
                    retry,
                },
            ));
        }
        Ok(cursor)
    }

    /// The cursor stored in the state directory `dir`; a fresh one when there is none.
    ///
    /// # Errors
    /// Reading fails, or the file is not the night's format.
    pub fn load(dir: &Path) -> io::Result<Self> {
        match fs::read(dir.join(CURSOR)) {
            Ok(bytes) => {
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|_| invalid("not JSON"))?;
                Self::from_json(&value)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Replaces the cursor in `dir` whole: a 0600 temporary file written and synced, renamed
    /// over the cursor, and the directory synced.
    ///
    /// # Errors
    /// Any I/O failure; the stored cursor is then the old one.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let temporary = dir.join(format!(".{CURSOR}.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        let mut bytes = serde_json::to_vec(&self.to_json()).map_err(io::Error::from)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, dir.join(CURSOR))?;
        File::open(dir)?.sync_all()
    }
}

/// Creates `dir` and its `runs` directory owner-only when missing.
///
/// # Errors
/// Any I/O failure.
pub fn prepare(dir: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir.join(RUNS))
}

/// The report of one run: JSON lines appended as the run goes.
#[derive(Debug)]
pub struct Report {
    file: File,
    path: PathBuf,
}

impl Report {
    /// A new report `runs/<start>-<pid>.jsonl` in `dir`, mode 0600; `<start>-<pid>-<n>.jsonl`
    /// for the `n`th run of one process in one second.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn create(dir: &Path, start: i64, pid: u32) -> io::Result<Self> {
        let mut n = 1;
        loop {
            let name = if n == 1 {
                format!("{start}-{pid}.jsonl")
            } else {
                format!("{start}-{pid}-{n}.jsonl")
            };
            let path = dir.join(RUNS).join(name);
            match OpenOptions::new()
                .append(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok(Self { file, path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        }
    }

    /// The report's file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `value` as one line.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn line(&mut self, value: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value).map_err(io::Error::from)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("night-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        prepare(&dir).unwrap();
        dir
    }

    #[test]
    fn the_cursor_round_trips_through_its_file() {
        let dir = scratch("cursor");
        assert_eq!(Cursor::load(&dir).unwrap(), Cursor::default());
        let mut cursor = Cursor::default();
        let source = cursor.source("old-runtime");
        source.after = 12;
        source.open(40);
        source.done.push("s-1".to_owned());
        assert_eq!(source.fail("s-2"), 1);
        assert_eq!(source.fail("s-2"), 2);
        source.retry.push(Retry {
            after: 0,
            end: 12,
            failed: vec![("s-0".to_owned(), 2)],
        });
        cursor.source("dialogue").after = 3;
        cursor.save(&dir).unwrap();
        let loaded = Cursor::load(&dir).unwrap();
        assert_eq!(loaded.get("old-runtime"), cursor.get("old-runtime"));
        assert_eq!(loaded.get("dialogue"), cursor.get("dialogue"));
        assert!(!dir.join(".cursor.json.tmp").exists());
        let old =
            r#"{"version":1,"sources":{"a":{"after":5,"window_end":null,"done":[],"failed":[]}}}"#;
        fs::write(dir.join(CURSOR), old).unwrap();
        assert_eq!(Cursor::load(&dir).unwrap().get("a").unwrap().after, 5);
        fs::write(dir.join(CURSOR), b"{\"version\":9}").unwrap();
        assert!(Cursor::load(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closing_a_window_moves_after_to_its_end() {
        let mut source = Source::default();
        source.open(10);
        source.done.push("a".to_owned());
        source.fail("b");
        source.fail("c");
        source.done.push("c".to_owned()); // given up
        assert!(source.is_done("a"));
        assert_eq!(source.failures("b"), 1);
        assert_eq!(source.waiting(), 1);
        source.close();
        let retry = Retry {
            after: 0,
            end: 10,
            failed: vec![("b".to_owned(), 1)],
        };
        assert_eq!(
            source,
            Source {
                after: 10,
                retry: vec![retry],
                ..Source::default()
            }
        );
        assert_eq!(source.waiting(), 1);
        source.open(20);
        source.close();
        assert_eq!((source.after, source.retry.len()), (20, 1));
    }

    #[test]
    fn a_report_appends_lines() {
        let dir = scratch("report");
        let mut report = Report::create(&dir, 1_700_000_000, 7).unwrap();
        report.line(&json!({"type": "a"})).unwrap();
        report.line(&json!({"type": "total"})).unwrap();
        let text = fs::read_to_string(report.path()).unwrap();
        assert_eq!(text, "{\"type\":\"a\"}\n{\"type\":\"total\"}\n");
        let second = Report::create(&dir, 1_700_000_000, 7).unwrap();
        assert!(second.path().ends_with("1700000000-7-2.jsonl"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
