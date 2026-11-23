//! The stderr pipe of a run, shared by `systemd-run`, the launcher and the connector: the unit's
//! start and result as `systemd-run` reports them, the launcher's first line and its failed
//! `execve` right after it, and the tail of the connector's own text.

use std::mem;

use super::unit::{CONNECT, EXEC_FAILED, LANDLOCK, LAUNCHER_PREFIX, REFUSED};

/// Bytes of the connector's stderr a run record keeps.
const TAIL: usize = 2048;
/// A longer line is the connector's text, never a line of `systemd-run` or the launcher, but
/// for `systemd-run`'s result line ending it after an unterminated connector tail; the reader
/// keeps this many of its last bytes for that line.
const MAX_LINE: usize = 8192;
/// `systemd-run` after its start job: `Running as unit: <unit>; invocation ID: <id>`.
const RUNNING: &str = "Running as unit: ";
/// `systemd-run --wait` once the unit is inactive, before its other summary lines.
const RESULT: &str = "Finished with result: ";

/// The launcher's first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Launcher {
    /// It executes the connector under this Landlock ABI, none under the owner's opt-out, and
    /// with TCP connect limited to these ports, none when not limited.
    Landlock(Option<u64>, Option<Vec<u16>>),
    /// It refused with this code and reason.
    Refused(String, String),
}

/// What a run's stderr said, gathered as it streams.
#[derive(Default)]
pub(super) struct Stderr {
    running: bool,
    launcher: Option<Launcher>,
    /// The next line may be the launcher's failed `execve`.
    after_landlock: bool,
    exec_failed: Option<String>,
    result: Option<String>,
    tail: Vec<u8>,
    /// The current line, or the last [`MAX_LINE`] bytes of a longer one.
    line: Vec<u8>,
    /// The current line is longer than [`MAX_LINE`]; its first bytes went to the tail.
    long: bool,
}

