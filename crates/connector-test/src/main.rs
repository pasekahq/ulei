//! `ulei-connector-test`, a C11 stdio connector over a synthetic source. It reads one `Request`
//! line on stdin and writes `limit` items numbered after its cursor, each followed by a
//! checkpoint, then `done`. With a key file, each item's text carries the file's SHA-256.
//! The probe arguments put one item per probe first, whose text is the answer of the call:
//! `--probe-inet` `socket(AF_INET)`, `--probe-unix PATH` `socket(AF_UNIX)` and its connect,
//! `--probe-read PATH` reading the file, `--probe-write PATH` appending to an existing file,
//! `--probe-create PATH` creating a new file, `--probe-truncate PATH` opening a file with
//! `O_TRUNC`, `--connect HOST:PORT` the broker's reply and the first line read from the source,
//! `--probe-limit HOST:PORT` 33 broker requests kept open, and `--probe-direct IP:PORT` its own
//! socket and connect.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, IoSliceMut, Write};
use std::mem::MaybeUninit;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketAddrUnix, SocketType,
};
use ulei_contracts::generated::c11::{BrokerRequest, ErrorCode, Line, LineType, Request};

/// The first fd systemd passes (`sd_listen_fds(3)`).
const LISTEN_FDS_START: RawFd = 3;
/// Broker requests of `--probe-limit`: one past the run's connections.
const LIMIT_REQUESTS: usize = 33;
/// How long reading the first line from a source may take.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What one run emits: the probes' items, then items `after + 1` to `after + limit`.
struct Run {
    probes: Vec<Probe>,
    /// The broker when a probe needs it, or why it is missing.
    broker: Option<Result<UnixStream, String>>,
    after: u64,
    limit: u64,
    key_sha256: Option<String>,
}

/// A call the run's sandbox decides on, named in argv.
enum Probe {
    Inet,
    Unix(String),
    File(FileCall, String),
    Connect(String),
    Limit(String),
    Direct(SocketAddr),
}

impl Probe {
    fn needs_broker(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Limit(_))
    }
}

/// A file call of a probe.
#[derive(Clone, Copy)]
enum FileCall {
    Read,
    Write,
    Create,
    Truncate,
}

impl FileCall {
    const ALL: [Self; 4] = [Self::Read, Self::Write, Self::Create, Self::Truncate];

    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Create => "create",
            Self::Truncate => "truncate",
        }
    }

    /// The call on `path` and what it did.
    fn on(self, path: &str) -> io::Result<String> {
        match self {
            Self::Read => fs::read(path).map(|bytes| format!("{} bytes", bytes.len())),
            Self::Write => OpenOptions::new()
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(b"probe\n"))
                .map(|()| "written".to_owned()),
            Self::Create => OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(|_| "created".to_owned()),
            Self::Truncate => OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .map(|_| "truncated".to_owned()),
        }
    }
}

