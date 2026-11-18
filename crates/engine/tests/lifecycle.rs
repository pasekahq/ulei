//! The lifecycle guard of `terminal`: a command, script or shell payload that would restart, stop
//! or kill the engine's own unit is refused before it runs, any other unit passes and runs, and
//! the unit comes from the setting or, under systemd, from the process's cgroup.
//!
//! No command here reaches a real `systemctl`, `kill` or `pkill`: every call runs with stand-ins
//! first on `PATH` and a `kill` function, which only log their arguments.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use fake_gateway::Scratch;
use serde_json::{Map, Value, json};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Context, Interrupt, Output, Registry, ServiceUnit, UnitError};

const OWN: &str = "ulei-engine-secretary.service";

fn blocked_text(unit: &str) -> String {
    format!(
        "Blocked: command or referenced script cannot restart or stop the {unit} service from \
         inside the service process. The service would kill this command before it could \
         complete (SIGTERM propagates to child processes). Run `systemctl restart {unit}` from a \
         separate shell outside the running service."
    )
}

/// A scratch directory with logging stand-ins for the lifecycle commands in `bin`.
struct Host {
    dir: Scratch,
}

impl Host {
    fn new() -> Self {
        let dir = Scratch::new();
        fs::create_dir(dir.path("bin")).unwrap();
        for tool in ["systemctl", "kill", "pkill", "pgrep", "pidof"] {
            let path = dir.path("bin").join(tool);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho \"{tool} $*\" >> {}\n",
                    dir.path("ran").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path(name)
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.path(name), text).unwrap();
    }

