//! The network broker of a run (ADR-0056 section 5.3): a socket the unit's manager connects as
//! the connector's fd 3, on which each C11 `BrokerRequest` gets a `BrokerReply` and, when it is
//! `ok`, the connected TCP socket by `SCM_RIGHTS`.

use std::fs::{self, Permissions};
use std::io::{self, BufRead, BufReader, IoSlice, Read as _, Write as _};
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use ulei_contracts::generated::c11::{BrokerErrorCode, BrokerReply, BrokerRequest};

/// Connections a run gets.
pub(super) const MAX_CONNECTIONS: usize = 32;
/// How long one address may take to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes of a request line, newline included; a longer line ends the broker.
const MAX_REQUEST: u64 = 4096;

/// The `host:port` entries a run may reach.
#[derive(Debug, Clone, Default)]
pub(super) struct Policy {
    /// The manifest's `source.addresses`.
    pub listed: Vec<String>,
    /// The owner's `allow`, which also admits the addresses [`class`] names.
    pub allow: Vec<String>,
}

/// The class of `ip` that a name may resolve to only under the owner's `allow`.
pub(super) fn class(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => class_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => class_v4(v4),
            None => class_v6(v6),
        },
    }
}

fn class_v4(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_private() {
        Some("private")
    } else if ip.is_link_local() {
        Some("link-local")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_multicast() {
        Some("multicast")
    } else {
        None
    }
}

fn class_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let first = ip.segments()[0];
    if ip.is_loopback() {
        Some("loopback")
    } else if first & 0xfe00 == 0xfc00 {
        Some("private")
    } else if first & 0xffc0 == 0xfe80 {
        Some("link-local")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_multicast() {
        Some("multicast")
    } else {
        None
    }
}

/// The answers of one run's broker; it counts the connections it handed out.
pub(super) struct Answerer<R, C> {
    policy: Policy,
    used: usize,
    resolve: R,
    connect: C,
}

impl<R, C> Answerer<R, C>
where
    R: Fn(&str) -> io::Result<Vec<SocketAddr>>,
    C: Fn(&SocketAddr) -> io::Result<TcpStream>,
{
    pub(super) fn new(policy: Policy, resolve: R, connect: C) -> Self {
        Self {
            policy,
            used: 0,
            resolve,
            connect,
        }
    }

    /// The reply to one request line without its newline and the socket an `ok` carries; none
    /// for a line that is not a `BrokerRequest`.
    pub(super) fn answer(&mut self, line: &[u8]) -> Option<(BrokerReply, Option<TcpStream>)> {
        let request: BrokerRequest = serde_json::from_slice(line).ok()?;
        let refused = |code| {
            Some((
                BrokerReply {
                    ok: None,
                    error: Some(code),
                },
                None,
            ))
        };
        if self.used >= MAX_CONNECTIONS {
            return refused(BrokerErrorCode::Limit);
        }
        let entry = request.connect;
        let allowed = self.policy.allow.contains(&entry);
        if !allowed && !self.policy.listed.contains(&entry) {
            return refused(BrokerErrorCode::NotAllowed);
        }
        let addresses = match (self.resolve)(&entry) {
            Ok(addresses) if !addresses.is_empty() => addresses,
            _ => return refused(BrokerErrorCode::ResolveFailed),
        };
        if !allowed && addresses.iter().any(|a| class(a.ip()).is_some()) {
            return refused(BrokerErrorCode::NotAllowed);
        }
        for address in &addresses {
            if let Ok(stream) = (self.connect)(address) {
                self.used += 1;
                let reply = BrokerReply {
                    ok: Some(address.to_string()),
                    error: None,
                };
                return Some((reply, Some(stream)));
            }
        }
        refused(BrokerErrorCode::ConnectFailed)
    }
}

/// Answers the request lines of `stream` until it ends, a line is malformed or a reply cannot
/// be sent. The runner never reads a connected socket.
pub(super) fn serve<R, C>(stream: &UnixStream, answerer: &mut Answerer<R, C>)
where
    R: Fn(&str) -> io::Result<Vec<SocketAddr>>,
    C: Fn(&SocketAddr) -> io::Result<TcpStream>,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader).take(MAX_REQUEST).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Some(request) = line.strip_suffix(b"\n") else {
            return;
        };
        let Some((reply, socket)) = answerer.answer(request) else {
            return;
        };
        if send(stream, &reply, socket.as_ref()).is_err() {
            return;
        }
    }
}