fn main() -> ExitCode {
    let mut out = BufWriter::new(io::stdout().lock());
    let written = match start() {
        Ok(run) => emit(&mut out, &run).map(|()| ExitCode::SUCCESS),
        Err((code, message)) => {
            write_line(&mut out, &error(code, message)).map(|()| ExitCode::FAILURE)
        }
    };
    match written.and_then(|code| out.flush().map(|()| code)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ulei-connector-test: stdout: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The run the request on stdin asks for, or the code and message of the `error` that ends it.
fn start() -> Result<Run, (ErrorCode, String)> {
    let probes = probes(std::env::args().skip(1))?;
    let broker = probes.iter().any(Probe::needs_broker).then(broker);
    let mut text = String::new();
    io::stdin()
        .lock()
        .read_line(&mut text)
        .map_err(|e| (ErrorCode::Internal, format!("stdin: {e}")))?;
    let request: Request = serde_json::from_str(text.trim_end())
        .map_err(|e| (ErrorCode::BadRequest, format!("request: {e}")))?;
    let after = match request.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor.parse().map_err(|_| {
            (
                ErrorCode::BadCursor,
                "cursor is not an item number".to_owned(),
            )
        })?,
    };
    let key_sha256 = match request.key_file {
        None => None,
        Some(path) => {
            let bytes =
                fs::read(&path).map_err(|e| (ErrorCode::KeyUnreadable, format!("{path}: {e}")))?;
            Some(sha256_hex(&bytes))
        }
    };
    Ok(Run {
        probes,
        broker,
        after,
        limit: request.limit,
        key_sha256,
    })
}

/// The probes `args` name.
fn probes(mut args: impl Iterator<Item = String>) -> Result<Vec<Probe>, (ErrorCode, String)> {
    let mut probes = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--probe-inet" {
            probes.push(Probe::Inet);
            continue;
        }
        let file = FileCall::ALL
            .into_iter()
            .find(|call| arg.strip_prefix("--probe-") == Some(call.name()));
        let known = [
            "--probe-unix",
            "--connect",
            "--probe-limit",
            "--probe-direct",
        ];
        if file.is_none() && !known.contains(&arg.as_str()) {
            return Err((ErrorCode::BadRequest, format!("unknown argument {arg:?}")));
        }
        let Some(value) = args.next() else {
            return Err((ErrorCode::BadRequest, format!("{arg} needs a value")));
        };
        probes.push(match (file, arg.as_str()) {
            (Some(call), _) => Probe::File(call, value),
            (None, "--connect") => Probe::Connect(value),
            (None, "--probe-limit") => Probe::Limit(value),
            (None, "--probe-direct") => Probe::Direct(value.parse().map_err(|_| {
                (
                    ErrorCode::BadRequest,
                    format!("--probe-direct {value:?} is not ip:port"),
                )
            })?),
            (None, _) => Probe::Unix(value),
        });
    }
    Ok(probes)
}

/// The broker socket systemd passed as the fd named `broker` (`sd_listen_fds(3)`), taken once.
fn broker() -> Result<UnixStream, String> {
    let var = |name: &str| std::env::var(name).map_err(|e| format!("{name}: {e}"));
    if var("LISTEN_PID")?.parse::<u32>().ok() != Some(std::process::id()) {
        return Err("LISTEN_PID is not this process".to_owned());
    }
    let count: usize = var("LISTEN_FDS")?
        .parse()
        .map_err(|_| "LISTEN_FDS is not a number".to_owned())?;
    let names = var("LISTEN_FDNAMES")?;
    let index = names
        .split(':')
        .take(count)
        .position(|name| name == "broker")
        .ok_or_else(|| format!("no fd named broker in LISTEN_FDNAMES {names}"))?;
    let fd = LISTEN_FDS_START + RawFd::try_from(index).map_err(|e| e.to_string())?;
    // std and rustix own no inherited fd above stderr; systemd passed this one to this pid,
    // and it is taken here once, before anything else opens a file.
    #[allow(unsafe_code)]
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(UnixStream::from(owned))
}

/// Sends one `BrokerRequest` for `entry` and reads its reply line and the fd sent with it.
fn ask(broker: &UnixStream, entry: &str) -> Result<(String, Option<OwnedFd>), String> {
    let request = BrokerRequest {
        connect: entry.to_owned(),
    };
    let mut line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    line.push(b'\n');
    let mut writer = broker;
    writer
        .write_all(&line)
        .map_err(|e| format!("broker write: {e}"))?;
    let (mut reply, mut fd) = (Vec::new(), None);
    while !reply.ends_with(b"\n") {
        let mut buf = [0u8; 256];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let got = rustix::net::recvmsg(
            broker,
            &mut [IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        )
        .map_err(|e| format!("broker recvmsg: {}", errno(e)))?;
        if got.bytes == 0 {
            return Err("the broker closed".to_owned());
        }
        reply.extend_from_slice(&buf[..got.bytes]);
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(mut rights) = message {
                fd = fd.or_else(|| rights.next());
            }
        }
    }
    reply.pop();
    Ok((String::from_utf8_lossy(&reply).into_owned(), fd))
}

