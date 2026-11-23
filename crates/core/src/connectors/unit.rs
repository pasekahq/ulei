//! The transient unit of a run (ADR-0056 section 5.1): the `systemd-run` argv with its
//! properties, and the checks and environment of the launcher `ulei-connectors exec` inside it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Exit status of the launcher when it refuses; `systemd-run --wait` passes it through.
pub const REFUSED_EXIT: u8 = 125;
/// Start of the launcher's one refusal line on stderr, followed by `<code>: <reason>`.
const REFUSED_PREFIX: &str = "ulei-connectors: exec refused ";
/// Seconds a run may take when its manifest names no `timeout`.
pub(super) const DEFAULT_TIMEOUT: u64 = 300;
/// The variables the launcher keeps: the fds systemd passes stay valid across `execve`.
const KEPT: [&str; 3] = ["LISTEN_FDS", "LISTEN_FDNAMES", "LISTEN_PID"];

/// The unit of a run of `name` started at Unix second `at`.
pub(super) fn unit_name(name: &str, at: u64) -> String {
    format!("ulei-connector-{name}-{at}.service")
}

/// The `systemd-run` options and properties of a run in `unit` that may last `timeout`
/// seconds; the launcher's argv follows them after `--`.
pub(super) fn systemd_run(unit: &str, timeout: u64) -> Vec<String> {
    let unit = unit.strip_suffix(".service").unwrap_or(unit);
    let mut args: Vec<String> = ["--user", "--wait", "--pipe", "--collect", "--quiet"]
        .map(str::to_owned)
        .to_vec();
    args.push(format!("--unit={unit}"));
    let properties = [
        "NoNewPrivileges=yes",
        "SystemCallFilter=@system-service landlock_create_ruleset landlock_add_rule landlock_restrict_self",
        "SystemCallErrorNumber=EPERM",
        "SystemCallArchitectures=native",
        // An empty allow list is `none` on the bus; systemd-run passes the word `none` on as a
        // family name, which the manager refuses.
        "RestrictAddressFamilies=",
        "RestrictNamespaces=yes",
        "LockPersonality=yes",
        "RestrictRealtime=yes",
        "RestrictSUIDSGID=yes",
        "UMask=0077",
        "MemoryMax=256M",
        "TasksMax=64",
        "TimeoutStopSec=10s",
    ];
    args.extend(properties.iter().map(|p| format!("--property={p}")));
    args.push(format!("--property=RuntimeMaxSec={timeout}s"));
    args
}

/// The launcher: checks that this process runs in a run unit of `name` under a seccomp filter,
/// then replaces itself with `program` in `data_dir` under the connector's environment. It
/// returns only on refusal, with the refusal's code and reason.
pub fn exec(
    name: &str,
    data_dir: &Path,
    program: &OsStr,
    args: &[OsString],
) -> (&'static str, String) {
    let read = |path: &str| fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    if let Err(reason) = read("/proc/self/status").and_then(|text| check_status(&text)) {
        return ("seccomp", reason);
    }
    if let Err(reason) = read("/proc/self/cgroup").and_then(|text| check_cgroup(&text, name)) {
        return ("unit", reason);
    }
    if !data_dir.is_absolute() {
        return (
            "data_dir",
            format!("data_dir {} is not absolute", data_dir.display()),
        );
    }
    let failed = Command::new(program)
        .args(args)
        .env_clear()
        .envs(environment(std::env::vars_os(), data_dir))
        .current_dir(data_dir)
        .exec();
    (
        "exec_failed",
        format!("{}: {failed}", Path::new(program).display()),
    )
}

/// The launcher's stderr line for a refusal.
pub fn refusal_line(code: &str, reason: &str) -> String {
    format!("{REFUSED_PREFIX}{code}: {reason}")
}

/// The code and reason of the launcher's refusal when it is the last line of `stderr`.
pub(super) fn refusal(stderr: &str) -> Option<(String, String)> {
    let line = stderr.lines().next_back()?.strip_prefix(REFUSED_PREFIX)?;
    let (code, reason) = line.split_once(": ")?;
    Some((code.to_owned(), reason.to_owned()))
}

/// Checks the text of `/proc/self/status` for `NoNewPrivs: 1` and `Seccomp: 2`.
///
/// # Errors
/// Either field is missing or has another value; the text names what was seen.
fn check_status(status: &str) -> Result<(), String> {
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    let nnp = field("NoNewPrivs");
    let seccomp = field("Seccomp");
    if nnp.as_deref() == Some("1") && seccomp.as_deref() == Some("2") {
        return Ok(());
    }
    let shown = |value: Option<String>| value.unwrap_or_else(|| "missing".to_owned());
    Err(format!(
        "NoNewPrivs {}, Seccomp {} in /proc/self/status; a run needs NoNewPrivs 1 and Seccomp 2 (a filter) from its unit",
        shown(nnp),
        shown(seccomp)
    ))
}

