//! The built `ulei-engine` binary: `--version`, usage errors, and `run` against the fake
//! gateway with settings from a config file or the environment.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fake_gateway::{
    Answer, FakeGateway, Scratch, call, gateway_error, request, text_step, tool_step, turns,
};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::ErrorClass;
use ulei_engine::journal::Journal;

const BIN: &str = env!("CARGO_BIN_EXE_ulei-engine");

/// The binary in `cwd` with no inherited engine settings.
fn engine(cwd: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(cwd)
        .env_remove("ULEI_CONFIG")
        .env_remove("ULEI_QB_SOCKET")
        .env_remove("ULEI_ENGINE_JOURNAL")
        .env_remove("ULEI_ENGINE_TERMINAL_OUTPUT")
        .env_remove("ULEI_SECURITY_REDACT_SECRETS")
        .env_remove("ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT");
    command
}

fn run_args(session: &str, prompt: &str) -> Vec<String> {
    [
        "run",
        "--session",
        session,
        "--route",
        "main",
        "--prompt",
        prompt,
    ]
    .map(String::from)
    .to_vec()
}

fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("ulei.toml");
    fs::write(&path, text).unwrap();
    path
}

/// Spill files go to `terminal-output` next to the journal.
fn config_text(socket: &Path, journal: &Path) -> String {
    format!(
        "# engine settings\n[qb]\nsocket = \"{}\"  # gateway\n\n[engine]\njournal = \"{}\"\nmax_steps = 8\nterminal_output = \"{}\"\n",
        socket.display(),
        journal.display(),
        spill_dir(journal).display()
    )
}

fn spill_dir(journal: &Path) -> PathBuf {
    journal.with_file_name("terminal-output")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `(role, text)` of each message of a recorded request, text blocks only.
fn texts(body: &[u8]) -> Vec<(String, String)> {
    let value: Value = serde_json::from_slice(body).unwrap();
    value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let text = m["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<String>();
            (m["role"].as_str().unwrap().to_owned(), text)
        })
        .collect()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn version_succeeds_and_other_invocations_print_usage() {
    let dir = Scratch::new();
    let out = engine(&dir.0).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        format!("ulei-engine {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(stderr(&out), "");

    let usage_errors: [&[&str]; 9] = [
        &[],
        &["--help"],
        &["run"],
        &["--version", "extra"],
        &["run", "--session", "s", "--route", "r"],
        &["run", "--session", "s", "--route", "r", "--prompt"],
        &[
            "run",
            "--session",
            "s",
            "--session",
            "t",
            "--route",
            "r",
            "--prompt",
            "p",
        ],
        &[
            "run",
            "--session",
            "s",
            "--route",
            "r",
            "--prompt",
            "p",
            "--new",
            "--new",
        ],
        &[
            "run",
            "--session",
            "s",
            "--route",
            "r",
            "--prompt",
            "p",
            "--model",
            "m",
        ],
    ];
    for args in usage_errors {
        let out = engine(&dir.0).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&out), "", "{args:?} wrote stdout");
        let err = stderr(&out);
        assert!(
            err.starts_with("usage: ulei-engine") && err.lines().count() == 1,
            "{args:?}: {err:?}"
        );
    }
}

#[test]
fn a_session_continues_across_invocations_and_new_starts_empty() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "note.txt", "content": "hello"}),
        )]),
        text_step("written"),
        text_step("still here"),
        text_step("fresh"),
    ]);
    let dir = Scratch::new();
    let work = dir.0.join("work");
    fs::create_dir(&work).unwrap();
    let state = dir.0.join("state").join("engine");
    let db = state.join("journal.sqlite3");
    let config = write_config(&dir.0, &config_text(fake.socket(), &db));

    // Settings from the file named by ULEI_CONFIG; the tool runs in the working directory.
    let out = engine(&work)
        .args(run_args("chat", "write a note"))
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "written\n");
    assert_eq!(stderr(&out), "");
    assert_eq!(fs::read_to_string(work.join("note.txt")).unwrap(), "hello");
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&db), 0o600);
    let journal = Journal::open(&db).unwrap();
    assert_eq!(journal.messages(1).unwrap().len(), 4);
    let ids: Vec<String> = journal
        .steps(1)
        .unwrap()
        .into_iter()
        .map(|s| s.request_id)
        .collect();
    assert_eq!(ids, ["s1-t1-0", "s1-t1-1"]);
    drop(journal);

    // Settings from the environment only: the next turn sends the stored conversation.
    let out = engine(&work)
        .args(run_args("chat", "are you there"))
        .env("ULEI_QB_SOCKET", fake.socket())
        .env("ULEI_ENGINE_JOURNAL", &db)
        .env("ULEI_ENGINE_TERMINAL_OUTPUT", spill_dir(&db))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "still here\n");

    // `--config` with a wrong socket that the environment overrides; `--new` sends no history.
    let wrong = write_config(&dir.0, &config_text(&dir.0.join("no-such.sock"), &db));
    let mut args = run_args("chat", "start over");
    args.extend([
        "--new".into(),
        "--config".into(),
        wrong.display().to_string(),
    ]);
    let out = engine(&work)
        .args(args)
        .env("ULEI_QB_SOCKET", fake.socket())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "fresh\n");

    let bodies = turns(&fake);
    assert_eq!(bodies.len(), 4);
    let first_last: Value = serde_json::from_slice(&bodies[1]).unwrap();
    let second: Value = serde_json::from_slice(&bodies[2]).unwrap();
    let earlier = first_last["messages"].as_array().unwrap();
    let sent = second["messages"].as_array().unwrap();
    assert_eq!(sent.len(), 5);
    assert_eq!(sent[..3], earlier[..]);
    assert_eq!(
        texts(&bodies[2])[3..],
        [
            ("assistant".to_owned(), "written".to_owned()),
            ("user".to_owned(), "are you there".to_owned())
        ]
    );
    assert_eq!(request(&bodies[2]).request_id, "s1-t2-0");
    assert_eq!(request(&bodies[2]).session_key.as_deref(), Some("chat"));
    assert_eq!(
        texts(&bodies[3]),
        [("user".to_owned(), "start over".to_owned())]
    );
    assert_eq!(request(&bodies[3]).request_id, "s2-t1-0");
}

