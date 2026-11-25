//! `ulei-priv serve` (ADR-0073 sections 2 to 5): the socket, the peer check, one command at a
//! time, the audit before every reply.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::audit::{self, Record};
use crate::child;
use crate::policy::{self, Policy};
use crate::wire::{self, Request};

/// Connections of allowed uids handled at once; one more is answered `busy` at once.
pub const MAX_CONNECTIONS: usize = 4;
/// How long a peer may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a peer may take to read its reply.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where `serve` reads and writes.
#[derive(Debug, Clone)]
pub struct Options {
    /// The policy file.
    pub policy: PathBuf,
    /// The socket it listens on.
    pub socket: PathBuf,
    /// The audit file it appends to.
    pub audit: PathBuf,
}

struct Shared {
    policy: Policy,
    audit: PathBuf,
    running: Mutex<()>,
    active: AtomicUsize,
}

/// Loads the policy, binds the socket and serves until killed.
///
/// # Errors
/// One line when the policy, the audit file or the socket is refused at start.
pub fn serve(opts: &Options) -> Result<(), String> {
    let owner = rustix::process::geteuid().as_raw();
    let policy = policy::load(&opts.policy, owner).map_err(|e| e.to_string())?;
    audit::open(&opts.audit).map_err(|e| format!("audit {}: {e}", opts.audit.display()))?;
    let listener = bind(&opts.socket)?;
    eprintln!(
        "ulei-priv: ready: {} with {} commands",
        opts.socket.display(),
        policy.commands.len()
    );
    let shared = Arc::new(Shared {
        policy,
        audit: opts.audit.clone(),
        running: Mutex::new(()),
        active: AtomicUsize::new(0),
    });
    loop {
        match listener.accept() {
            Ok((stream, _)) => dispatch(&shared, stream),
            Err(e) => {
                // EMFILE and the like: do not spin on them.
                eprintln!("ulei-priv: accept: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn bind(path: &Path) -> Result<UnixListener, String> {
    let shown = path.display();
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            fs::remove_file(path).map_err(|e| format!("socket {shown}: {e}"))?;
        }
        Ok(_) => return Err(format!("socket {shown}: exists and is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("socket {shown}: {e}")),
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("socket {shown}: {e}"))?;
    // Any local user may connect; SO_PEERCRED decides.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("socket {shown}: {e}"))?;
    Ok(listener)
}

/// Decrements the count of handled connections when the handler ends.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Refuses a uid off the policy and a connection past [`MAX_CONNECTIONS`] here, in the accept
/// thread, without reading the request: no local user outside `allowed_uids` can hold a slot.
fn dispatch(shared: &Arc<Shared>, mut stream: UnixStream) {
    let started = Instant::now();
    let (mut rec, uid) = record_for(&stream);
    let refusal = if !uid.is_some_and(|u| shared.policy.allowed_uids.contains(&u)) {
        Some("uid_not_allowed")
    } else if shared.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        shared.active.fetch_sub(1, Ordering::SeqCst);
        Some("busy")
    } else {
        None
    };
    if let Some(reason) = refusal {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let reason = finish(shared, &mut rec, None, reason, started);
        let _ = stream.write_all(&wire::refused(reason));
        return;
    }
    let slot = Slot(Arc::clone(shared));
    let spawned = thread::Builder::new().name("call".into()).spawn(move || {
        let slot = slot;
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let reply = answer(&slot.0, &mut stream, &mut rec, started);
        let _ = stream.write_all(&reply);
    });
    if let Err(e) = spawned {
        // The closure, the stream and the slot with it, is dropped: the peer sees EOF.
        eprintln!("ulei-priv: thread: {e}");
    }
}

/// The audit record of the peer and its uid; no uid when the kernel gives no credentials.
fn record_for(stream: &UnixStream) -> (Record, Option<u32>) {
    match rustix::net::sockopt::socket_peercred(stream) {
        Ok(c) => (
            Record::new(c.pid.as_raw_nonzero().get(), c.uid.as_raw(), c.gid.as_raw()),
            Some(c.uid.as_raw()),
        ),
        Err(_) => (Record::new(0, u32::MAX, u32::MAX), None),
    }
}

/// The reply bytes of one connection; the audit line is written before it returns.
fn answer(shared: &Shared, stream: &mut UnixStream, rec: &mut Record, started: Instant) -> Vec<u8> {
    let request = wire::read_line(stream).and_then(|line| wire::decode(&line));
    match &request {
        Ok(Request::List) => rec.set("op", "list"),
        Ok(Request::Run { id, args }) => {
            rec.set("op", "run");
            rec.set("id", id.as_str());
            rec.set("args", args.clone());
        }
        Err(_) => {}
    }
    let refuse = |rec: &mut Record, reason: &'static str| {
        wire::refused(finish(shared, rec, None, reason, started))
    };
    let (id, args) = match request {
        Err(fault) => return refuse(rec, fault.as_str()),
        Ok(Request::List) => {
            let commands: Vec<Value> = shared
                .policy
                .commands
                .iter()
                .map(|c| json!({"id": c.id, "arg": c.arg}))
                .collect();
            let reason = finish(shared, rec, None, "ok", started);
            if reason != "ok" {
                return wire::refused(reason);
            }
            return line(&json!({"decision": "ok", "commands": commands}));
        }
        Ok(Request::Run { id, args }) => (id, args),
    };
    let (cmd, command_line) = match shared.policy.resolve(&id, &args) {
        Ok(found) => found,
        Err(refusal) => return refuse(rec, refusal.as_str()),
    };
    let _guard = match shared.running.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return refuse(rec, "busy"),
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
    };
    // The audit file opens before the command runs: no audit, no command.
    let file = match audit::open(&shared.audit) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ulei-priv: audit {}: {e}", shared.audit.display());
            return refuse(rec, "audit_failed");
        }
    };
    let outcome = match child::run(
        &command_line,
        Duration::from_secs(cmd.timeout_s),
        cmd.max_output_bytes,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ulei-priv: {}: {e}", cmd.id);
            return wire::refused(finish(shared, rec, Some(file), "exec_failed", started));
        }
    };
    rec.set("exit", outcome.exit);
    rec.set("signal", outcome.signal);
    rec.set("timed_out", outcome.timed_out);
    rec.set("stdout_bytes", outcome.stdout.len());
    rec.set("stderr_bytes", outcome.stderr.len());
    rec.set("truncated", outcome.truncated);
    if finish(shared, rec, Some(file), "ok", started) != "ok" {
        return wire::refused("audit_failed");
    }
    let mut reply = line(&json!({
        "decision": "ok",
        "exit": outcome.exit,
        "signal": outcome.signal,
        "timed_out": outcome.timed_out,
        "stdout_bytes": outcome.stdout.len(),
        "stderr_bytes": outcome.stderr.len(),
        "truncated": outcome.truncated,
    }));
    reply.extend_from_slice(&outcome.stdout);
    reply.extend_from_slice(&outcome.stderr);
    reply
}

/// Writes the audit line with `decision` (`ok` or the reason after `refused:`) to the audit file
/// and stderr. Returns `decision`, or `audit_failed` when the file could not be written.
fn finish(
    shared: &Shared,
    rec: &mut Record,
    file: Option<fs::File>,
    decision: &'static str,
    started: Instant,
) -> &'static str {
    let shown = if decision == "ok" {
        "ok".to_owned()
    } else {
        format!("refused:{decision}")
    };
    rec.set("decision", shown);
    rec.set(
        "duration_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    let text = rec.line();
    eprintln!("ulei-priv: audit {text}");
    let written = match file {
        Some(mut f) => audit::append(&mut f, &text),
        None => audit::open(&shared.audit).and_then(|mut f| audit::append(&mut f, &text)),
    };
    match written {
        Ok(()) => decision,
        Err(e) => {
            eprintln!("ulei-priv: audit {}: {e}", shared.audit.display());
            "audit_failed"
        }
    }
}

fn line(value: &Value) -> Vec<u8> {
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}
