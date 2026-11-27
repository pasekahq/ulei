//! The transient unit of a run (ADR-0056 sections 5.1, 5.2 and 5.5): the `systemd-run` argv with
//! its properties, and the launcher `ulei-connectors exec` inside it with its checks, Landlock and
//! environment.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::sandbox;

/// Exit status of the launcher when it refuses; `systemd-run --wait` passes it through.
pub const REFUSED_EXIT: u8 = 125;
/// Start of each line the launcher writes on stderr.
pub(super) const LAUNCHER_PREFIX: &str = "ulei-connectors: exec ";
/// After [`LAUNCHER_PREFIX`]: a refusal, followed by `<code>: <reason>`.
pub(super) const REFUSED: &str = "refused ";
/// After [`LAUNCHER_PREFIX`]: the Landlock the connector runs under, `abi <n>` or `off`, written
/// just before the launcher executes it; `abi <n>` is followed by [`CONNECT`] when it limits
/// TCP connect ports.
pub(super) const LANDLOCK: &str = "landlock ";
/// After `abi <n>`: the ports TCP connect is limited to, comma-separated, or `none`.
pub(super) const CONNECT: &str = " connect ";
/// The refusal code of a failed `execve` of the connector, written after the Landlock line.
pub(super) const EXEC_FAILED: &str = "exec_failed";
/// Seconds a run may take when its manifest names no `timeout`.
pub(super) const DEFAULT_TIMEOUT: u64 = 300;
/// The variables the launcher keeps: the fds systemd passes stay valid across `execve`.
const KEPT: [&str; 3] = ["LISTEN_FDS", "LISTEN_FDNAMES", "LISTEN_PID"];

/// The unit of a run of `name` started at Unix second `at`.
pub(super) fn unit_name(name: &str, at: u64) -> String {
    format!("ulei-connector-{name}-{at}.service")
}

/// The `systemd-run` options and properties of a run in `unit` that may last `timeout`
/// seconds, gets the broker at `broker` as fd 3 and, when `direct`, its own `AF_INET` and
/// `AF_INET6` sockets, in the background class (ADR-0103); the launcher's argv follows them
/// after `--`.
pub(super) fn systemd_run(unit: &str, timeout: u64, broker: &Path, direct: bool) -> Vec<String> {
    let unit = unit.strip_suffix(".service").unwrap_or(unit);
    // Without `--quiet`, systemd-run reports on stderr that it started the unit and the unit's
    // result once it ended.
    let mut args: Vec<String> = ["--user", "--wait", "--pipe", "--collect"]
        .map(str::to_owned)
        .to_vec();
    args.push(format!("--unit={unit}"));
    args.push("--slice=background.slice".to_owned());
    // An empty allow list is `none` on the bus; systemd-run passes the word `none` on as a
    // family name, which the manager refuses.
    let families = if direct { "AF_INET AF_INET6" } else { "" };
    let properties = [
        "NoNewPrivileges=yes".to_owned(),
        "SystemCallFilter=@system-service landlock_create_ruleset landlock_add_rule landlock_restrict_self".to_owned(),
        "SystemCallErrorNumber=EPERM".to_owned(),
        "SystemCallArchitectures=native".to_owned(),
        format!("RestrictAddressFamilies={families}"),
        format!("OpenFile={}:broker", broker.display()),
        "RestrictNamespaces=yes".to_owned(),
        "LockPersonality=yes".to_owned(),
        "RestrictRealtime=yes".to_owned(),
        "RestrictSUIDSGID=yes".to_owned(),
        "UMask=0077".to_owned(),
        "MemoryMax=256M".to_owned(),
        "TasksMax=64".to_owned(),
        "IOSchedulingClass=idle".to_owned(),
        "Nice=10".to_owned(),
        "TimeoutStopSec=10s".to_owned(),
    ];
    args.extend(properties.iter().map(|p| format!("--property={p}")));
    args.push(format!("--property=RuntimeMaxSec={timeout}s"));
    args
}