    fn executable(&self, name: &str, text: &str) {
        self.write(name, text);
        fs::set_permissions(self.path(name), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `command` between the stand-ins and a closing `done` line, guarded for `service`.
    fn call(&self, service: Option<&str>, command: &str, workdir: Option<&str>) -> Output {
        let ran = self.path("ran");
        let full = format!(
            "PATH={}:$PATH\nkill() {{ echo \"kill $*\" >> {ran}; }}\n{command}\necho done >> {ran}",
            self.path("bin").display(),
            ran = ran.display()
        );
        let mut input = Map::new();
        input.insert("command".into(), json!(full));
        if let Some(workdir) = workdir {
            input.insert("workdir".into(), json!(workdir));
        }
        let interrupt = Interrupt::new();
        let registry = Registry::builtin(
            &self.path("spill"),
            Redactor { enabled: true },
            service.map(|s| ServiceUnit::new(s).unwrap()),
        );
        registry.execute(
            "terminal",
            &input,
            Context {
                cwd: &self.dir.0,
                interrupt: &interrupt,
            },
        )
    }

    /// The lines the stand-ins and the closing line logged, then clears the log.
    fn take_ran(&self) -> Vec<String> {
        let ran = fs::read_to_string(self.path("ran")).unwrap_or_default();
        let _ = fs::remove_file(self.path("ran"));
        ran.lines().map(str::to_owned).collect()
    }

    /// The call under [`OWN`] is refused with the reference shape and ran nothing.
    fn assert_blocked(&self, command: &str, workdir: Option<&str>) {
        let out = self.call(Some(OWN), command, workdir);
        let want = Value::from(blocked_text(OWN)).to_string();
        assert_eq!(
            out,
            Output {
                content: format!(
                    "{{\"output\": \"\", \"exit_code\": 1, \"error\": {want}, \"status\": \"error\"}}"
                ),
                is_error: true,
            },
            "{command}"
        );
        assert_eq!(self.take_ran(), Vec::<String>::new(), "{command} ran");
    }

    /// The call under [`OWN`] ran to its end; the lines the stand-ins logged before it.
    fn assert_runs(&self, command: &str, workdir: Option<&str>) -> Vec<String> {
        let out = self.call(Some(OWN), command, workdir);
        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert!(body.get("status").is_none(), "{command}: {}", out.content);
        let mut ran = self.take_ran();
        assert_eq!(
            ran.pop().as_deref(),
            Some("done"),
            "{command}: {}",
            out.content
        );
        ran
    }
}

#[test]
fn systemctl_restart_stop_and_start_of_the_own_unit_are_blocked() {
    let host = Host::new();
    for command in [
        "systemctl stop ulei-engine-secretary",
        "systemctl --user restart ulei-engine-secretary.service",
        "systemctl --user --no-block start ulei-engine-secretary",
        "SYSTEMD_PAGER= nice systemctl Restart ULEI-ENGINE-SECRETARY",
        "systemctl --user \\\n  restart \\\n  ulei-engine-secretary",
        "systemctl --user restart ulei-engine-other ulei-engine-secretary",
        "systemctl --user stop ulei-engine-other && systemctl --user restart ulei-engine-secretary",
        "systemctl --user stop ulei-engine-other; echo ulei-engine-secretary",
        "systemctl --user kill ulei-engine-secretary",
        "true systemctl stop ulei-engine-secretary",
    ] {
        host.assert_blocked(command, None);
    }
}

#[test]
fn kill_and_pkill_naming_the_own_unit_or_the_executable_are_blocked() {
    let host = Host::new();
    for command in [
        "pkill -f ulei-engine-secretary",
        "pkill ulei-engine",
        "pkill -f 'ulei-engine run --session x'",
        "kill -TERM ulei-engine-secretary",
        "pkill -f ulei-engine-other; pkill -f ulei-engine",
        "pkill -f other; echo ulei-engine-secretary",
        "pkill -f \\\n  ulei-engine",
    ] {
        host.assert_blocked(command, None);
    }
}

#[test]
fn a_glob_regex_substitution_or_pipe_that_may_reach_the_own_unit_is_blocked() {
    let host = Host::new();
    for command in [
        "systemctl --user restart 'ulei-engine-secretary*'",
        "systemctl --user restart 'ulei-engine-$X'",
        "systemctl --user restart 'ulei-*'",
        "systemctl --user stop '*secret?ry.service'",
        "systemctl --user restart \"$UNIT\"",
        "systemctl --user restart $(cat unit.txt)",
        "systemctl --user restart ulei-engine-{other,secretary}",
        "echo ulei-engine-secretary | xargs systemctl --user restart",
        "ls units | xargs systemctl --user restart",
        "pkill -f 'ulei.*'",
        "pkill engine",
        "pkill -f '['",
        "pkill -f \"$NAME\"",
        "pkill -f 'ulei-engine-other|ulei-engine-secretary'",
        "pkill -f ulei-engine-other; echo $(date)",
        "kill $(pidof ulei-engine)",
        "kill $(pgrep -f 'ulei.*')",
        "pgrep -f ulei-engine | xargs kill",
        "pgrep -f engine | xargs -n1 kill",
    ] {
        host.assert_blocked(command, None);
    }
}

#[test]
fn the_same_commands_naming_another_unit_pass_and_run() {
    let host = Host::new();
    let cases: [(&str, &[&str]); 12] = [
        (
            "systemctl --user stop ulei-engine-other",
            &["systemctl --user stop ulei-engine-other"],
        ),
        (
            "systemctl --user restart ulei-engine-other.service ulei-engine-peer",
            &["systemctl --user restart ulei-engine-other.service ulei-engine-peer"],
        ),
        (
            "systemctl --user start ulei-engine",
            &["systemctl --user start ulei-engine"],
        ),
        (
            "systemctl --user restart qb-gateway.service",
            &["systemctl --user restart qb-gateway.service"],
        ),
        (
            "systemctl --user status ulei-engine-secretary",
            &["systemctl --user status ulei-engine-secretary"],
        ),
        (
            "systemctl --user restart 'nginx*'",
            &["systemctl --user restart nginx*"],
        ),
        (
            "pkill -f ulei-engine-other",
            &["pkill -f ulei-engine-other"],
        ),
        (
            "pkill -f 'python3 server.py'",
            &["pkill -f python3 server.py"],
        ),
        ("kill -TERM 12345", &["kill -TERM 12345"]),
        ("echo 42 > other.pid; kill $(cat other.pid)", &["kill 42"]),
        ("pgrep -f sleep | xargs kill", &["pgrep -f sleep", "kill "]),
        (
            "bash -c 'systemctl stop ulei-engine-other'",
            &["systemctl stop ulei-engine-other"],
        ),
    ];
    for (command, want) in cases {
        assert_eq!(host.assert_runs(command, None), want, "{command}");
    }
}

#[test]
fn a_lifecycle_shape_inside_a_data_sink_argument_runs_unless_it_can_execute() {
    let host = Host::new();
    host.write(
        "notes.txt",
        "systemctl restart ulei-engine-secretary\nsystemctl stop ulei-engine-other\n",
    );
    host.assert_runs(
        "grep -c 'systemctl restart ulei-engine-secretary' notes.txt",
        None,
    );
    host.assert_runs(
        "grep -e 'pkill -f ulei-engine' -e 'systemctl stop ulei-engine-secretary' notes.txt",
        None,
    );
    let ran = host.assert_runs(
        "grep 'systemctl stop ulei-engine-other' notes.txt | sh",
        None,
    );
    assert_eq!(ran, ["systemctl stop ulei-engine-other"]);
    host.assert_blocked(
        "grep 'systemctl restart ulei-engine-secretary' notes.txt | sh",
        None,
    );
    host.assert_blocked(
        "grep \"$(systemctl restart ulei-engine-secretary)\" notes.txt",
        None,
    );
    host.assert_blocked(
        "grep x notes.txt; systemctl restart ulei-engine-secretary",
        None,
    );
}

#[test]
fn shell_payloads_and_referenced_scripts_are_scanned() {
    let host = Host::new();
    let own = "systemctl --user stop ulei-engine-secretary\n";
    let other = "systemctl --user stop ulei-engine-other\n";
    host.executable("own.sh", own);
    host.executable("other.sh", other);

    host.assert_blocked("bash -c 'systemctl stop ulei-engine-secretary'", None);
    host.assert_blocked("sh -c \"pkill -f ulei-engine\"", None);
    for runner in ["sh", "bash -e", "bash -o errexit --", "source", "./"] {
        let space = if runner.ends_with('/') { "" } else { " " };
        host.assert_blocked(&format!("{runner}{space}own.sh"), None);
        let ran = host.assert_runs(&format!("{runner}{space}other.sh"), None);
        assert_eq!(ran, ["systemctl --user stop ulei-engine-other"], "{runner}");
    }

    host.write("outer.sh", "echo outer\nbash inner.sh\n");
    host.write("inner.sh", own);
    host.assert_blocked("bash outer.sh", None);
    host.write("inner.sh", other);
    host.assert_runs("bash outer.sh", None);
}

#[test]
fn scripts_are_followed_eight_levels_deep_and_deeper_nesting_is_blocked() {
    let host = Host::new();
    let chain = |levels: usize| {
        for level in 1..levels {
            host.write(
                &format!("s{level}.sh"),
                &format!("bash s{}.sh\n", level + 1),
            );
        }
        host.write(&format!("s{levels}.sh"), "echo deepest\n");
    };
    chain(7);
    host.assert_runs("bash s1.sh", None);
    chain(8);
    host.assert_blocked("bash s1.sh", None);
}

#[test]
fn a_script_over_one_mebibyte_is_blocked_unread_and_one_at_the_limit_is_scanned() {
    let host = Host::new();
    let filler = |size: usize| {
        let line = "# filler line of a large script\n";
        let mut text = line.repeat(size / line.len());
        text.push_str(&"#".repeat(size - text.len()));
        text
    };
    host.write("big.sh", &filler(1024 * 1024));
    host.assert_runs("bash big.sh", None);
    host.write("big.sh", &filler(1024 * 1024 + 1));
    host.assert_blocked("bash big.sh", None);
    let mut own = "systemctl stop ulei-engine-secretary\n".to_owned();
    own.push_str(&filler(1024 * 1024 - own.len()));
    host.write("big.sh", &own);
    host.assert_blocked("bash big.sh", None);
}

#[test]
fn binaries_and_directories_are_not_scanned_and_other_special_files_are_blocked() {
    let host = Host::new();
    let mut elf = b"\x7fELF\x02\x01\x01\x00".to_vec();
    elf.extend_from_slice(b"systemctl stop ulei-engine-secretary\n");
    fs::write(host.path("tool.bin"), &elf).unwrap();
    host.assert_runs("bash tool.bin", None);

    let mut nul = b"#!/bin/sh\n\x00".to_vec();
    nul.extend_from_slice(b"systemctl stop ulei-engine-secretary\n");
    fs::write(host.path("nul.sh"), &nul).unwrap();
    host.assert_runs("bash nul.sh", None);

    // An executable magic without a NUL byte is read again by the run and scanned.
    host.write("mz.sh", "MZ=1\nsystemctl stop ulei-engine-secretary\n");
    host.assert_blocked("bash mz.sh", None);

    fs::create_dir(host.path("scripts.d")).unwrap();
    host.assert_runs("source scripts.d", None);

    let status = Command::new("mkfifo")
        .arg(host.path("pipe.sh"))
        .status()
        .unwrap();
    assert!(status.success());
    host.assert_blocked("bash pipe.sh", None);
}

#[test]
fn the_guard_resolves_scripts_against_the_run_working_directory_before_the_workdir_check() {
    let host = Host::new();
    fs::create_dir(host.path("sub")).unwrap();
    fs::write(
        host.path("sub/restart.sh"),
        "systemctl stop ulei-engine-secretary\n",
    )
    .unwrap();
    host.write("restart.sh", "echo harmless\n");
    host.assert_blocked("bash restart.sh", Some("sub"));
    host.assert_runs("bash restart.sh", None);
    host.assert_blocked("systemctl stop ulei-engine-secretary", Some("a;b"));
}

#[test]
fn without_a_unit_the_guard_is_off() {
    let host = Host::new();
    let out = host.call(None, "true systemctl stop ulei-engine-secretary", None);
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(host.take_ran(), ["done"]);
}

fn discover(
    setting: Option<&str>,
    under_systemd: bool,
    cgroup: io::Result<&str>,
) -> Result<Option<String>, UnitError> {
    ServiceUnit::discover(setting, under_systemd, || cgroup.map(str::to_owned))
        .map(|unit| unit.map(|u| u.name()))
}

#[test]
fn the_unit_is_the_setting_else_under_systemd_the_service_of_the_cgroup() {
    let cases = [
        (
            "0::/system.slice/ulei-engine.service\n",
            "ulei-engine.service",
        ),
        (
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/ulei-engine-secretary.service\n",
            "ulei-engine-secretary.service",
        ),
        (
            "0::/system.slice/system-ulei\\x2dengine.slice/ulei-engine@secretary.service/payload\n",
            "ulei-engine@secretary.service",
        ),
        (
            "12:pids:/system.slice/a.service\n1:name=systemd:/system.slice/legacy.service\n0::/\n",
            "legacy.service",
        ),
    ];
    for (cgroup, want) in cases {
        assert_eq!(
            discover(None, true, Ok(cgroup)).unwrap().as_deref(),
            Some(want)
        );
    }
    let unread = || -> io::Result<String> { panic!("the cgroup file was read") };
    let set = ServiceUnit::discover(Some("custom"), true, unread).unwrap();
    assert_eq!(set.map(|u| u.name()).as_deref(), Some("custom.service"));
    assert!(
        ServiceUnit::discover(None, false, unread)
            .unwrap()
            .is_none()
    );
}

#[test]
fn under_systemd_without_a_unit_the_engine_does_not_start() {
    let error = discover(
        None,
        true,
        Ok("0::/user.slice/user-1000.slice/session-2.scope\n"),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "running under systemd (INVOCATION_ID is set) but /proc/self/cgroup names no service \
         unit; set engine.service_unit (ULEI_ENGINE_SERVICE_UNIT)"
    );
    let error = discover(None, true, Err(io::Error::from(io::ErrorKind::NotFound))).unwrap_err();
    assert!(
        error.to_string().starts_with(
            "running under systemd (INVOCATION_ID is set) but /proc/self/cgroup could not be read: "
        ),
        "{error}"
    );
    let error = discover(Some("bad unit"), false, Ok("")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "\"bad unit\" is not a service unit name; set engine.service_unit \
         (ULEI_ENGINE_SERVICE_UNIT)"
    );
}

#[test]
fn the_blocked_text_names_the_configured_unit() {
    let host = Host::new();
    let out = host.call(
        Some("ulei-engine@other"),
        "systemctl stop ulei-engine@other",
        None,
    );
    let body: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(body["error"], blocked_text("ulei-engine@other.service"));
    assert!(!Path::new(&host.path("ran")).exists());
}
