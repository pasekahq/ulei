//! The audit line of every call (ADR-0073 section 5) and the peer's systemd unit.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Opens the audit file for one append, creating it with mode 0600.
///
/// # Errors
/// The open's error.
pub fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

/// Appends `line` and a newline to `file` in one write.
///
/// # Errors
/// The write's error, a short write included.
pub fn append(file: &mut File, line: &str) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.flush()
}

/// The fields of one call (`serde_json` writes them sorted by key).
#[derive(Debug, Default)]
pub struct Record {
    fields: Map<String, Value>,
}

impl Record {
    /// A record stamped now for a peer.
    pub fn new(pid: i32, uid: u32, gid: u32) -> Self {
        let mut fields = Map::new();
        fields.insert("time".into(), rfc3339(SystemTime::now()).into());
        fields.insert("pid".into(), pid.into());
        fields.insert("uid".into(), uid.into());
        fields.insert("gid".into(), gid.into());
        fields.insert(
            "unit".into(),
            peer_unit(pid).map_or(Value::Null, Value::from),
        );
        for key in ["op", "id", "args", "decision", "exit", "signal"] {
            fields.insert(key.into(), Value::Null);
        }
        fields.insert("timed_out".into(), false.into());
        fields.insert("duration_ms".into(), 0.into());
        fields.insert("stdout_bytes".into(), 0.into());
        fields.insert("stderr_bytes".into(), 0.into());
        fields.insert("truncated".into(), false.into());
        Self { fields }
    }

    /// Sets `key`.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.fields.insert(key.into(), value.into());
    }

    /// The JSON line, without its newline.
    pub fn line(&self) -> String {
        Value::Object(self.fields.clone()).to_string()
    }
}

/// The last `.service` or `.scope` of the peer's cgroup v2 path, best effort.
pub fn peer_unit(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    unit_of_cgroup(&text)
}

/// The unit of a `/proc/<pid>/cgroup` text.
pub fn unit_of_cgroup(text: &str) -> Option<String> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    path.rsplit('/')
        .find(|c| {
            std::path::Path::new(c)
                .extension()
                .is_some_and(|e| e == "service" || e == "scope")
        })
        .map(str::to_owned)
}

/// `t` as RFC 3339 UTC with milliseconds, `2026-11-25T12:00:00.000Z`.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil from days (H. Hinnant), proleptic Gregorian.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2026-11-25T12:34:56.789Z
        let t = UNIX_EPOCH + Duration::from_millis(1_795_610_096_789);
        assert_eq!(rfc3339(t), "2026-11-25T12:34:56.789Z");
        // A leap day.
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339(t), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn unit_from_cgroup() {
        assert_eq!(
            unit_of_cgroup("0::/system.slice/ulei-engine.service\n").as_deref(),
            Some("ulei-engine.service")
        );
        assert_eq!(
            unit_of_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/ulei-tg.service\n"
            )
            .as_deref(),
            Some("ulei-tg.service")
        );
        assert_eq!(
            unit_of_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n").as_deref(),
            Some("session-3.scope")
        );
        assert_eq!(unit_of_cgroup("0::/\n"), None);
        assert_eq!(unit_of_cgroup(""), None);
    }
}