/// The broker's reply for `entry` and, with a socket, the first line the source sent on it.
fn connect(broker: &UnixStream, entry: &str) -> String {
    match ask(broker, entry) {
        Err(e) => e,
        Ok((reply, None)) => reply,
        Ok((reply, Some(fd))) => {
            let stream = TcpStream::from(fd);
            let mut first = String::new();
            let read = stream
                .set_read_timeout(Some(READ_TIMEOUT))
                .and_then(|()| BufReader::new(stream).read_line(&mut first));
            match read {
                Ok(_) => format!("{reply}; first line: {}", first.trim_end()),
                Err(e) => format!("{reply}; first line: {e}"),
            }
        }
    }
}

/// [`LIMIT_REQUESTS`] requests for `entry` with every passed socket kept open: how many were
/// `ok` and the first other reply.
fn limit(broker: &UnixStream, entry: &str) -> String {
    let mut kept = Vec::new();
    for _ in 0..LIMIT_REQUESTS {
        match ask(broker, entry) {
            Ok((_, Some(fd))) => kept.push(fd),
            Ok((reply, None)) => return format!("{} ok, then {reply}", kept.len()),
            Err(e) => return format!("{} ok, then {e}", kept.len()),
        }
    }
    format!("{} ok", kept.len())
}

/// Its own socket for `addr` and a connect to it.
fn direct(addr: SocketAddr) -> String {
    let family = match addr {
        SocketAddr::V4(_) => (AddressFamily::INET, "AF_INET"),
        SocketAddr::V6(_) => (AddressFamily::INET6, "AF_INET6"),
    };
    match rustix::net::socket(family.0, SocketType::STREAM, None) {
        Err(e) => format!("socket({}): {}", family.1, errno(e)),
        Ok(fd) => match rustix::net::connect(&fd, &addr) {
            Ok(()) => format!("socket({}) created, connected", family.1),
            Err(e) => format!("socket({}) created, connect: {}", family.1, errno(e)),
        },
    }
}

/// The item of `probe`: its id and the call's answer as text.
fn probe(probe: &Probe, broker: Option<&Result<UnixStream, String>>) -> (String, String) {
    let through = |entry: &str, call: fn(&UnixStream, &str) -> String| match broker {
        Some(Ok(broker)) => call(broker, entry),
        Some(Err(e)) => format!("no broker: {e}"),
        None => "no broker".to_owned(),
    };
    match probe {
        Probe::Connect(entry) => (
            "probe-connect".to_owned(),
            format!("connect {entry}: {}", through(entry, connect)),
        ),
        Probe::Limit(entry) => (
            "probe-limit".to_owned(),
            format!("limit {entry}: {}", through(entry, limit)),
        ),
        Probe::Direct(addr) => (
            "probe-direct".to_owned(),
            format!("direct {addr}: {}", direct(*addr)),
        ),
        Probe::Inet => (
            "probe-inet".to_owned(),
            match rustix::net::socket(AddressFamily::INET, SocketType::STREAM, None) {
                Ok(_) => "socket(AF_INET): created".to_owned(),
                Err(e) => format!("socket(AF_INET): {}", errno(e)),
            },
        ),
        Probe::Unix(path) => {
            let answer = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
                .map_err(|e| format!("socket(AF_UNIX): {}", errno(e)))
                .and_then(|fd| {
                    let addr = SocketAddrUnix::new(path.as_str())
                        .map_err(|e| format!("address {path}: {}", errno(e)))?;
                    rustix::net::connect(&fd, &addr)
                        .map_err(|e| format!("connect(AF_UNIX) {path}: {}", errno(e)))
                });
            (
                "probe-unix".to_owned(),
                answer.map_or_else(|e| e, |()| format!("connect(AF_UNIX) {path}: connected")),
            )
        }
        Probe::File(call, path) => {
            let answer = match call.on(path) {
                Ok(done) => done,
                Err(e) => match e.raw_os_error() {
                    Some(raw) => errno(Errno::from_raw_os_error(raw)),
                    None => e.to_string(),
                },
            };
            (
                format!("probe-{}", call.name()),
                format!("{} {path}: {answer}", call.name()),
            )
        }
    }
}

