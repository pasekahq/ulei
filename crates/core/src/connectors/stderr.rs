//! The stderr pipe of a run, shared by `systemd-run`, the launcher and the connector: the unit's
//! start and result as `systemd-run` reports them, the launcher's first line, and the tail of
//! the connector's own text.

use std::mem;

use super::unit::{LANDLOCK, LAUNCHER_PREFIX, REFUSED};

/// Bytes of the connector's stderr a run record keeps.
const TAIL: usize = 2048;
/// A longer line is the connector's text, never a line of `systemd-run` or the launcher.
const MAX_LINE: usize = 8192;
/// `systemd-run` after its start job: `Running as unit: <unit>; invocation ID: <id>`.
const RUNNING: &str = "Running as unit: ";
/// `systemd-run --wait` once the unit is inactive, before its other summary lines.
const RESULT: &str = "Finished with result: ";

/// The launcher's first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Launcher {
    /// It executes the connector under this Landlock ABI, none under the owner's opt-out.
    Landlock(Option<u64>),
    /// It refused with this code and reason.
    Refused(String, String),
}

/// What a run's stderr said, gathered as it streams.
#[derive(Default)]
pub(super) struct Stderr {
    running: bool,
    launcher: Option<Launcher>,
    result: Option<String>,
    tail: Vec<u8>,
    line: Vec<u8>,
    /// Inside a line already passed to the tail as too long.
    long: bool,
    /// After the result line: only `systemd-run`'s summary follows.
    summary: bool,
}

/// The end of a run's stderr.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Seen {
    /// The unit started: `systemd-run` or the launcher spoke from it.
    pub started: bool,
    pub launcher: Option<Launcher>,
    /// The unit's `Result=`, such as `success`, `exit-code` or `timeout`.
    pub result: Option<String>,
    /// The last [`TAIL`] bytes of the connector's text from a UTF-8 boundary.
    pub tail: String,
}

impl Stderr {
    /// Takes the next bytes of the pipe.
    pub(super) fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let end = bytes
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |i| i + 1);
            let (part, rest) = bytes.split_at(end);
            bytes = rest;
            let newline = part.ends_with(b"\n");
            if self.long {
                self.text(part);
            } else {
                self.line.extend_from_slice(part);
                if self.line.len() > MAX_LINE {
                    let line = mem::take(&mut self.line);
                    self.text(&line);
                    self.long = true;
                } else if newline {
                    let line = mem::take(&mut self.line);
                    self.complete(&line);
                }
            }
            if newline {
                self.long = false;
            }
        }
    }

    /// What the pipe said once it closed.
    pub(super) fn end(mut self) -> Seen {
        let line = mem::take(&mut self.line);
        self.text(&line);
        let mut tail = &self.tail[self.tail.len().saturating_sub(TAIL)..];
        while tail.first().is_some_and(|b| b & 0xC0 == 0x80) {
            tail = &tail[1..];
        }
        Seen {
            started: self.running || self.launcher.is_some() || self.result.is_some(),
            launcher: self.launcher,
            result: self.result,
            tail: String::from_utf8_lossy(tail).into_owned(),
        }
    }

    fn complete(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\n');
        // The unit is gone when systemd-run reports its result, so the last such line is
        // systemd-run's own even if the connector printed one.
        if let Some((_, result)) = text.rsplit_once(RESULT) {
            self.result = Some(result.trim().to_owned());
            self.summary = true;
            return;
        }
        if self.summary {
            return;
        }
        if !self.running && text.contains(RUNNING) {
            self.running = true;
            return;
        }
        if self.launcher.is_none()
            && let Some(said) = text.strip_prefix(LAUNCHER_PREFIX)
        {
            // The launcher speaks before the connector exists, so its first line is its own.
            let launcher = launcher(said);
            let keep = matches!(launcher, Some(Launcher::Refused(..)));
            self.launcher = launcher;
            if !keep {
                return;
            }
        }
        self.text(line);
    }

    fn text(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > 2 * TAIL {
            self.tail.drain(..self.tail.len() - TAIL);
        }
    }
}

