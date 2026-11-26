//! `ulei-hive-mcp serve` over HTTP with the fake hive CLI: the transport's refusals (token,
//! `Host`, `Origin`, method, session, protocol version), the MCP methods, each tool's argv and
//! cleared environment, the C2 outputs and tool errors, the timeout and output cap, and the
//! refused configs.

#![allow(missing_docs, clippy::unwrap_used, clippy::too_many_lines)]

mod common;

use std::fs;
use std::process::Command;

use common::{FAKE_HIVE, Running, SERVER, Scratch, TOKEN, write_config};
use serde_json::{Value, json};

fn fake(dir: &Scratch, extra: &str) -> Running {
    let hive_config = dir.0.join("hive.toml");
    Running::start(&write_config(&dir.0, FAKE_HIVE, &hive_config, extra, 0o600))
}

/// The argv and environment names of each fake hive run so far.
fn runs(dir: &Scratch) -> Vec<Value> {
    fs::read_to_string(dir.0.join("hive.toml.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn the_transport_refuses_what_the_spec_and_the_token_refuse() {
    let dir = Scratch::new();
    let server = fake(&dir, "");
    let host = format!("127.0.0.1:{}", server.port);
    let auth = format!("Bearer {TOKEN}");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}).to_string();
    let json_headers = |extra: &[(&'static str, &'static str)]| {
        let mut h: Vec<(&str, String)> = vec![
            ("Host", host.clone()),
            ("Content-Type", "application/json".into()),
        ];
        h.extend(extra.iter().map(|(n, v)| (*n, (*v).to_owned())));
        h
    };
    let send = |headers: &[(&str, String)], method: &str, body: &str| {
        let refs: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        server.raw(method, &refs, body)
    };

    // No token, a wrong token, another scheme.
    let (status, head, _) = send(&json_headers(&[]), "POST", &init);
    assert_eq!(status, 401);
    assert!(head.contains("WWW-Authenticate: Bearer"));
    let wrong = format!("Bearer {}x", &TOKEN[1..]);
    assert_eq!(
        send(
            &[json_headers(&[]), vec![("Authorization", wrong)]].concat(),
            "POST",
            &init
        )
        .0,
        401
    );
    assert_eq!(
        send(
            &[
                json_headers(&[]),
                vec![("Authorization", format!("Basic {TOKEN}"))]
            ]
            .concat(),
            "POST",
            &init
        )
        .0,
        401
    );

    let authed = |extra: &[(&'static str, &'static str)]| {
        let mut h = json_headers(extra);
        h.push(("Authorization", auth.clone()));
        h
    };
    // A foreign Origin and a foreign Host are refused even with the token.
    assert_eq!(
        send(&authed(&[("Origin", "http://evil.test")]), "POST", &init).0,
        403
    );
    assert_eq!(
        send(
            &authed(&[("Origin", "http://localhost:3000")]),
            "POST",
            &init
        )
        .0,
        200
    );
    let rebound = vec![
        ("Host", "evil.test:80".to_owned()),
        ("Authorization", auth.clone()),
        ("Content-Type", "application/json".into()),
    ];
    assert_eq!(send(&rebound, "POST", &init).0, 403);

    // No standing stream: GET and HEAD are 405, as the engine's preflight expects.
    let (status, head, _) = send(&authed(&[]), "GET", "");
    assert_eq!(status, 405);
    assert!(head.contains("Allow: POST, DELETE"));
    assert_eq!(send(&authed(&[]), "HEAD", "").0, 405);
    assert_eq!(send(&authed(&[]), "PUT", "").0, 405);
    // Another content type, a batch, garbage.
    let text = vec![
        ("Host", host.clone()),
        ("Authorization", auth.clone()),
        ("Content-Type", "text/plain".into()),
    ];
    assert_eq!(send(&text, "POST", &init).0, 415);
    assert_eq!(send(&authed(&[]), "POST", &format!("[{init}]")).0, 400);
    assert_eq!(send(&authed(&[]), "POST", "{not json").0, 400);

    // Sessions: none, unknown, ended.
    let tools = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    assert_eq!(server.post(&tools, None).0, 400);
    assert_eq!(
        server
            .post(&tools, Some("0123456789abcdef0123456789abcdef"))
            .0,
        404
    );
    let session = server.session();
    assert_eq!(server.post(&tools, Some(&session)).0, 200);
    let unsupported = [
        authed(&[("MCP-Protocol-Version", "1999-01-01")]),
        vec![("Mcp-Session-Id", session.clone())],
    ]
    .concat();
    assert_eq!(send(&unsupported, "POST", &tools.to_string()).0, 400);
    let end = [authed(&[]), vec![("Mcp-Session-Id", session.clone())]].concat();
    assert_eq!(send(&end, "DELETE", "").0, 204);
    assert_eq!(send(&end, "DELETE", "").0, 404);
    assert_eq!(server.post(&tools, Some(&session)).0, 404);

    assert!(runs(&dir).is_empty(), "no refused request reached the CLI");
    assert!(server.stderr().iter().all(|l| !l.contains(TOKEN)));
}

