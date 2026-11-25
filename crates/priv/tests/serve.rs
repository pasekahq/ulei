//! `ulei-priv serve` as the test's own user on a temp policy, socket and audit file, driven by
//! the `ulei-priv` client and by raw requests (ADR-0073).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_ulei-priv");

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// A fresh directory 0700 under the temp dir, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ulei-priv-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("temp dir");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn policy_text(dir: &Path, allowed: u32) -> String {
    let touched = dir.join("touched");
    format!(
        r#"allowed_uids = [{allowed}]

[[command]]
id = "echo"
argv = ["/bin/echo", "hello"]

[[command]]
id = "echo-arg"
argv = ["/bin/echo", "value={{arg}}"]
arg = ["a", "b"]

[[command]]
id = "sleep"
argv = ["/bin/sleep", "30"]
timeout_s = 1

[[command]]
id = "slow"
argv = ["/bin/sleep", "2"]

[[command]]
id = "yes"
argv = ["/usr/bin/yes"]
max_output_bytes = 1000

[[command]]
id = "fail"
argv = ["/bin/ls", "/nonexistent-ulei-priv"]

[[command]]
id = "missing"
argv = ["/nonexistent/ulei-priv-program"]

[[command]]
id = "touch"
argv = ["/usr/bin/touch", "{touched}"]
"#,
        touched = touched.display()
    )
}

struct Daemon {
    dir: Dir,
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn socket(&self) -> PathBuf {
        self.dir.0.join("priv.sock")
    }

    fn audit(&self) -> PathBuf {
        self.dir.0.join("audit.jsonl")
    }