/// The launcher line after its prefix.
fn launcher(said: &str) -> Option<Launcher> {
    if let Some(refusal) = said.strip_prefix(REFUSED) {
        let (code, reason) = refusal.split_once(": ")?;
        return Some(Launcher::Refused(code.to_owned(), reason.to_owned()));
    }
    match said.strip_prefix(LANDLOCK)? {
        "off" => Some(Launcher::Landlock(None)),
        abi => abi
            .strip_prefix("abi ")?
            .parse()
            .ok()
            .map(|abi| Launcher::Landlock(Some(abi))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::unit::refusal_line;
    use super::*;

    fn seen(chunks: &[&[u8]]) -> Seen {
        let mut stderr = Stderr::default();
        for chunk in chunks {
            stderr.feed(chunk);
        }
        stderr.end()
    }

    #[test]
    fn systemd_run_and_launcher_lines_are_read_and_kept_out_of_the_tail() {
        let seen = seen(&[
            b"ulei-connectors: exec landlock abi 8\nconnector says",
            b" hi\nRunning as unit: ulei-connector-notes-1.service; invocation ID: 0f\n",
            b"ulei-connectors: exec landlock abi 9\nmore\n",
            b"Finished with result: timeout\nMain processes terminated with: code=killed/status=TERM\n",
            b"Service runtime: 2.001s\n",
        ]);
        assert_eq!(
            seen,
            Seen {
                started: true,
                launcher: Some(Launcher::Landlock(Some(8))),
                result: Some("timeout".to_owned()),
                tail: "connector says hi\nulei-connectors: exec landlock abi 9\nmore\n".to_owned(),
            }
        );
    }

    #[test]
    fn a_launcher_refusal_stays_in_the_tail_and_off_is_no_abi() {
        let line = refusal_line("landlock", "landlock unavailable (ENOSYS); required");
        let refused = seen(&[line.as_bytes(), b"Finished with result: exit-code\n"]);
        assert_eq!(
            refused.launcher,
            Some(Launcher::Refused(
                "landlock".to_owned(),
                "landlock unavailable (ENOSYS); required".to_owned()
            ))
        );
        assert_eq!(refused.tail, line);
        assert_eq!(refused.result.as_deref(), Some("exit-code"));
        let off = seen(&[b"ulei-connectors: exec landlock off\n"]);
        assert_eq!(off.launcher, Some(Launcher::Landlock(None)));
        assert!(off.started);
        assert_eq!(off.tail, "");
    }

    #[test]
    fn without_systemd_run_or_launcher_lines_the_unit_did_not_start() {
        let seen = seen(&[b"Failed to start transient service unit: Unit ulei-connector-notes-1.service was already loaded or has a fragment file.\n"]);
        assert!(!seen.started);
        assert_eq!(seen.launcher, None);
        assert!(
            seen.tail
                .starts_with("Failed to start transient service unit: ")
        );
    }

    #[test]
    fn the_last_result_line_wins_over_one_the_connector_printed() {
        let seen = seen(&[
            b"Running as unit: u.service
Finished with result: success
connector text
",
            b"Finished with result: timeout
CPU time consumed: 3ms
",
        ]);
        assert!(seen.started);
        assert_eq!(seen.result.as_deref(), Some("timeout"));
        assert_eq!(seen.tail, "");
    }

    #[test]
    fn long_lines_and_a_flood_keep_only_the_last_bytes_of_text() {
        let long = vec![b'x'; MAX_LINE + 10];
        let mut flood = Vec::new();
        for n in 0..1000 {
            flood.extend_from_slice(format!("line {n}\n").as_bytes());
        }
        let flooded = seen(&[&long, b"Finished with result: success\n", &flood]);
        assert_eq!(flooded.result, None);
        assert_eq!(flooded.tail.len(), TAIL);
        assert!(flooded.tail.ends_with("line 999\n"));
        let wide = "\u{e9}".repeat(TAIL);
        let cut = seen(&[wide.as_bytes()]);
        assert!(!cut.tail.is_empty() && cut.tail.chars().all(|c| c == '\u{e9}'));
    }
}