#[test]
fn initialize_negotiates_the_engine_handshake_versions() {
    let dir = Scratch::new();
    let server = fake(&dir, "");
    for (asked, answered) in [
        ("2025-11-25", "2025-11-25"),
        ("2025-06-18", "2025-06-18"),
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2024-11-05"),
        ("2099-01-01", "2025-11-25"),
    ] {
        let (status, head, body) = server.post(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": asked, "capabilities": {}}}),
            None,
        );
        assert_eq!(status, 200);
        assert!(head.contains("Content-Type: application/json"));
        let id = head
            .lines()
            .find_map(|l| l.strip_prefix("Mcp-Session-Id: "))
            .unwrap();
        assert_eq!(id.len(), 32);
        let result = &serde_json::from_str::<Value>(&body).unwrap()["result"];
        assert_eq!(result["protocolVersion"], answered);
        assert_eq!(
            result["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(result["serverInfo"]["name"], "ulei-hive-mcp");
    }
    let session = server.session();
    let (_, _, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        Some(&session),
    );
    let names: Vec<String> = serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "hive_create",
            "hive_list",
            "hive_show",
            "hive_steer",
            "hive_cancel",
            "hive_inspect",
            "hive_health",
            "hive_file"
        ]
    );
    let (_, _, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}),
        Some(&session),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["result"],
        json!({})
    );
    let (_, _, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
        Some(&session),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32601
    );
    let (_, _, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "hive_script", "arguments": {}}}),
        Some(&session),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32602
    );
}