#[test]
fn the_results_of_a_parallel_step_are_journaled_and_sent_in_call_order() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![
            call("c1", "read_file", json!({"path": "a.txt"})),
            call("c2", "search_files", json!({"pattern": "b-line"})),
            call("c3", "read_file", json!({"path": "b.txt"})),
        ]),
        text_step("read"),
    ]);
    let dir = Scratch::new();
    let work = dir.0.join("work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.txt"), "a-line\n").unwrap();
    fs::write(work.join("b.txt"), "b-line\n").unwrap();
    let db = dir.0.join("journal.sqlite3");
    let config = write_config(
        &dir.0,
        &format!(
            "{}concurrent_tool_timeout = 30\n",
            config_text(fake.socket(), &db)
        ),
    );

    let out = engine(&work)
        .args(run_args("chat", "read both"))
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "read\n");

    let journal = Journal::open(&db).unwrap();
    let messages = journal.messages(1).unwrap();
    assert_eq!(messages.len(), 4);
    let results: Vec<(String, String)> = messages[2]
        .content
        .iter()
        .map(|b| (b.id.clone().unwrap(), b.content.clone().unwrap()))
        .collect();
    let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert!(results[0].1.contains("1|a-line"), "{results:?}");
    assert!(results[1].1.contains("b.txt"), "{results:?}");
    assert!(results[2].1.contains("1|b-line"), "{results:?}");
    let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
    assert_eq!(
        sent["messages"][2],
        serde_json::to_value(&messages[2]).unwrap()
    );

    let out = engine(&work)
        .args(run_args("chat", "again"))
        .env("ULEI_CONFIG", &config)
        .env("ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT", "soon")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "ulei-engine: engine.concurrent_tool_timeout (ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT) must \
         be a number of seconds\n"
    );
}

#[test]
fn quota_exhausted_exits_3_with_its_line_and_keeps_the_prompt() {
    let mut quota = gateway_error(ErrorClass::QuotaExhausted);
    quota.resets_at = Some(1_700_000_000);
    let fake = FakeGateway::start(vec![Answer::Reject(429, quota), text_step("back")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = write_config(&dir.0, &config_text(fake.socket(), &db));

    let out = engine(&dir.0)
        .args(run_args("chat", "first"))
        .args(["--config".as_ref(), config.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "");
    assert_eq!(
        stderr(&out),
        "quota exhausted on route main; nearest reset 2023-11-14 22:13:20 UTC\n"
    );

    let out = engine(&dir.0)
        .args(run_args("chat", "second"))
        .args(["--config".as_ref(), config.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        texts(&turns(&fake)[1]),
        [
            ("user".to_owned(), "first".to_owned()),
            ("user".to_owned(), "second".to_owned())
        ]
    );
}

#[test]
fn a_gateway_failure_exits_1_with_one_line() {
    let fake = FakeGateway::start(vec![Answer::Reject(
        503,
        gateway_error(ErrorClass::Unavailable),
    )]);
    let dir = Scratch::new();
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .env("ULEI_QB_SOCKET", fake.socket())
        .env("ULEI_ENGINE_JOURNAL", dir.0.join("journal.sqlite3"))
        .env("ULEI_ENGINE_TERMINAL_OUTPUT", dir.0.join("terminal-output"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "");
    assert_eq!(
        stderr(&out),
        "ulei-engine: gateway error unavailable: no message\n"
    );
}

#[test]
fn terminal_output_beyond_the_cap_is_saved_in_the_configured_directory() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "terminal",
            json!({"command": "seq 1 30000"}),
        )]),
        text_step("seen"),
    ]);
    let dir = Scratch::new();
    let spill = dir.0.join("spill");
    let out = engine(&dir.0)
        .args(run_args("chat", "count"))
        .env("ULEI_QB_SOCKET", fake.socket())
        .env("ULEI_ENGINE_JOURNAL", dir.0.join("journal.sqlite3"))
        .env("ULEI_ENGINE_TERMINAL_OUTPUT", &spill)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
    let result = sent["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap();
    let result: Value = serde_json::from_str(result).unwrap();
    let path = PathBuf::from(result["full_output_path"].as_str().unwrap());
    assert_eq!(path.parent(), Some(spill.as_path()));
    let full: String = (1..=30_000).map(|i| i.to_string() + "\n").collect();
    assert_eq!(fs::read_to_string(&path).unwrap(), full);
}