/// Checks that the cgroup v2 line of `/proc/self/cgroup` ends in a run unit of `name`,
/// `ulei-connector-<name>-<unix time>.service`.
///
/// # Errors
/// No cgroup v2 line, or its last segment is not such a unit; the text names it.
fn check_cgroup(cgroup: &str, name: &str) -> Result<(), String> {
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("/proc/self/cgroup has no cgroup v2 line")?;
    let last = path.rsplit('/').next().unwrap_or(path);
    let at = last
        .strip_prefix("ulei-connector-")
        .and_then(|rest| rest.strip_prefix(name))
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".service"));
    match at {
        Some(at) if !at.is_empty() && at.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        _ => Err(format!(
            "cgroup {path} is not a run unit ulei-connector-{name}-<unix time>.service"
        )),
    }
}

/// The connector's whole environment: `HOME` and `TMPDIR` at `data_dir`, a fixed `PATH` and
/// `LANG`, and the `LISTEN_*` variables of `inherited`.
fn environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    data_dir: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), data_dir.into()),
        ("TMPDIR".into(), data_dir.into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ];
    env.extend(
        inherited
            .into_iter()
            .filter(|(key, _)| KEPT.iter().any(|kept| OsStr::new(kept) == key)),
    );
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_carries_the_manifest_timeout_and_its_name_without_suffix() {
        let unit = unit_name("notes", 1_700_000_000);
        assert_eq!(unit, "ulei-connector-notes-1700000000.service");
        let args = systemd_run(&unit, 7);
        assert_eq!(
            args[..6],
            [
                "--user",
                "--wait",
                "--pipe",
                "--collect",
                "--quiet",
                "--unit=ulei-connector-notes-1700000000"
            ]
        );
        assert!(args.iter().all(|a| a.starts_with("--")));
        assert_eq!(
            args.last().map(String::as_str),
            Some("--property=RuntimeMaxSec=7s")
        );
        for property in [
            "NoNewPrivileges=yes",
            "RestrictAddressFamilies=",
            "SystemCallErrorNumber=EPERM",
        ] {
            assert!(
                args.contains(&format!("--property={property}")),
                "{property}"
            );
        }
    }

    #[test]
    fn status_passes_only_with_no_new_privs_and_a_seccomp_filter() {
        let status = |nnp: &str, seccomp: &str| {
            format!("Name:\tx\nNoNewPrivs:\t{nnp}\nSeccomp:\t{seccomp}\nSeccomp_filters:\t1\n")
        };
        assert_eq!(check_status(&status("1", "2")), Ok(()));
        for (nnp, seccomp) in [("0", "2"), ("1", "0"), ("1", "1"), ("0", "0")] {
            let refused = check_status(&status(nnp, seccomp)).expect_err("refused");
            assert!(
                refused.starts_with(&format!("NoNewPrivs {nnp}, Seccomp {seccomp} ")),
                "{refused}"
            );
        }
        let refused = check_status("Name:\tx\nSeccomp:\t2\n").expect_err("refused");
        assert!(
            refused.starts_with("NoNewPrivs missing, Seccomp 2"),
            "{refused}"
        );
    }

    #[test]
    fn the_cgroup_must_be_a_run_unit_of_this_connector() {
        let base = "0::/user.slice/user-1001.slice/user@1001.service/app.slice/";
        assert_eq!(
            check_cgroup(
                &format!("{base}ulei-connector-test-1700000000.service\n"),
                "test"
            ),
            Ok(())
        );
        for last in [
            "ulei-connector-other-1700000000.service",
            "ulei-connector-test-2-1700000000.service",
            "ulei-connector-test-.service",
            "ulei-connector-test-1700000000.scope",
            "ulei-engine.service",
        ] {
            let refused = check_cgroup(&format!("{base}{last}\n"), "test").expect_err(last);
            assert!(refused.contains(last), "{refused}");
        }
        assert!(check_cgroup("1:name=systemd:/x\n", "test").is_err());
    }

    #[test]
    fn the_environment_is_cleared_but_for_the_fixed_variables_and_listen_fds() {
        let inherited = [
            ("HOME", "/home/owner"),
            ("XDG_RUNTIME_DIR", "/run/user/1001"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1001/bus"),
            ("LISTEN_FDS", "1"),
            ("LISTEN_FDNAMES", "broker"),
            ("LISTEN_PID", "42"),
            ("PATH", "/home/owner/bin:/usr/bin"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = environment(inherited, Path::new("/state/test/data"));
        let shown: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v)| (k.to_str().expect("key"), v.to_str().expect("value")))
            .collect();
        assert_eq!(
            shown,
            [
                ("HOME", "/state/test/data"),
                ("TMPDIR", "/state/test/data"),
                ("PATH", "/usr/bin:/bin"),
                ("LANG", "C.UTF-8"),
                ("LISTEN_FDS", "1"),
                ("LISTEN_FDNAMES", "broker"),
                ("LISTEN_PID", "42"),
            ]
        );
    }

    #[test]
    fn the_last_stderr_line_carries_a_launcher_refusal() {
        let line = refusal_line("seccomp", "NoNewPrivs 0, Seccomp 0");
        assert_eq!(
            refusal(&format!("noise\n{line}\n")),
            Some(("seccomp".to_owned(), "NoNewPrivs 0, Seccomp 0".to_owned()))
        );
        assert_eq!(refusal(&format!("{line}\nlater\n")), None);
        assert_eq!(refusal(""), None);
    }
}