#[test]
fn each_tool_runs_its_argv_in_a_cleared_environment_and_answers_c2() {
    let dir = Scratch::new();
    let server = fake(&dir, "actor = \"secretary\"\n");
    let session = server.session();

    let created = server.call(
        &session,
        "hive_create",
        &json!({
        "title": "--script=/tmp/x", "goal": "Synthetic goal", "acceptance": "test -f out.txt",
        "step": 2, "source": "msg:1", "idem_key": "req-1"}),
    );
    assert_eq!(created["isError"], false, "{created}");
    assert_eq!(
        created["structuredContent"],
        json!({"card_id": "h_00000000", "created": true, "state": "queued", "card_class": "owner_task", "owner_visible": true})
    );
    let text: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, created["structuredContent"]);

    let listed = server.call(
        &session,
        "hive_list",
        &json!({"state": "queued", "limit": 1}),
    );
    assert_eq!(
        listed["structuredContent"],
        json!({"cards": [{"card_id": "h_0000000a", "state": "queued", "round": 0, "title": "Synthetic one"}]})
    );
    let shown = server.call(&session, "hive_show", &json!({"card_id": "h_0000000b"}));
    assert_eq!(
        shown["structuredContent"],
        json!({"card_id": "h_0000000b", "state": "done", "round": 1,
        "title": "Synthetic card", "result_line": "Synthetic result.", "report_line": "# Synthetic report"})
    );
    let missing = server.call(&session, "hive_show", &json!({"card_id": "h_00000000"}));
    assert_eq!(
        missing,
        json!({"content": [{"type": "text", "text": "no such card h_00000000"}], "isError": true})
    );
    let inspected = server.call(&session, "hive_inspect", &json!({"card_id": "h_0000000b"}));
    assert_eq!(inspected["structuredContent"]["state"], "running");
    assert_eq!(
        inspected["structuredContent"]["waiting_on"],
        "60 s: gh pr checks 7 --watch PR #7"
    );
    assert_eq!(
        inspected["structuredContent"]["snapshot"]["card_id"],
        "h_0000000b"
    );
    let health = server.call(&session, "hive_health", &json!({}));
    assert_eq!(health["structuredContent"]["slots_used"], 4);
    assert_eq!(health["structuredContent"]["ticking"], true);
    let read = server.call(
        &session,
        "hive_file",
        &json!({"card_id": "h_0000000b", "path": "REPORT.md"}),
    );
    assert_eq!(read["structuredContent"]["text"], "Synthetic report\n");
    let escaped = server.call(
        &session,
        "hive_file",
        &json!({"card_id": "h_0000000b", "path": "ev/../../x"}),
    );
    assert_eq!(escaped["isError"], true);
    let steered = server.call(
        &session,
        "hive_steer",
        &json!({"card_id": "h_0000000a", "note": "-- use the notes"}),
    );
    assert_eq!(steered["structuredContent"]["ok"], true);
    assert!(
        steered["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("not delivered yet")
    );
    let cancelled = server.call(
        &session,
        "hive_cancel",
        &json!({"card_id": "h_0000000a", "reason": "No longer needed."}),
    );
    assert_eq!(
        cancelled["structuredContent"],
        json!({"ok": true, "message": "h_0000000a cancelled"})
    );

    let runs = runs(&dir);
    let argv: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            r["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_owned())
                .collect()
        })
        .collect();
    assert_eq!(
        argv,
        [
            vec![
                "create",
                "--json",
                "--title=--script=/tmp/x",
                "--goal=Synthetic goal",
                "--acceptance=test -f out.txt",
                "--step=2",
                "--source=msg:1",
                "--created-by=secretary",
                "--kind=owner_task",
                "--idem=req-1"
            ],
            vec!["list", "--json", "--state=queued", "--limit=1"],
            vec!["show", "--json", "--", "h_0000000b"],
            vec!["show", "--json", "--", "h_00000000"],
            vec!["inspect", "--json", "--", "h_0000000b"],
            vec!["health", "--json"],
            vec!["file", "--", "h_0000000b", "REPORT.md"],
            vec![
                "steer",
                "--by=secretary",
                "--",
                "h_0000000a",
                "-- use the notes"
            ],
            vec![
                "cancel",
                "--reason=No longer needed.",
                "--by=secretary",
                "--",
                "h_0000000a"
            ],
        ]
    );
    for run in &runs {
        let mut env: Vec<&str> = run["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        env.sort_unstable();
        assert_eq!(env, ["HIVE_CONFIG", "HOME", "LANG", "PATH"], "{run}");
    }
    let log = server.stderr().join("\n");
    assert!(log.contains("ulei-hive-mcp: hive_create - ok "), "{log}");
    assert!(
        log.contains("ulei-hive-mcp: hive_show h_00000000 error "),
        "{log}"
    );
    assert!(
        !log.contains("Synthetic goal") && !log.contains("No longer needed"),
        "no field value is logged: {log}"
    );
}