#[test]
fn the_redaction_switch_comes_from_the_environment_then_the_config_file_and_is_on_by_default() {
    let token = format!("{}{}{}", "gh", "p_", "fake".repeat(6));
    let masked = "ghp_fa...fake";
    let dir = Scratch::new();
    let cases = [
        (None, None, masked),
        (Some("false"), None, token.as_str()),
        (Some("\"yes\""), None, masked),
        (Some("false"), Some("ON"), masked),
        (Some("true"), Some("0"), token.as_str()),
    ];
    for (i, (file, env, want)) in cases.into_iter().enumerate() {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call(
                "c1",
                "terminal",
                json!({"command": format!("echo {token}")}),
            )]),
            text_step("seen"),
        ]);
        let mut text = config_text(fake.socket(), &dir.0.join(format!("journal-{i}.sqlite3")));
        if let Some(value) = file {
            text = format!("{text}[security]\nredact_secrets = {value}\n");
        }
        let mut command = engine(&dir.0);
        command
            .args(run_args("chat", "echo"))
            .env("ULEI_CONFIG", write_config(&dir.0, &text));
        if let Some(value) = env {
            command.env("ULEI_SECURITY_REDACT_SECRETS", value);
        }
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

        let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
        let result = sent["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap();
        let result: Value = serde_json::from_str(result).unwrap();
        assert_eq!(result["output"], want, "file {file:?}, env {env:?}");
    }
}

#[test]
fn a_missing_setting_names_its_key_and_variable() {
    let dir = Scratch::new();
    let socket = dir.0.join("gw.sock");
    let db = dir.0.join("journal.sqlite3");
    let cases = [
        (
            format!("[engine]\njournal = \"{}\"\n", db.display()),
            "ulei-engine: missing qb.socket: set it in the config file or in ULEI_QB_SOCKET\n",
        ),
        (
            format!("[qb]\nsocket = \"{}\"\n", socket.display()),
            "ulei-engine: missing engine.journal: set it in the config file or in ULEI_ENGINE_JOURNAL\n",
        ),
        (
            format!(
                "[qb]\nsocket = 7\n[engine]\njournal = \"{}\"\n",
                db.display()
            ),
            "ulei-engine: qb.socket in the config file must be a string\n",
        ),
        (
            format!(
                "[qb]\nsocket = \"{}\"\n[engine]\njournal = \"{}\"\n",
                socket.display(),
                db.display()
            ),
            "ulei-engine: missing engine.terminal_output: set it in the config file or in \
             ULEI_ENGINE_TERMINAL_OUTPUT\n",
        ),
    ];
    for (text, want) in cases {
        let config = write_config(&dir.0, &text);
        let out = engine(&dir.0)
            .args(run_args("chat", "hi"))
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{text}");
        assert_eq!(stderr(&out), want, "{text}");
    }
    assert!(!db.exists());
}

#[test]
fn an_unsupported_config_line_is_named() {
    let dir = Scratch::new();
    let cases = [
        ("tools = [\"terminal\"]", "unsupported value"),
        ("name = 'literal'", "unsupported value"),
        ("ratio = 1.5", "unsupported value"),
        ("big = 1_000", "unsupported value"),
        ("flag = True", "unsupported value"),
        ("a.b = \"x\"", "unsupported key"),
        ("\"quoted\" = \"x\"", "unsupported key"),
        ("path = \"a\\nb\"", "escapes in strings are not supported"),
        ("path = \"open", "unterminated string"),
        ("path = \"a\" \"b\"", "unexpected text after the value"),
        ("[[servers]]", "unsupported section header"),
        ("[engine", "unterminated section header"),
        ("socket", "expected key = value"),
        ("journal = \"again\"", "duplicate key"),
        ("[qb]", "duplicate section"),
    ];
    for (line, reason) in cases {
        let config = write_config(
            &dir.0,
            &format!("# settings\n[qb]\njournal = \"/x\"\n{line}\n"),
        );
        let out = engine(&dir.0)
            .args(run_args("chat", "hi"))
            .env("ULEI_CONFIG", &config)
            .env("ULEI_QB_SOCKET", dir.0.join("gw.sock"))
            .env("ULEI_ENGINE_JOURNAL", dir.0.join("journal.sqlite3"))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{line}");
        assert_eq!(
            stderr(&out),
            format!(
                "ulei-engine: config {} line 4: {reason}\n",
                config.display()
            ),
            "{line}"
        );
    }
}
