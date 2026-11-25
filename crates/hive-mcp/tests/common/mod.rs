//! A running `ulei-hive-mcp serve` on a synthetic config in a scratch directory, and a raw
//! HTTP/1.1 client of its endpoint.

#![allow(dead_code, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

pub(crate) const SERVER: &str = env!("CARGO_BIN_EXE_ulei-hive-mcp");
pub(crate) const FAKE_HIVE: &str = env!("CARGO_BIN_EXE_ulei-hive-mcp-fake-hive");
pub(crate) const TOKEN: &str = "synthetic-test-token-0123456789abcdef";

/// A directory removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ulei-hive-mcp-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes the token file with `mode` and a config whose `[hive_mcp]` runs `command`; `extra`
/// lines go into the section.
pub(crate) fn write_config(
    dir: &Path,
    command: &str,
    hive_config: &Path,
    extra: &str,
    mode: u32,
) -> PathBuf {
    let token = dir.join("hive-mcp.token");
    fs::write(&token, format!("{TOKEN}\n")).unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(mode)).unwrap();
    let config = dir.join("ulei.toml");
    fs::write(
        &config,
        format!(
            "[hive_mcp]\nlisten = \"127.0.0.1:0\"\ntoken_file = \"{}\"\nhive_command = \"{command}\"\nhive_config = \"{}\"\n{extra}",
            token.display(),
            hive_config.display()
        ),
    )
    .unwrap();
    config
}

/// A running server, killed on drop; its stderr lines are collected.
pub(crate) struct Running {
    pub(crate) child: Child,
    pub(crate) port: u16,
    pub(crate) stderr: Arc<Mutex<Vec<String>>>,
}

impl Running {
    pub(crate) fn start(config: &Path) -> Self {
        let mut child = Command::new(SERVER)
            .args(["serve", "--config"])
            .arg(config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", config.parent().unwrap())
            .env("LANG", "C.UTF-8")
            .env("SECRET_OF_THE_TEST", "must-not-reach-the-cli")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let address = line
            .trim()
            .strip_prefix("ulei-hive-mcp: ready: ")
            .unwrap_or_else(|| panic!("no ready line: {line:?}"));
        let port = address.rsplit(':').next().unwrap().parse().unwrap();
        let stderr = Arc::new(Mutex::new(vec![line.trim().to_owned()]));
        let lines = Arc::clone(&stderr);
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                lines.lock().unwrap().push(line);
            }
        });
        Self {
            child,
            port,
            stderr,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub(crate) fn stderr(&self) -> Vec<String> {
        self.stderr.lock().unwrap().clone()
    }

    /// Sends a raw request and returns the status, the head and the body.
    pub(crate) fn raw(
        &self,
        method: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut request = format!("{method} /mcp HTTP/1.1\r\n");
        for (name, value) in headers {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        let _ = write!(
            request,
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head.to_owned(), body.to_owned())
    }

    /// A POST of `message` with the bearer token, the host and `session` if any.
    pub(crate) fn post(&self, message: &Value, session: Option<&str>) -> (u16, String, String) {
        let host = format!("127.0.0.1:{}", self.port);
        let auth = format!("Bearer {TOKEN}");
        let mut headers = vec![
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        if let Some(session) = session {
            headers.push(("Mcp-Session-Id", session));
        }
        self.raw("POST", &headers, &message.to_string())
    }

    /// A new session's id after `initialize` and `notifications/initialized`.
    pub(crate) fn session(&self) -> String {
        let (status, head, _) = self.post(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
            None,
        );
        assert_eq!(status, 200, "{head}");
        let id = head
            .lines()
            .find_map(|l| l.strip_prefix("Mcp-Session-Id: "))
            .unwrap()
            .to_owned();
        let (status, _, _) = self.post(
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            Some(&id),
        );
        assert_eq!(status, 202);
        id
    }

    /// The `result` of `tools/call` of `name` with `arguments`.
    pub(crate) fn call(&self, session: &str, name: &str, arguments: &Value) -> Value {
        let (status, _, body) = self.post(
            &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": arguments}}),
            Some(session),
        );
        assert_eq!(status, 200, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        answer["result"].clone()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