    fn start(allowed: u32) -> Self {
        let dir = Dir::new();
        let policy = dir.0.join("priv.toml");
        fs::write(&policy, policy_text(&dir.0, allowed)).expect("policy");
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o644)).expect("chmod");
        let mut child = serve(&dir.0, &policy).spawn().expect("spawn serve");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().expect("stderr");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.starts_with("ulei-priv: ready: ") {
                    let _ = tx.send(());
                }
                sink.lock().expect("log").push(line);
            }
        });
        if rx.recv_timeout(Duration::from_secs(10)).is_err() {
            let _ = child.kill();
            panic!("no ready line: {:?}", log.lock().expect("log"));
        }
        Self { dir, child, log }
    }

    fn client(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .output()
            .expect("client")
    }

    fn audit_lines(&self) -> Vec<Value> {
        fs::read_to_string(self.audit())
            .expect("audit file")
            .lines()
            .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
            .collect()
    }

    fn last_audit(&self) -> Value {
        self.audit_lines().pop().expect("an audit line")
    }

    /// Sends raw bytes and returns the reply header.
    fn raw(&self, bytes: &[u8]) -> Value {
        let mut s = UnixStream::connect(self.socket()).expect("connect");
        let _ = s.write_all(bytes);
        let mut reply = String::new();
        BufReader::new(s).read_line(&mut reply).expect("reply");
        serde_json::from_str(&reply).expect("reply JSON")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path, policy: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("serve")
        .arg("--policy")
        .arg(policy)
        .arg("--socket")
        .arg(dir.join("priv.sock"))
        .arg("--audit")
        .arg(dir.join("audit.jsonl"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_refused(out: &Output, reason: &str) {
    assert_eq!(out.status.code(), Some(77), "{}", stderr(out));
    assert_eq!(stderr(out), format!("ulei-priv: refused: {reason}\n"));
    assert_eq!(out.stdout.len(), 0);
}

#[test]
fn runs_a_listed_command_and_audits_it() {
    let d = Daemon::start(uid());
    let out = d.client(&["run", "echo"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello\n");
    let a = d.last_audit();
    assert_eq!(a["decision"], "ok");
    assert_eq!(a["op"], "run");
    assert_eq!(a["id"], "echo");
    assert_eq!(a["args"], serde_json::json!([]));
    assert_eq!(a["uid"], uid());
    assert_eq!(a["gid"], rustix::process::getgid().as_raw());
    assert!(a["pid"].as_i64().is_some_and(|p| p > 0));
    assert!(
        a.get("unit").is_some(),
        "unit key, null when the peer has none"
    );
    assert_eq!(a["exit"], 0);
    assert_eq!(a["signal"], Value::Null);
    assert_eq!(a["timed_out"], false);
    assert_eq!(a["stdout_bytes"], 6);
    assert_eq!(a["stderr_bytes"], 0);
    assert_eq!(a["truncated"], false);
    assert!(a["duration_ms"].as_u64().is_some());
    let time = a["time"].as_str().expect("time");
    assert!(
        time.len() == 24 && time.ends_with('Z') && time.as_bytes()[10] == b'T',
        "{time}"
    );
    let mode = fs::metadata(d.audit()).expect("audit").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let sock = fs::metadata(d.socket())
        .expect("socket")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(sock, 0o666);
    // The same line on stderr, for the journal.
    let line = a.to_string();
    let log = d.log.lock().expect("log").clone();
    assert!(
        log.iter().any(|l| l
            .strip_prefix("ulei-priv: audit ")
            .and_then(|j| serde_json::from_str::<Value>(j).ok())
            == Some(a.clone())),
        "{line} in {log:?}"
    );

    let out = d.client(&["run", "echo-arg", "b"]);
    assert_eq!(
        (out.status.code(), out.stdout.as_slice()),
        (Some(0), &b"value=b\n"[..])
    );
    assert_eq!(d.last_audit()["args"], serde_json::json!(["b"]));
    let out = d.client(&["run", "fail"]);
    assert_eq!(out.status.code(), Some(2), "the child's exit code");
    assert!(!out.stderr.is_empty(), "the child's stderr");
}

#[test]
fn refuses_ids_and_arguments_off_the_policy() {
    let d = Daemon::start(uid());
    for (args, reason) in [
        (&["run", "nope"][..], "unknown_command"),
        (&["run", "echo-arg", "c"], "bad_argument"),
        (&["run", "echo-arg"], "bad_argument"),
        (&["run", "echo-arg", "a", "b"], "bad_argument"),
        (&["run", "echo", "x"], "bad_argument"),
        (&["run", "echo-arg", "a; id"], "bad_argument"),
        (&["run", "missing"], "exec_failed"),
    ] {
        let out = d.client(args);
        assert_refused(&out, reason);
        let a = d.last_audit();
        assert_eq!(a["decision"], format!("refused:{reason}"), "{args:?}");
        assert_eq!(a["id"], args[1]);
        assert_eq!(a["exit"], Value::Null);
    }
    assert_eq!(d.audit_lines().len(), 7, "one line per call");
}

#[test]
fn lists_the_calls() {
    let d = Daemon::start(uid());
    let out = d.client(&["list"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "ulei-priv run echo");
    assert_eq!(
        lines[1..3],
        ["ulei-priv run echo-arg a", "ulei-priv run echo-arg b"]
    );
    assert_eq!(lines.len(), 9);
    let a = d.last_audit();
    assert_eq!(
        (a["op"].as_str(), a["decision"].as_str()),
        (Some("list"), Some("ok"))
    );
}

#[test]
fn kills_at_the_timeout_and_cuts_at_the_cap() {
    let d = Daemon::start(uid());
    let started = Instant::now();
    let out = d.client(&["run", "sleep"]);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(128 + 15), "{}", stderr(&out));
    assert!(stderr(&out).contains("ulei-priv: timed out"));
    let a = d.last_audit();
    assert_eq!(
        (a["timed_out"].as_bool(), a["signal"].as_i64()),
        (Some(true), Some(15))
    );
    assert!(a["duration_ms"].as_u64().is_some_and(|ms| ms >= 1000));

    let out = d.client(&["run", "yes"]);
    assert_eq!(out.stdout.len(), 1000);
    assert!(stderr(&out).contains("ulei-priv: output truncated"));
    assert_eq!(
        out.status.code(),
        Some(128 + 13),
        "SIGPIPE once the pipe is closed"
    );
    let a = d.last_audit();
    assert_eq!(
        (a["truncated"].as_bool(), a["stdout_bytes"].as_u64()),
        (Some(true), Some(1000))
    );
}

#[test]
fn one_command_at_a_time() {
    let d = Daemon::start(uid());
    let mut first = Command::new(BIN)
        .arg("--socket")
        .arg(d.socket())
        .args(["run", "slow"])
        .stdout(Stdio::null())
        .spawn()
        .expect("first");
    let until = Instant::now() + Duration::from_secs(5);
    // The first holds the lock once its child exists.
    while !fs::read_dir("/proc")
        .expect("proc")
        .flatten()
        .any(|e| fs::read(e.path().join("cmdline")).is_ok_and(|c| c == b"/bin/sleep\x002\x00"))
    {
        assert!(Instant::now() < until, "the first command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_refused(&d.client(&["run", "echo"]), "busy");
    assert_eq!(d.last_audit()["decision"], "refused:busy");
    assert_eq!(first.wait().expect("first").code(), Some(0));
    assert_eq!(d.client(&["run", "echo"]).status.code(), Some(0));
}

#[test]
fn refuses_oversized_malformed_and_crowded_requests() {
    let d = Daemon::start(uid());
    let big = vec![b'x'; 5000];
    assert_eq!(d.raw(&big)["reason"], "too_large");
    assert_eq!(d.last_audit()["decision"], "refused:too_large");
    for bad in [
        &b"hello\n"[..],
        b"{\"op\":\"run\",\"id\":\"echo\",\"env\":{}}\n",
        b"{\"op\":\"exec\"}\n",
    ] {
        assert_eq!(d.raw(bad)["reason"], "malformed");
        assert_eq!(d.last_audit()["decision"], "refused:malformed");
    }
    // Four connections that send nothing hold every slot; the fifth is busy at once.
    let held: Vec<UnixStream> = (0..4)
        .map(|_| UnixStream::connect(d.socket()).expect("connect"))
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    assert_eq!(d.raw(b"{\"op\":\"list\"}\n")["reason"], "busy");
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(held);
}

#[test]
fn refuses_a_uid_off_the_list() {
    let d = Daemon::start(uid() + 1);
    assert_refused(&d.client(&["run", "echo"]), "uid_not_allowed");
    assert_refused(&d.client(&["list"]), "uid_not_allowed");
    let a = d.last_audit();
    assert_eq!(a["decision"], "refused:uid_not_allowed");
    assert_eq!(a["uid"], uid());
    // Refused before anything is read: a silent peer holds no slot.
    let started = Instant::now();
    assert_eq!(d.raw(b"")["reason"], "uid_not_allowed");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(d.audit_lines().len(), 3);
}

#[test]
fn refuses_when_the_audit_cannot_be_written() {
    if uid() == 0 {
        return; // root writes a 0400 file.
    }
    let d = Daemon::start(uid());
    assert_eq!(d.client(&["run", "echo"]).status.code(), Some(0));
    fs::set_permissions(d.audit(), fs::Permissions::from_mode(0o400)).expect("chmod");
    assert_refused(&d.client(&["run", "touch"]), "audit_failed");
    assert!(!d.dir.0.join("touched").exists(), "the command did not run");
    assert_refused(&d.client(&["run", "nope"]), "audit_failed");
    assert_refused(&d.client(&["list"]), "audit_failed");
    fs::set_permissions(d.audit(), fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(d.client(&["run", "touch"]).status.code(), Some(0));
    assert!(d.dir.0.join("touched").exists());
}

#[test]
fn client_without_a_daemon_exits_69() {
    let dir = Dir::new();
    let out = Command::new(BIN)
        .arg("--socket")
        .arg(dir.0.join("none.sock"))
        .args(["run", "echo"])
        .output()
        .expect("client");
    assert_eq!(out.status.code(), Some(69));
    assert!(stderr(&out).starts_with("ulei-priv: daemon unavailable at "));
    let help = Command::new(BIN).arg("--help").output().expect("help");
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("ulei-priv run <id> [<arg>]"));
    assert_eq!(
        Command::new(BIN).output().expect("usage").status.code(),
        Some(2)
    );
}

#[test]
fn serve_refuses_a_bad_policy_or_socket() {
    let check = |text: &str, mode: u32, expect: &str| {
        let dir = Dir::new();
        let policy = dir.0.join("priv.toml");
        fs::write(&policy, text).expect("policy");
        fs::set_permissions(&policy, fs::Permissions::from_mode(mode)).expect("chmod");
        let mut child = serve(&dir.0, &policy).spawn().expect("serve");
        let mut err = String::new();
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut err)
            .expect("read");
        assert_eq!(child.wait().expect("wait").code(), Some(2), "{err}");
        assert!(err.contains(expect), "{expect} in {err}");
    };
    let good = policy_text(Path::new("/nonexistent"), uid());
    if uid() != 0 {
        check(&good, 0o664, "writable by group or others");
        check(&good, 0o646, "writable by group or others");
    }
    check(
        &good.replace("\"/bin/sleep\"", "\"sleep\""),
        0o644,
        "not an absolute path",
    );
    check(&good.replace("echo-arg", "<ULEI_USER>"), 0o644, "id");
    check("allowed_uids = [<ULEI_UID>]\n", 0o644, "line 1");
    let dir = Dir::new();
    let policy = dir.0.join("priv.toml");
    fs::write(&policy, &good).expect("policy");
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(dir.0.join("priv.sock"), "not a socket").expect("file");
    let out = serve(&dir.0, &policy).output().expect("serve");
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("exists and is not a socket"));
}

#[test]
fn shipped_example_parses_once_filled() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/priv/priv.example.toml");
    let text = fs::read_to_string(path).expect("example");
    assert!(
        ulei_priv::policy::parse(&text).is_err(),
        "placeholders refused"
    );
    let filled = text
        .replace("<ULEI_UID>", "1000")
        .replace("<ULEI_USER>", "someone");
    let p = ulei_priv::policy::parse(&filled).expect("the filled example parses");
    let ids: Vec<&str> = p.commands.iter().map(|c| c.id.as_str()).collect();
    for id in [
        "sshd-effective",
        "docker-ps",
        "ufw-status",
        "fail2ban-status",
        "fail2ban-sshd",
        "restart-engine",
        "restart-user-unit",
        "status-engine",
        "status-user-unit",
        "journal-engine",
        "journal-user-unit",
    ] {
        assert!(ids.contains(&id), "{id} in the example");
    }
    let docker = &p.commands[1];
    assert_eq!(docker.argv[3], "{{.Names}}\t{{.Status}}\t{{.Ports}}");
    for c in &p.commands {
        let program = Path::new(&c.argv[0]);
        let name = program.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            !["sh", "bash", "env", "sudo", "su"].contains(&name),
            "{}: {name}",
            c.id
        );
    }
    let (_, argv) = p
        .resolve("journal-user-unit", &["ulei-tg.service".into()])
        .expect("resolves");
    assert_eq!(
        argv[1..3],
        ["_UID=1000", "_SYSTEMD_USER_UNIT=ulei-tg.service"]
    );
}