#[test]
fn refusals_are_tool_errors_and_never_reach_the_cli() {
    let dir = Scratch::new();
    let server = fake(&dir, "hold = true\n");
    let session = server.session();
    let base =
        json!({"title": "T", "goal": "G", "acceptance": "true", "step": 1, "source": "manual"});
    let with = |k: &str, v: Value| {
        let mut b = base.clone();
        b[k] = v;
        b
    };
    for (args, text) in [
        (with("script", json!("/tmp/x.sh")), "unknown field `script`"),
        (
            with("created_by", json!("owner")),
            "unknown field `created_by`",
        ),
        (with("executor", json!("dots")), "unknown field `executor`"),
        (with("accept_review", json!("c")), "not both"),
        (with("acceptance", json!("done")), "shell command"),
        (with("source", json!("--deliver=local")), "source must be"),
        (with("step", json!(9)), "step must be"),
    ] {
        let result = server.call(&session, "hive_create", &args);
        assert_eq!(result["isError"], true, "{args}");
        let said = result["content"][0]["text"].as_str().unwrap();
        assert!(said.contains(text), "{said}");
    }
    let steer = server.call(
        &session,
        "hive_steer",
        &json!({"card_id": "--by=owner", "note": "n"}),
    );
    assert_eq!(steer["isError"], true);
    assert_eq!(runs(&dir), Vec::<Value>::new());

    // A card the hive refuses: its reason, as a tool error.
    let rejected = server.call(&session, "hive_create", &with("title", json!("reject")));
    assert_eq!(
        rejected,
        json!({"content": [{"type": "text", "text": "card rejected: synthetic refusal"}], "isError": true})
    );
    // `hold = true` holds every card.
    assert!(
        runs(&dir)[0]["argv"]
            .as_array()
            .unwrap()
            .contains(&json!("--hold"))
    );
}

#[test]
fn a_slow_or_flooding_cli_is_cut_off() {
    let dir = Scratch::new();
    let server = fake(&dir, "timeout = 1\n");
    let session = server.session();
    let base =
        json!({"title": "slow", "goal": "G", "acceptance": "true", "step": 1, "source": "manual"});
    let started = std::time::Instant::now();
    let slow = server.call(&session, "hive_create", &base);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(
        slow["content"][0]["text"],
        "the hive CLI did not finish within 1 s"
    );
    let mut flood = base.clone();
    flood["title"] = json!("flood");
    let flooded = server.call(&session, "hive_create", &flood);
    assert_eq!(
        flooded["content"][0]["text"],
        "the hive CLI wrote more to stdout than is read"
    );
    assert_eq!(flooded["isError"], true);
}

#[test]
fn a_refused_config_exits_2_naming_the_key() {
    let dir = Scratch::new();
    let hive_config = dir.0.join("hive.toml");
    let refused = |config: &std::path::Path| {
        let out = Command::new(SERVER)
            .args(["serve", "--config"])
            .arg(config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        String::from_utf8(out.stderr).unwrap()
    };
    let open = write_config(&dir.0, FAKE_HIVE, &hive_config, "", 0o640);
    let said = refused(&open);
    assert!(
        said.contains("hive_mcp.token_file") && said.contains("chmod 600"),
        "{said}"
    );
    assert!(!said.contains(TOKEN));
    let relative = write_config(&dir.0, "hive.py", &hive_config, "", 0o600);
    assert!(refused(&relative).contains("hive_mcp.hive_command"));
    let wide = write_config(&dir.0, FAKE_HIVE, &hive_config, "", 0o600);
    fs::write(
        &wide,
        fs::read_to_string(&wide)
            .unwrap()
            .replace("127.0.0.1:0", "0.0.0.0:0"),
    )
    .unwrap();
    assert!(refused(&wide).contains("hive_mcp.listen"));
    let unknown = write_config(
        &dir.0,
        FAKE_HIVE,
        &hive_config,
        "script = \"/tmp/x\"\n",
        0o600,
    );
    assert!(refused(&unknown).contains("unknown key hive_mcp.script"));
    let out = Command::new(SERVER).arg("--version").output().unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("ulei-hive-mcp ")
    );
}
