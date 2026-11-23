//! `ulei-connector-mcp-synthetic`, the synthetic ready MCP server the example's tests and CI
//! wrap. It serves the tool `list_records` with arguments `after` and `limit` over stdio and
//! answers `{"records": [...]}` as its `structuredContent` and as JSON text.
//!
//! - `--count N`: records `n` from 1 to `N`, `{"id": "item-n", "seq": n, "body": "synthetic item
//!   n"}`, the same records as the test connector's items; `after` is the last `seq` delivered.
//! - `--entries FILE`: the JSON array in FILE, whole, whatever `after` and `limit` say.
//! - `--report`: one record whose `body` names its environment variables and its fds from 3.
//! - `--error FILE`: an `isError` result with the JSON in FILE as `structuredContent`.
//! - `--fail exit|garbage|rpc-error|tool-error`: its answer to `list_records` is an exit with
//!   status 3, a line that is not JSON, a JSON-RPC error or an `isError` result with text only.

use std::fs;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

enum Records {
    Count(u64),
    Entries(Value),
    Report,
    Error(Value),
    ToolError,
}

/// What the server answers `list_records`.
enum Mode {
    Serve(Records),
    Fail(Fail),
}

/// An answer to `list_records` that is no result.
enum Fail {
    Exit,
    Garbage,
    RpcError,
}

fn main() -> ExitCode {
    let mode = match mode(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("ulei-connector-mcp-synthetic: {e}");
            return ExitCode::from(2);
        }
    };
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let Ok(message) = line.map(|line| serde_json::from_str::<Value>(&line)) else {
            return ExitCode::FAILURE;
        };
        let Ok(message) = message else {
            return ExitCode::FAILURE;
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        let params = &message["params"];
        let answer = match message["method"].as_str() {
            Some("initialize") => reply(
                id,
                &json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "synthetic", "version": "0.0.1"},
                }),
            ),
            Some("tools/call") if params["name"] == "list_records" => match &mode {
                Mode::Fail(Fail::Exit) => return ExitCode::from(3),
                Mode::Fail(Fail::Garbage) => "not json".to_owned(),
                Mode::Fail(Fail::RpcError) => rpc_error(id, -32603, "synthetic failure"),
                Mode::Serve(records) => reply(id, &list(records, &params["arguments"])),
            },
            _ => rpc_error(id, -32601, "Method not found"),
        };
        if writeln!(out, "{answer}")
            .and_then(|()| out.flush())
            .is_err()
        {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn mode(args: &[String]) -> Result<Mode, String> {
    let read = |path: &str| -> Result<Value, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    };
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--count", n] => n
            .parse()
            .map(|n| Mode::Serve(Records::Count(n)))
            .map_err(|e| format!("--count {n}: {e}")),
        ["--entries", path] => read(path).map(|v| Mode::Serve(Records::Entries(v))),
        ["--report"] => Ok(Mode::Serve(Records::Report)),
        ["--error", path] => read(path).map(|v| Mode::Serve(Records::Error(v))),
        ["--fail", "exit"] => Ok(Mode::Fail(Fail::Exit)),
        ["--fail", "garbage"] => Ok(Mode::Fail(Fail::Garbage)),
        ["--fail", "rpc-error"] => Ok(Mode::Fail(Fail::RpcError)),
        ["--fail", "tool-error"] => Ok(Mode::Serve(Records::ToolError)),
        _ => Err(format!("unknown arguments {args:?}")),
    }
}

/// The result of `list_records` with `arguments`.
fn list(records: &Records, arguments: &Value) -> Value {
    let entries = match records {
        Records::Count(count) => {
            let after = match &arguments["after"] {
                Value::Null => 0,
                Value::String(after) => match after.parse::<u64>() {
                    Ok(after) => after,
                    Err(_) => return tool_error("after is not a record number"),
                },
                _ => return tool_error("after is not a string"),
            };
            let limit = arguments["limit"].as_u64().unwrap_or(100);
            let last = after.saturating_add(limit).min(*count);
            (after + 1..=last)
                .map(|n| json!({"id": format!("item-{n}"), "seq": n, "body": format!("synthetic item {n}")}))
                .collect()
        }
        Records::Entries(entries) => entries.clone(),
        Records::Report => {
            let mut names: Vec<String> = std::env::vars_os()
                .map(|(name, _)| name.to_string_lossy().into_owned())
                .collect();
            names.sort();
            let fds: Vec<String> = (3..16)
                .filter_map(|fd| {
                    let target = fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
                    Some(format!("{fd} {}", target.display()))
                })
                .collect();
            json!([{"id": "report", "seq": 1,
                "body": format!("env {}; fds {}", names.join(","), fds.join(","))}])
        }
        Records::Error(error) => {
            return json!({
                "content": [{"type": "text", "text": "synthetic error"}],
                "structuredContent": error,
                "isError": true,
            });
        }
        Records::ToolError => return tool_error("upstream answered 503"),
    };
    let content = json!({"records": entries});
    json!({
        "content": [{"type": "text", "text": content.to_string()}],
        "structuredContent": content,
    })
}

fn tool_error(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn reply(id: &Value, result: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn rpc_error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
