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
        .env_remove("ULEI_ENGINE_JOURNAL");
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

fn config_text(socket: &Path, journal: &Path) -> String {
    format!(
        "# engine settings\n[qb]\nsocket = \"{}\"  # gateway\n\n[engine]\njournal = \"{}\"\nmax_steps = 8\n",
        socket.display(),
        journal.display()
    )
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
            "write",
            json!({"file_path": "note.txt", "content": "hello"}),
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
        ("tools = [\"shell\"]", "unsupported value"),
        ("name = 'literal'", "unsupported value"),
        ("ratio = 1.5", "unsupported value"),
        ("big = 1_000", "unsupported value"),
        ("flag = true", "unsupported value"),
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
