//! The engine's own MCP client against `ulei-hive-mcp`: `ulei-engine run` of this build, with
//! `engine.mcp_servers = "hive"` and the bearer header from an env-file, on a fake C4G gateway
//! that calls the five tools in one step. The engine must connect (preflight, handshake,
//! listing), register `mcp__hive__<tool>`, and hand each tool's answer back to the model.

#![allow(missing_docs, clippy::unwrap_used, clippy::too_many_lines)]

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::{FAKE_HIVE, Running, Scratch, TOKEN, write_config};
use serde_json::{Value, json};

/// The `ulei-engine` binary of this build: in the target directory that holds this test.
fn engine_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .map(|dir| dir.join("ulei-engine"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no ulei-engine above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

fn read_request(stream: &mut UnixStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut piece = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let length = head
        .split("\r\n")
        .find_map(|l| {
            let (name, value) = l.split_once(": ")?;
            name.eq_ignore_ascii_case("content-length").then_some(value)
        })
        .map_or(Some(0), |v| v.parse::<usize>().ok())?;
    while buf.len() < end + length {
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    }
    let target = head.split("\r\n").next()?.rsplit_once(' ')?.0.to_owned();
    Some((target, buf.split_off(end)))
}

fn json_answer(status: u16, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn stream(events: &[Value]) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for event in events {
        let mut line = event.to_string().into_bytes();
        line.push(b'\n');
        out.extend_from_slice(format!("{:x}\r\n", line.len()).as_bytes());
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

fn route() -> Value {
    json!({"type": "route", "route": {"vendor": "vendor-a", "model": "model-a", "login_label": "login-a", "lease": "lease-1"}})
}

/// A C4G gateway answering turn `n` with `steps[n]` and recording each turn request.
struct Gateway {
    turns: Arc<Mutex<Vec<Value>>>,
}

impl Gateway {
    fn start(socket: &Path, steps: Vec<Vec<Value>>) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let turns = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&turns);
        thread::spawn(move || {
            for stream_ in listener.incoming() {
                let Ok(mut conn) = stream_ else { return };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                let Some((target, body)) = read_request(&mut conn) else {
                    continue;
                };
                let bytes = match target.as_str() {
                    "POST /v1/model/turn" => {
                        let mut turns = recorded.lock().unwrap();
                        turns.push(serde_json::from_slice(&body).unwrap());
                        let step = steps.get(turns.len() - 1).cloned().unwrap_or_default();
                        stream(&step)
                    }
                    "GET /v1/model/routes" => json_answer(200, &json!({"routes": []})),
                    _ => json_answer(
                        404,
                        &json!({"class": "bad_request", "retryable": false, "message": "no"}),
                    ),
                };
                let _ = conn.write_all(&bytes);
            }
        });
        Self { turns }
    }
}

#[test]
fn the_engine_registers_and_calls_the_five_hive_tools() {
    let dir = Scratch::new();
    let hive_config = dir.0.join("hive.toml");
    let server = Running::start(&write_config(&dir.0, FAKE_HIVE, &hive_config, "", 0o600));

    let call = |id: &str, name: &str, input: Value| json!({"type": "tool_call", "tool_call": {"id": id, "name": format!("mcp__hive__{name}"), "input": input}});
    let tool_step = vec![
        route(),
        call(
            "c1",
            "hive_create",
            json!({"title": "Synthetic delegation", "goal": "G", "accept_review": "The report names G.", "step": 2, "source": "msg:1"}),
        ),
        call("c2", "hive_list", json!({})),
        call("c3", "hive_show", json!({"card_id": "h_0000000b"})),
        call(
            "c4",
            "hive_steer",
            json!({"card_id": "h_0000000a", "note": "Use the notes."}),
        ),
        call(
            "c5",
            "hive_cancel",
            json!({"card_id": "h_0000000a", "reason": "Done elsewhere."}),
        ),
        json!({"type": "stop", "reason": "tool_use"}),
    ];
    let text_step = vec![
        route(),
        json!({"type": "text_delta", "text": "delegated"}),
        json!({"type": "usage", "usage": {"input": 10, "output": 3, "cache_read": 0, "cache_write": 0}}),
        json!({"type": "stop", "reason": "end"}),
    ];
    let socket = dir.0.join("gateway.sock");
    let gateway = Gateway::start(&socket, vec![tool_step, text_step]);

    let brain = dir.0.join("brain");
    fs::create_dir_all(brain.join("skills")).unwrap();
    fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").unwrap();
    let env_file = dir.0.join("engine-state").join("hive-mcp.env");
    fs::create_dir_all(env_file.parent().unwrap()).unwrap();
    fs::write(&env_file, format!("Authorization=Bearer {TOKEN}\n")).unwrap();
    let journal = dir.0.join("journal.sqlite3");
    let config = dir.0.join("engine.toml");
    fs::write(
        &config,
        format!(
            "[qb]\nsocket = \"{socket}\"\n\n[engine]\njournal = \"{journal}\"\nterminal_output = \"{spill}\"\n\
             soul_file = \"{brain}/SOUL.md\"\nindex_file = \"{brain}/INDEX.md\"\nmemory_file = \"{brain}/MEMORY.md\"\n\
             user_file = \"{brain}/USER.md\"\nskills_dir = \"{brain}/skills\"\nmax_steps = 8\nmcp_servers = \"hive\"\n\n\
             [engine.mcp.hive]\nurl = \"{url}\"\nenv_file = \"{env}\"\ntimeout = 60\nconnect_timeout = 10\n",
            socket = socket.display(),
            journal = journal.display(),
            spill = dir.0.join("terminal-output").display(),
            brain = brain.display(),
            url = server.url(),
            env = env_file.display(),
        ),
    )
    .unwrap();

    let out = Command::new(engine_binary())
        .args([
            "run",
            "--session",
            "chat",
            "--route",
            "main",
            "--prompt",
            "delegate it",
            "--config",
        ])
        .arg(&config)
        .current_dir(&dir.0)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &dir.0)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "delegated\n",
        "{stderr}"
    );
    assert!(!stderr.contains("Failed to connect"), "{stderr}");

    let turns = gateway.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 2);
    let offered: Vec<&str> = turns[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with("mcp__hive__"))
        .collect();
    assert_eq!(
        offered,
        [
            "mcp__hive__hive_create",
            "mcp__hive__hive_list",
            "mcp__hive__hive_show",
            "mcp__hive__hive_steer",
            "mcp__hive__hive_cancel",
            "mcp__hive__hive_resolve",
            "mcp__hive__hive_inspect",
            "mcp__hive__hive_health",
            "mcp__hive__hive_file"
        ]
    );
    let create = turns[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "mcp__hive__hive_create")
        .unwrap();
    assert_eq!(
        create["input_schema"]["required"],
        json!(["title", "goal", "step", "source"]),
        "{create}"
    );

    let sent = turns[1].to_string();
    for needle in [
        "h_00000000",
        "Synthetic one",
        "# Synthetic report",
        "not delivered yet",
        "h_0000000a cancelled",
    ] {
        assert!(sent.contains(needle), "{needle} not in {sent}");
    }
    assert_eq!(sent.matches(r#""is_error":false"#).count(), 5, "{sent}");
    assert!(!sent.contains(TOKEN));

    let argv: Vec<Value> = fs::read_to_string(dir.0.join("hive.toml.log"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["argv"][0].clone())
        .collect();
    assert_eq!(
        argv,
        [
            json!("create"),
            json!("list"),
            json!("show"),
            json!("steer"),
            json!("cancel")
        ]
    );
    let log = server.stderr().join("\n");
    for tool in [
        "hive_create",
        "hive_list",
        "hive_show",
        "hive_steer",
        "hive_cancel",
    ] {
        assert!(log.contains(&format!("ulei-hive-mcp: {tool} ")), "{log}");
    }
}