/// The launcher's argv after `exec`: `NAME DATA_DIR DIR [--key FILE] [--read PATH]...
/// [--write PATH]... [--landlock-off] [--direct [--port N]...] -- PROGRAM [ARG...]`, paths and
/// ports only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The connector's name.
    pub name: String,
    /// `<connectors.state>/<name>/data`, its working, home and only writable directory.
    pub data_dir: PathBuf,
    /// Its directory in `connectors.dir`.
    pub dir: PathBuf,
    /// Its key file, readable alone.
    pub key: Option<PathBuf>,
    /// The owner's `read` grants.
    pub read: Vec<PathBuf>,
    /// The owner's `write` grants.
    pub write: Vec<PathBuf>,
    /// False under the owner's `landlock = "off"`.
    pub landlock: bool,
    /// Under the owner's `network = "direct"`: the ports Landlock limits TCP connect to from
    /// ABI 4.
    pub direct: Option<Vec<u16>>,
    /// The connector's program.
    pub program: OsString,
    /// The program's arguments.
    pub args: Vec<OsString>,
}

impl Launch {
    /// The launch `args` describe, none for a malformed argv or name.
    pub fn parse(args: &[OsString]) -> Option<Self> {
        let [name, data_dir, dir, rest @ ..] = args else {
            return None;
        };
        let name = name.to_str().filter(|n| super::is_name(n))?.to_owned();
        let (mut key, mut read, mut write, mut landlock) = (None, Vec::new(), Vec::new(), true);
        let (mut direct, mut ports) = (false, Vec::new());
        let mut rest = rest.iter();
        loop {
            match rest.next()?.to_str()? {
                "--" => break,
                "--landlock-off" => landlock = false,
                "--direct" => direct = true,
                "--port" => ports.push(rest.next()?.to_str()?.parse().ok().filter(|&p| p > 0)?),
                "--key" if key.is_none() => key = Some(rest.next()?.into()),
                "--read" => read.push(rest.next()?.into()),
                "--write" => write.push(rest.next()?.into()),
                _ => return None,
            }
        }
        if !direct && !ports.is_empty() {
            return None;
        }
        Some(Self {
            name,
            data_dir: data_dir.into(),
            dir: dir.into(),
            key,
            read,
            write,
            landlock,
            direct: direct.then_some(ports),
            program: rest.next()?.clone(),
            args: rest.cloned().collect(),
        })
    }