/// `NAME errno N` for the values the probes expect, `errno N` otherwise.
fn errno(e: Errno) -> String {
    let name = [
        (Errno::PERM, "EPERM"),
        (Errno::NOENT, "ENOENT"),
        (Errno::ACCESS, "EACCES"),
        (Errno::AFNOSUPPORT, "EAFNOSUPPORT"),
        (Errno::CONNREFUSED, "ECONNREFUSED"),
        (Errno::BADF, "EBADF"),
    ]
    .into_iter()
    .find_map(|(known, name)| (known == e).then_some(name));
    match name {
        Some(name) => format!("{name} errno {}", e.raw_os_error()),
        None => format!("errno {}", e.raw_os_error()),
    }
}

fn emit(out: &mut impl Write, run: &Run) -> io::Result<()> {
    for (id, text) in run.probes.iter().map(|p| probe(p, run.broker.as_ref())) {
        write_line(out, &item(id, text))?;
    }
    let last = run.after.saturating_add(run.limit);
    for n in (run.after..last).map(|n| n + 1) {
        let mut text = format!("synthetic item {n}");
        if let Some(digest) = &run.key_sha256 {
            text.push_str("\nkey sha256:");
            text.push_str(digest);
        }
        write_line(out, &item(format!("item-{n}"), text))?;
        let mut checkpoint = line(LineType::Checkpoint);
        checkpoint.cursor = Some(n.to_string());
        write_line(out, &checkpoint)?;
        out.flush()?;
    }
    let mut done = line(LineType::Done);
    done.cursor = (last > run.after).then(|| last.to_string());
    write_line(out, &done)
}

fn item(external_id: String, text: String) -> Line {
    let mut item = line(LineType::Item);
    item.external_id = Some(external_id);
    item.kind = Some("text/plain".to_owned());
    item.bytes_b64 = Some(base64(text.as_bytes()));
    item.text = Some(text);
    item
}

fn write_line(out: &mut impl Write, line: &Line) -> io::Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")
}

fn line(r#type: LineType) -> Line {
    Line {
        r#type,
        external_id: None,
        who: None,
        at: None,
        kind: None,
        text: None,
        bytes_b64: None,
        cursor: None,
        code: None,
        message: None,
        retry_after: None,
    }
}

fn error(code: ErrorCode, message: String) -> Line {
    let mut line = line(LineType::Error);
    line.code = Some(code);
    line.message = Some(message);
    line
}

/// Standard base64 with padding (RFC 4648 section 4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(group >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 (FIPS 180-4) of `data` as lowercase hex. The crate's dependencies are fixed by
/// ADR-0056 to the contract types, so the digest is computed here.
fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = state;
        for (k, wi) in SHA256_K.iter().zip(w) {
            let e = v[4];
            let a = v[0];
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & v[5]) ^ (!e & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & v[1]) ^ (a & v[2]) ^ (v[1] & v[2]);
            v.rotate_right(1);
            v[4] = v[4].wrapping_add(t1);
            v[0] = t1.wrapping_add(s0.wrapping_add(maj));
        }
        for (s, x) in state.iter_mut().zip(v) {
            *s = s.wrapping_add(x);
        }
    }
    state.iter().fold(String::with_capacity(64), |mut hex, x| {
        let _ = write!(hex, "{x:08x}");
        hex
    })
}