/// Writes `reply` as one line with `socket` attached to its first bytes.
///
/// # Errors
/// The line cannot be written.
pub(super) fn send(
    stream: &UnixStream,
    reply: &BrokerReply,
    socket: Option<&TcpStream>,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    let fds: Vec<_> = socket.iter().map(|s| s.as_fd()).collect();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("SCM_RIGHTS does not fit its buffer"));
    }
    let sent = rustix::net::sendmsg(
        stream,
        &[IoSlice::new(&line)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    let mut rest = stream;
    rest.write_all(&line[sent..])
}

/// What the broker's thread and the run share.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    conn: Mutex<Option<UnixStream>>,
}

/// A run's broker: its socket file until the manager connects, then the thread answering on
/// that connection. Dropping it ends the broker and removes the socket file.
pub(super) struct Broker {
    path: PathBuf,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Broker {
    /// Listens on `path`, mode 0600, for one connection, answers it under `policy` through the
    /// system resolver, and unlinks `path` once connected.
    ///
    /// # Errors
    /// The socket cannot be bound or its mode set; the text names the path.
    pub(super) fn listen(path: &Path, policy: Policy) -> Result<Self, String> {
        let failed = |e: io::Error| format!("broker socket {}: {e}", path.display());
        let listener = UnixListener::bind(path).map_err(failed)?;
        if let Err(e) = fs::set_permissions(path, Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(path);
            return Err(failed(e));
        }
        let shared = Arc::new(Shared::default());
        let (owned, mine) = (path.to_owned(), Arc::clone(&shared));
        let thread = thread::spawn(move || accept_and_serve(&listener, &owned, &mine, policy));
        Ok(Self {
            path: path.to_owned(),
            shared,
            thread: Some(thread),
        })
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wakes an accept still waiting; once the path is unlinked this finds nothing.
        drop(UnixStream::connect(&self.path));
        let conn = self
            .shared
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(conn) = conn {
            let _ = conn.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn accept_and_serve(listener: &UnixListener, path: &Path, shared: &Shared, policy: Policy) {
    let Ok((conn, _)) = listener.accept() else {
        return;
    };
    let _ = fs::remove_file(path);
    let Ok(handle) = conn.try_clone() else {
        return;
    };
    {
        let mut slot = shared.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        *slot = Some(handle);
    }
    let resolve = |entry: &str| entry.to_socket_addrs().map(Iterator::collect);
    let connect = |address: &SocketAddr| {
        if shared.stop.load(Ordering::SeqCst) {
            return Err(io::Error::other("the run ended"));
        }
        TcpStream::connect_timeout(address, CONNECT_TIMEOUT)
    };
    serve(&conn, &mut Answerer::new(policy, resolve, connect));
}

#[cfg(test)]
mod tests {
    use std::io::IoSliceMut;
    use std::net::TcpListener;
    use std::os::fd::OwnedFd;

    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

    use super::*;
    use crate::scratch::Scratch;

    fn reply(line: &[u8]) -> String {
        String::from_utf8(line.to_vec()).expect("UTF-8")
    }

    /// A resolver answering every name with `answer`.
    fn resolving(answer: &'static [&'static str]) -> impl Fn(&str) -> io::Result<Vec<SocketAddr>> {
        move |_| Ok(answer.iter().map(|a| a.parse().expect("address")).collect())
    }

    fn line(answer: Option<(BrokerReply, Option<TcpStream>)>) -> (String, bool) {
        let (reply, socket) = answer.expect("a reply");
        (
            serde_json::to_string(&reply).expect("reply"),
            socket.is_some(),
        )
    }

    #[test]
    fn address_classes_follow_the_broker_rule() {
        for (ip, want) in [
            ("127.0.0.1", Some("loopback")),
            ("127.255.0.9", Some("loopback")),
            ("::1", Some("loopback")),
            ("10.1.2.3", Some("private")),
            ("172.16.0.1", Some("private")),
            ("172.31.255.255", Some("private")),
            ("192.168.0.1", Some("private")),
            ("fc00::1", Some("private")),
            ("fdff::1", Some("private")),
            ("169.254.169.254", Some("link-local")),
            ("fe80::1", Some("link-local")),
            ("0.0.0.0", Some("unspecified")),
            ("::", Some("unspecified")),
            ("224.0.0.1", Some("multicast")),
            ("ff02::1", Some("multicast")),
            ("::ffff:127.0.0.1", Some("loopback")),
            ("::ffff:10.0.0.1", Some("private")),
            ("172.32.0.1", None),
            ("93.184.216.34", None),
            ("2606:2800:220:1::1", None),
            ("::ffff:93.184.216.34", None),
        ] {
            let ip: IpAddr = ip.parse().expect("ip");
            assert_eq!(class(ip), want, "{ip}");
        }
    }

    #[test]
    fn only_listed_entries_reach_public_addresses_and_allow_admits_private_ones() {
        let policy = Policy {
            listed: vec![
                "notes.example.test:443".to_owned(),
                "lan.example.test:80".to_owned(),
            ],
            allow: vec!["owner.example.test:8443".to_owned()],
        };
        let open = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = open.local_addr().expect("addr").port();
        let connect = move |_: &SocketAddr| TcpStream::connect((Ipv4Addr::LOCALHOST, port));
        let table: [(&str, &'static [&'static str], &str, bool); 7] = [
            (
                "notes.example.test:443",
                &["93.184.216.34:443"],
                r#"{"ok":"93.184.216.34:443"}"#,
                true,
            ),
            (
                "other.example.test:443",
                &["93.184.216.34:443"],
                r#"{"error":"not_allowed"}"#,
                false,
            ),
            (
                "notes.example.test:444",
                &["93.184.216.34:444"],
                r#"{"error":"not_allowed"}"#,
                false,
            ),
            (
                "lan.example.test:80",
                &["192.168.1.5:80"],
                r#"{"error":"not_allowed"}"#,
                false,
            ),
            (
                "notes.example.test:443",
                &["93.184.216.34:443", "[::1]:443"],
                r#"{"error":"not_allowed"}"#,
                false,
            ),
            (
                "owner.example.test:8443",
                &["127.0.0.1:8443"],
                r#"{"ok":"127.0.0.1:8443"}"#,
                true,
            ),
            (
                "notes.example.test:443",
                &[],
                r#"{"error":"resolve_failed"}"#,
                false,
            ),
        ];
        for (entry, resolved, want, passed) in table {
            let mut answerer = Answerer::new(policy.clone(), resolving(resolved), connect);
            let request = format!(r#"{{"connect":"{entry}"}}"#);
            assert_eq!(
                line(answerer.answer(request.as_bytes())),
                (want.to_owned(), passed),
                "{entry} -> {resolved:?}"
            );
        }
    }

    #[test]
    fn each_address_is_tried_and_none_connecting_is_connect_failed() {
        let policy = Policy {
            listed: vec!["notes.example.test:443".to_owned()],
            allow: Vec::new(),
        };
        let open = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = open.local_addr().expect("addr").port();
        let second = |a: &SocketAddr| {
            if a.ip() == "93.184.216.35".parse::<IpAddr>().expect("ip") {
                TcpStream::connect(("127.0.0.1", port))
            } else {
                Err(io::Error::from(io::ErrorKind::TimedOut))
            }
        };
        let request = br#"{"connect":"notes.example.test:443"}"#;
        let mut answerer = Answerer::new(
            policy.clone(),
            resolving(&["93.184.216.34:443", "93.184.216.35:443"]),
            second,
        );
        assert_eq!(
            line(answerer.answer(request)),
            (r#"{"ok":"93.184.216.35:443"}"#.to_owned(), true)
        );
        let mut answerer = Answerer::new(
            policy,
            resolving(&["93.184.216.34:443"]),
            |_: &SocketAddr| Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        );
        assert_eq!(
            line(answerer.answer(request)),
            (r#"{"error":"connect_failed"}"#.to_owned(), false)
        );
    }

    #[test]
    fn the_thirty_third_request_is_limit_and_malformed_lines_get_no_reply() {
        let open = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = open.local_addr().expect("addr").port();
        let entry = format!("127.0.0.1:{port}");
        let policy = Policy {
            listed: Vec::new(),
            allow: vec![entry.clone()],
        };
        let mut answerer = Answerer::new(
            policy,
            |e: &str| e.to_socket_addrs().map(Iterator::collect),
            |a: &SocketAddr| TcpStream::connect(a),
        );
        let request = format!(r#"{{"connect":"{entry}"}}"#);
        let mut kept = Vec::new();
        for n in 1..=MAX_CONNECTIONS {
            let (reply, socket) = answerer.answer(request.as_bytes()).expect("reply");
            assert_eq!(reply.ok.as_deref(), Some(entry.as_str()), "{n}");
            kept.push(socket.expect("socket"));
        }
        assert_eq!(
            line(answerer.answer(request.as_bytes())),
            (r#"{"error":"limit"}"#.to_owned(), false)
        );
        for malformed in [&b"connect me"[..], br#"{"connect":"a:1","x":1}"#, b"{}"] {
            assert!(answerer.answer(malformed).is_none(), "{}", reply(malformed));
        }
    }

    /// Reads one reply line from `stream` and the fds that came with it.
    fn receive(stream: &UnixStream) -> (String, Vec<OwnedFd>) {
        let mut got = Vec::new();
        let mut fds = Vec::new();
        while !got.ends_with(b"\n") {
            let mut buf = [0u8; 256];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let msg = rustix::net::recvmsg(
                stream,
                &mut [IoSliceMut::new(&mut buf)],
                &mut control,
                RecvFlags::empty(),
            )
            .expect("recvmsg");
            assert!(msg.bytes > 0, "the broker closed");
            got.extend_from_slice(&buf[..msg.bytes]);
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(rights) = message {
                    fds.extend(rights);
                }
            }
        }
        (reply(&got), fds)
    }

    #[test]
    fn an_ok_reply_carries_the_connected_socket_over_scm_rights() {
        let source = TcpListener::bind("127.0.0.1:0").expect("source");
        let port = source.local_addr().expect("addr").port();
        let entry = format!("127.0.0.1:{port}");
        let (runner, connector) = UnixStream::pair().expect("pair");
        let policy = Policy {
            listed: Vec::new(),
            allow: vec![entry.clone()],
        };
        let server = thread::spawn(move || {
            let mut answerer = Answerer::new(
                policy,
                |e: &str| e.to_socket_addrs().map(Iterator::collect),
                |a: &SocketAddr| TcpStream::connect(a),
            );
            serve(&runner, &mut answerer);
        });
        let mut writer = &connector;
        writeln!(writer, r#"{{"connect":"{entry}"}}"#).expect("request");
        let (line, mut fds) = receive(&connector);
        assert_eq!(line, format!("{{\"ok\":\"{entry}\"}}\n"));
        assert_eq!(fds.len(), 1);
        let (mut accepted, _) = source.accept().expect("accept");
        accepted.write_all(b"first line\n").expect("source writes");
        let mut passed = BufReader::new(TcpStream::from(fds.remove(0)));
        let mut first = String::new();
        passed
            .read_line(&mut first)
            .expect("read through the passed socket");
        assert_eq!(first, "first line\n");
        writeln!(writer, r#"{{"connect":"127.0.0.1:1"}}"#).expect("request");
        let (line, fds) = receive(&connector);
        assert_eq!(line, "{\"error\":\"not_allowed\"}\n");
        assert!(fds.is_empty());
        writer.write_all(b"not json\n").expect("malformed");
        server.join().expect("the broker ends on a malformed line");
    }

    #[test]
    fn the_broker_unlinks_its_socket_once_connected_and_ends_with_its_run() {
        let scratch = Scratch::new("broker");
        let path = scratch.0.join("run.sock");
        let broker = Broker::listen(&path, Policy::default()).expect("listen");
        let mode = fs::metadata(&path).expect("socket").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let conn = UnixStream::connect(&path).expect("connect");
        let mut writer = &conn;
        writeln!(writer, r#"{{"connect":"a.example.test:1"}}"#).expect("request");
        let (line, _) = receive(&conn);
        assert_eq!(line, "{\"error\":\"not_allowed\"}\n");
        assert!(!path.exists());
        drop(broker);
        let mut rest = Vec::new();
        assert_eq!((&conn).read_to_end(&mut rest).expect("eof"), 0);

        let unused = scratch.0.join("unused.sock");
        drop(Broker::listen(&unused, Policy::default()).expect("listen"));
        assert!(!unused.exists());
        assert_eq!(fs::read_dir(&scratch.0).expect("dir").count(), 0);
    }
}