    /// The argv [`Launch::parse`] reads back.
    pub(super) fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![
            self.name.clone().into(),
            self.data_dir.clone().into(),
            self.dir.clone().into(),
        ];
        for (flag, path) in (self.key.iter().map(|p| ("--key", p)))
            .chain(self.read.iter().map(|p| ("--read", p)))
            .chain(self.write.iter().map(|p| ("--write", p)))
        {
            argv.push(flag.into());
            argv.push(path.clone().into());
        }
        if !self.landlock {
            argv.push("--landlock-off".into());
        }
        if let Some(ports) = &self.direct {
            argv.push("--direct".into());
            for port in ports {
                argv.push("--port".into());
                argv.push(port.to_string().into());
            }
        }
        argv.push("--".into());
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// The launcher: checks that this process runs in a run unit of its connector under a seccomp
/// filter, restricts itself with Landlock unless the owner opted out, says which on stderr and
/// replaces itself with the connector in its data directory under the connector's environment.
/// It returns only on refusal, with the refusal's code and reason.
pub fn exec(launch: &Launch) -> (&'static str, String) {
    let read = |path: &str| fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    if let Err(reason) = read("/proc/self/status").and_then(|text| check_status(&text)) {
        return ("seccomp", reason);
    }
    if let Err(reason) =
        read("/proc/self/cgroup").and_then(|text| check_cgroup(&text, &launch.name))
    {
        return ("unit", reason);
    }
    let data_dir = &launch.data_dir;
    if !data_dir.is_absolute() {
        return (
            "data_dir",
            format!("data_dir {} is not absolute", data_dir.display()),
        );
    }
    let landlock = if launch.landlock {
        match sandbox::confine(launch) {
            Ok((abi, None)) => format!("abi {abi}"),
            Ok((abi, Some(ports))) => format!("abi {abi}{CONNECT}{}", ports_text(&ports)),
            Err(reason) => return ("landlock", reason),
        }
    } else {
        "off".to_owned()
    };
    // One write, so systemd-run's own lines on the same pipe cannot split it.
    let line = format!("{LAUNCHER_PREFIX}{LANDLOCK}{landlock}\n");
    if let Err(e) = std::io::stderr().write_all(line.as_bytes()) {
        return ("stderr", e.to_string());
    }
    let failed = Command::new(&launch.program)
        .args(&launch.args)
        .env_clear()
        .envs(environment(std::env::vars_os(), data_dir))
        .current_dir(data_dir)
        .exec();
    (
        EXEC_FAILED,
        format!("{}: {failed}", Path::new(&launch.program).display()),
    )
}

/// `ports` comma-separated, `none` when empty.
fn ports_text(ports: &[u16]) -> String {
    if ports.is_empty() {
        return "none".to_owned();
    }
    let shown: Vec<String> = ports.iter().map(u16::to_string).collect();
    shown.join(",")
}

/// The launcher's stderr line for a refusal, with its newline.
pub fn refusal_line(code: &str, reason: &str) -> String {
    format!("{LAUNCHER_PREFIX}{REFUSED}{code}: {reason}\n")
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
        let broker = Path::new("/run/c/ulei-connector-notes-1700000000.sock");
        let args = systemd_run(&unit, 7, broker, false);
        assert_eq!(
            args[..5],
            [
                "--user",
                "--wait",
                "--pipe",
                "--collect",
                "--unit=ulei-connector-notes-1700000000"
            ]
        );
        assert!(args.iter().all(|a| a.starts_with("--")));
        assert_eq!(
            args.last().map(String::as_str),
            Some("--property=RuntimeMaxSec=7s")
        );
        assert_eq!(args[5], "--slice=background.slice");
        for property in [
            "NoNewPrivileges=yes",
            "RestrictAddressFamilies=",
            "SystemCallErrorNumber=EPERM",
            "OpenFile=/run/c/ulei-connector-notes-1700000000.sock:broker",
            "IOSchedulingClass=idle",
            "Nice=10",
        ] {
            assert!(
                args.contains(&format!("--property={property}")),
                "{property}"
            );
        }
        let direct = systemd_run(&unit, 7, broker, true);
        let families: Vec<&String> = direct
            .iter()
            .filter(|a| a.starts_with("--property=RestrictAddressFamilies="))
            .collect();
        assert_eq!(
            families,
            ["--property=RestrictAddressFamilies=AF_INET AF_INET6"]
        );
        assert!(direct.contains(&format!("--property=OpenFile={}:broker", broker.display())));
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
            ("HOME", "/srv/account"),
            ("XDG_RUNTIME_DIR", "/run/user/1001"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1001/bus"),
            ("LISTEN_FDS", "1"),
            ("LISTEN_FDNAMES", "broker"),
            ("LISTEN_PID", "42"),
            ("PATH", "/srv/account/bin:/usr/bin"),
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
    fn the_runner_argv_of_a_launch_parses_back_to_it() {
        let launch = Launch {
            name: "notes".to_owned(),
            data_dir: "/state/notes/data".into(),
            dir: "/connectors/notes".into(),
            key: Some("/secrets/notes.env".into()),
            read: vec!["/shared/a".into(), "/shared/b".into()],
            write: vec!["/outbox".into()],
            landlock: false,
            direct: Some(vec![443, 8443]),
            program: "/connectors/notes/bin/run".into(),
            args: vec!["--".into(), "--read".into()],
        };
        let argv = launch.argv();
        assert_eq!(Launch::parse(&argv), Some(launch.clone()));
        let closed = Launch {
            direct: Some(Vec::new()),
            ..launch.clone()
        };
        assert_eq!(Launch::parse(&closed.argv()), Some(closed));
        let bare = Launch {
            key: None,
            read: Vec::new(),
            write: Vec::new(),
            landlock: true,
            direct: None,
            ..launch
        };
        let argv: Vec<OsString> = bare.argv();
        assert_eq!(
            argv,
            [
                "notes",
                "/state/notes/data",
                "/connectors/notes",
                "--",
                "/connectors/notes/bin/run",
                "--",
                "--read"
            ]
            .map(OsString::from)
        );
        assert_eq!(Launch::parse(&argv), Some(bare));
    }

    #[test]
    fn a_malformed_launch_argv_is_none() {
        for args in [
            &["Bad", "/d", "/c", "--", "/p"][..],
            &["notes", "/d", "/c", "/p"],
            &["notes", "/d", "/c", "--"],
            &[
                "notes", "/d", "/c", "--key", "/k", "--key", "/k2", "--", "/p",
            ],
            &["notes", "/d", "/c", "--read"],
            &["notes", "/d", "/c", "--other", "--", "/p"],
            &["notes", "/d", "/c", "--port", "443", "--", "/p"],
            &["notes", "/d", "/c", "--direct", "--port", "0", "--", "/p"],
            &[
                "notes", "/d", "/c", "--direct", "--port", "65536", "--", "/p",
            ],
            &["notes", "/d"],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(Launch::parse(&args), None, "{args:?}");
        }
    }
}