/// The end of a run's stderr.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Seen {
    /// The unit started: `systemd-run` or the launcher spoke from it.
    pub started: bool,
    pub launcher: Option<Launcher>,
    /// The reason of the launcher's failed `execve` after its Landlock line.
    pub exec_failed: Option<String>,
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
            self.line.extend_from_slice(part);
            if self.line.len() > MAX_LINE {
                let older: Vec<u8> = self.line.drain(..self.line.len() - MAX_LINE).collect();
                self.text(&older);
                self.long = true;
            }
            if part.ends_with(b"\n") {
                let line = mem::take(&mut self.line);
                if mem::take(&mut self.long) {
                    self.complete_long(&line);
                } else {
                    self.complete(&line);
                }
            }
        }
    }

    /// Whether the unit started, as far as the pipe said yet.
    pub(super) fn started(&self) -> bool {
        self.running || self.launcher.is_some() || self.result.is_some()
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
            started: self.started(),
            launcher: self.launcher,
            exec_failed: self.exec_failed,
            result: self.result,
            tail: String::from_utf8_lossy(tail).into_owned(),
        }
    }

    fn complete(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\n');
        // The unit is gone when systemd-run reports its result, so the last such line is
        // systemd-run's own even if the connector printed one; what precedes it on the line is
        // the connector's unterminated last text.
        if let Some((before, result)) = text.rsplit_once(RESULT) {
            self.text(before.as_bytes());
            self.result = Some(result.trim().to_owned());
            return;
        }
        // After the result line only systemd-run's summary follows.
        if self.result.is_some() {
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
            let refused = matches!(launcher, Some(Launcher::Refused(..)));
            self.after_landlock = matches!(launcher, Some(Launcher::Landlock(..)));
            self.launcher = launcher;
            if !refused {
                return;
            }
        } else if mem::take(&mut self.after_landlock)
            && let Some(Launcher::Refused(code, reason)) =
                text.strip_prefix(LAUNCHER_PREFIX).and_then(launcher)
            && code == EXEC_FAILED
        {
            // Only the launcher's failed execve or the connector's first line follows the
            // Landlock line; a connector naming this code mislabels only its own run.
            self.exec_failed = Some(reason);
        }
        self.text(line);
    }

    /// The end of a line longer than [`MAX_LINE`]: the connector's text, which only
    /// `systemd-run`'s result line can end; that line wins over any result line before it.
    fn complete_long(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        match text.rsplit_once(RESULT) {
            Some((before, result)) => {
                self.text(before.as_bytes());
                self.result = Some(result.trim().to_owned());
            }
            None if self.result.is_none() => self.text(line),
            None => {}
        }
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
    let said = said.strip_prefix(LANDLOCK)?;
    if said == "off" {
        return Some(Launcher::Landlock(None, None));
    }
    let said = said.strip_prefix("abi ")?;
    let (abi, connect) = match said.split_once(CONNECT) {
        None => (said, None),
        Some((abi, "none")) => (abi, Some(Vec::new())),
        Some((abi, ports)) => (
            abi,
            Some(
                ports
                    .split(',')
                    .map(|p| p.parse().ok())
                    .collect::<Option<Vec<u16>>>()?,
            ),
        ),
    };
    Some(Launcher::Landlock(Some(abi.parse().ok()?), connect))
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
                launcher: Some(Launcher::Landlock(Some(8), None)),
                exec_failed: None,
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
        assert_eq!(off.launcher, Some(Launcher::Landlock(None, None)));
        assert!(off.started);
        assert_eq!(off.tail, "");
    }

    #[test]
    fn a_failed_execve_counts_only_as_the_line_right_after_the_landlock_line() {
        let failed = refusal_line(
            "exec_failed",
            "/srv/c/bin/run: Permission denied (os error 13)",
        );
        let seen_after = seen(&[
            b"ulei-connectors: exec landlock abi 8\n",
            b"Running as unit: u.service; invocation ID: 0f\n",
            failed.as_bytes(),
            b"Finished with result: exit-code\n",
        ]);
        assert_eq!(seen_after.launcher, Some(Launcher::Landlock(Some(8), None)));
        assert_eq!(
            seen_after.exec_failed.as_deref(),
            Some("/srv/c/bin/run: Permission denied (os error 13)")
        );
        assert_eq!(seen_after.tail, failed);
        let later = seen(&[
            b"ulei-connectors: exec landlock off\nconnector text\n",
            failed.as_bytes(),
        ]);
        assert_eq!(later.exec_failed, None);
        for code in ["landlock", "seccomp"] {
            let forged = refusal_line(code, "forged");
            let after = seen(&[b"ulei-connectors: exec landlock abi 8\n", forged.as_bytes()]);
            assert_eq!(after.exec_failed, None, "{code}");
            assert_eq!(
                after.launcher,
                Some(Launcher::Landlock(Some(8), None)),
                "{code}"
            );
        }
    }

    #[test]
    fn the_landlock_line_names_the_connect_ports_it_limits() {
        for (line, want) in [
            (
                "ulei-connectors: exec landlock abi 4 connect 443,8443\n",
                Launcher::Landlock(Some(4), Some(vec![443, 8443])),
            ),
            (
                "ulei-connectors: exec landlock abi 6 connect none\n",
                Launcher::Landlock(Some(6), Some(Vec::new())),
            ),
            (
                "ulei-connectors: exec landlock abi 3\n",
                Launcher::Landlock(Some(3), None),
            ),
        ] {
            assert_eq!(seen(&[line.as_bytes()]).launcher, Some(want), "{line}");
        }
        for broken in [
            "ulei-connectors: exec landlock abi 4 connect 443,x\n",
            "ulei-connectors: exec landlock abi 4 connect \n",
            "ulei-connectors: exec landlock abi 4 connect 70000\n",
        ] {
            assert_eq!(seen(&[broken.as_bytes()]).launcher, None, "{broken}");
        }
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
        let flooded = seen(&[&long, b" Finished with result: success\n", &flood]);
        assert_eq!(flooded.result.as_deref(), Some("success"));
        assert_eq!(flooded.tail.len(), TAIL);
        assert!(
            flooded.tail.ends_with("xxx "),
            "{}",
            &flooded.tail[TAIL - 8..]
        );
        let texted = seen(&[&flood, &long, b"\nafter\n"]);
        assert_eq!(texted.result, None);
        assert_eq!(texted.tail.len(), TAIL);
        assert!(texted.tail.ends_with("xxx\nafter\n"));
        let wide = "\u{e9}".repeat(TAIL);
        let cut = seen(&[wide.as_bytes()]);
        assert!(!cut.tail.is_empty() && cut.tail.chars().all(|c| c == '\u{e9}'));
    }

    #[test]
    fn an_unterminated_connector_tail_does_not_hide_the_unit_result() {
        let short = seen(&[
            b"Running as unit: u.service; invocation ID: 0f\n",
            b"connector without newline",
            b"Finished with result: timeout\nService runtime: 2.001s\n",
        ]);
        assert_eq!(short.result.as_deref(), Some("timeout"));
        assert_eq!(short.tail, "connector without newline");
        let long = vec![b'y'; 3 * MAX_LINE + 5];
        let longer = seen(&[
            b"Running as unit: u.service; invocation ID: 0f\n",
            &long,
            b"Finished with result: timeout\nMain processes terminated with: code=killed\n",
        ]);
        assert_eq!(longer.result.as_deref(), Some("timeout"));
        assert_eq!(longer.tail, "y".repeat(TAIL));
    }

    #[test]
    fn a_result_line_the_connector_printed_does_not_relabel_the_unit_result_after_a_long_tail() {
        let long = vec![b'z'; 2 * MAX_LINE];
        let spoofed = seen(&[
            b"Running as unit: u.service; invocation ID: 0f\n",
            b"Finished with result: success\n",
            &long,
            b"Finished with result: timeout\nService runtime: 2.001s\n",
        ]);
        assert_eq!(spoofed.result.as_deref(), Some("timeout"));
    }
}
