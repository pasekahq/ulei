//! The built `ulei-engine` binary: `--version`, usage errors, and `run` against the fake
//! gateway with settings from a config file or the environment.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fake_gateway::{
    Answer, FakeGateway, SUMMARY_REQUEST, Scratch, call, gateway_error, request, text_step,
    tool_step, turns,
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
        .env_remove("ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT")
        .env_remove("ULEI_ENGINE_MAX_STEPS")
        .env_remove("ULEI_ENGINE_EMPTY_RESPONSE_GUARD_ENABLED")
        .env_remove("ULEI_ENGINE_SERVICE_UNIT")
        .env_remove("ULEI_ENGINE_SOUL_FILE")
        .env_remove("ULEI_ENGINE_INDEX_FILE")
        .env_remove("ULEI_ENGINE_MEMORY_FILE")
        .env_remove("ULEI_ENGINE_USER_FILE")
        .env_remove("ULEI_ENGINE_MEMORY_CHAR_LIMIT")
        .env_remove("ULEI_ENGINE_USER_CHAR_LIMIT")
        .env_remove("ULEI_ENGINE_TIMEZONE")
        .env_remove("INVOCATION_ID");
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

/// Spill files go to `terminal-output` next to the journal; the prompt files are those of
/// [`brain`] in `dir`.
fn config_text(socket: &Path, journal: &Path, dir: &Path) -> String {
    let brain = brain(dir)
        .iter()
        .map(|(key, _, path)| format!("{key} = \"{}\"\n", path.display()))
        .collect::<Vec<_>>()
        .concat();
    format!(
        "# engine settings\n[qb]\nsocket = \"{}\"  # gateway\n\n[engine]\njournal = \"{}\"\n{brain}max_steps = 8\nterminal_output = \"{}\"\n",
        socket.display(),
        journal.display(),
        spill_dir(journal).display()
    )
}

/// Synthetic SOUL.md, MEMORY.md and USER.md written to `dir/brain`, and the INDEX.md path the
/// prompt names; `(key in [engine], environment variable, path)` of each.
fn brain(dir: &Path) -> [(&'static str, &'static str, PathBuf); 4] {
    let brain = dir.join("brain");
    fs::create_dir_all(&brain).unwrap();
    fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").unwrap();
    fs::write(brain.join("MEMORY.md"), "Notes live in the test brain.\n").unwrap();
    fs::write(brain.join("USER.md"), "The tester prefers short answers.\n").unwrap();
    [
        ("soul_file", "ULEI_ENGINE_SOUL_FILE", brain.join("SOUL.md")),
        (
            "index_file",
            "ULEI_ENGINE_INDEX_FILE",
            brain.join("INDEX.md"),
        ),
        (
            "memory_file",
            "ULEI_ENGINE_MEMORY_FILE",
            brain.join("MEMORY.md"),
        ),
        ("user_file", "ULEI_ENGINE_USER_FILE", brain.join("USER.md")),
    ]
}

/// The environment settings of [`brain`] in `dir`.
fn brain_env(dir: &Path) -> impl Iterator<Item = (&'static str, PathBuf)> {
    brain(dir).into_iter().map(|(_, env, path)| (env, path))
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
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));

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
        .envs(brain_env(&dir.0))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "still here\n");

    // `--config` with a wrong socket that the environment overrides; `--new` sends no history.
    let wrong = write_config(
        &dir.0,
        &config_text(&dir.0.join("no-such.sock"), &db, &dir.0),
    );
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
            config_text(fake.socket(), &db, &dir.0)
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
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));

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
        .envs(brain_env(&dir.0))
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
fn the_step_limit_comes_from_the_environment_then_the_config_file_and_defaults_to_200() {
    let dir = Scratch::new();
    let cases = [
        (Some("max_steps = 8"), Some("1"), 1),
        (Some("max_steps = 2"), None, 2),
        (None, None, 200),
    ];
    for (i, (file, env, steps)) in cases.into_iter().enumerate() {
        let mut script: Vec<Answer> = (0..steps)
            .map(|n| {
                tool_step(vec![call(
                    &format!("c{n}"),
                    "read_file",
                    json!({"path": "missing.txt"}),
                )])
            })
            .collect();
        script.push(text_step("Summary."));
        let fake = FakeGateway::start(script);
        let db = dir.0.join(format!("journal-{i}.sqlite3"));
        let text =
            config_text(fake.socket(), &db, &dir.0).replace("max_steps = 8", file.unwrap_or(""));
        let mut command = engine(&dir.0);
        command
            .args(run_args("chat", "look"))
            .env("ULEI_CONFIG", write_config(&dir.0, &text));
        if let Some(value) = env {
            command.env("ULEI_ENGINE_MAX_STEPS", value);
        }
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stdout(&out), "Summary.\n");

        let sent = turns(&fake);
        assert_eq!(sent.len(), steps + 1, "{file:?} {env:?}");
        let last = request(&sent[steps]);
        assert_eq!(last.tools, None);
        assert_eq!(
            texts(&sent[steps]).last(),
            Some(&("user".to_owned(), SUMMARY_REQUEST.to_owned()))
        );
        assert!(request(&sent[steps - 1]).tools.is_some());
    }
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
        .envs(brain_env(&dir.0))
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
        let mut text = config_text(
            fake.socket(),
            &dir.0.join(format!("journal-{i}.sqlite3")),
            &dir.0,
        );
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
        (
            format!(
                "[qb]\nsocket = \"{}\"\n[engine]\njournal = \"{}\"\nterminal_output = \"{}\"\n",
                socket.display(),
                db.display(),
                dir.0.join("terminal-output").display()
            ),
            "ulei-engine: missing engine.soul_file: set it in the config file or in \
             ULEI_ENGINE_SOUL_FILE\n",
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

#[test]
fn the_service_unit_comes_from_the_environment_then_the_config_file_and_guards_terminal() {
    let dir = Scratch::new();
    let ran = dir.0.join("ran");
    let command = format!(
        "true systemctl stop ulei-engine-secretary; touch {}",
        ran.display()
    );
    let cases = [
        (Some("ulei-engine-secretary.service"), None, false, true),
        (None, Some("ulei-engine-secretary"), true, true),
        (
            Some("ulei-engine-other.service"),
            Some("ulei-engine-secretary"),
            false,
            true,
        ),
        (
            Some("ulei-engine-secretary.service"),
            Some("ulei-engine-other"),
            false,
            false,
        ),
        (None, None, false, false),
    ];
    for (i, (file, env, under_systemd, blocked)) in cases.into_iter().enumerate() {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call("c1", "terminal", json!({"command": command}))]),
            text_step("seen"),
        ]);
        let mut text = config_text(
            fake.socket(),
            &dir.0.join(format!("journal-{i}.sqlite3")),
            &dir.0,
        );
        if let Some(unit) = file {
            text = format!("{text}service_unit = \"{unit}\"\n");
        }
        let mut engine = engine(&dir.0);
        engine
            .args(run_args("chat", "stop"))
            .env("ULEI_CONFIG", write_config(&dir.0, &text));
        if let Some(unit) = env {
            engine.env("ULEI_ENGINE_SERVICE_UNIT", unit);
        }
        if under_systemd {
            engine.env("INVOCATION_ID", "0123456789abcdef");
        }
        let out = engine.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

        let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
        let result = sent["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap();
        let result: Value = serde_json::from_str(result).unwrap();
        let case = format!("file {file:?}, env {env:?}");
        assert_eq!(result["exit_code"], i32::from(blocked), "{case}");
        assert_eq!(ran.exists(), !blocked, "{case}");
        let _ = fs::remove_file(&ran);
    }
}

#[test]
fn an_empty_or_invalid_service_unit_refuses_to_start() {
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let cases = [
        (
            "",
            "ulei-engine: engine.service_unit (ULEI_ENGINE_SERVICE_UNIT) is empty\n",
        ),
        (
            "ulei engine",
            "ulei-engine: \"ulei engine\" is not a service unit name; set engine.service_unit \
             (ULEI_ENGINE_SERVICE_UNIT)\n",
        ),
    ];
    for (unit, want) in cases {
        let out = engine(&dir.0)
            .args(run_args("chat", "hi"))
            .env("ULEI_QB_SOCKET", dir.0.join("gw.sock"))
            .env("ULEI_ENGINE_JOURNAL", &db)
            .env("ULEI_ENGINE_TERMINAL_OUTPUT", dir.0.join("terminal-output"))
            .env("ULEI_ENGINE_SERVICE_UNIT", unit)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{unit:?}");
        assert_eq!(stderr(&out), want, "{unit:?}");
    }
    assert!(!db.exists());
}

/// The `system` of each recorded turn request.
fn systems(fake: &FakeGateway) -> Vec<String> {
    turns(fake)
        .iter()
        .map(|body| request(body).system.unwrap())
        .collect()
}

#[test]
fn the_system_prompt_is_built_once_per_session_and_survives_brain_changes_and_restarts() {
    let fake = FakeGateway::start(vec![text_step("one"), text_step("two"), text_step("three")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));
    let run = |prompt: &str, new: bool| {
        let mut args = run_args("chat", prompt);
        if new {
            args.push("--new".into());
        }
        let out = engine(&dir.0)
            .args(args)
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    };
    let stored = || {
        Journal::open(&db)
            .unwrap()
            .session("chat")
            .unwrap()
            .system_prompt
    };

    run("first", false);
    fs::write(
        dir.0.join("brain/SOUL.md"),
        "You are the changed assistant.\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("brain/MEMORY.md"),
        "A note written mid-session.\n",
    )
    .unwrap();
    run("second", false);
    let first = systems(&fake);
    assert_eq!(stored().as_ref(), Some(&first[0]));
    run("third", true);

    let sent = systems(&fake);
    assert!(sent[0].starts_with(&format!(
        "You are the test assistant.\n\nKnowledge base index: {}. ",
        dir.0.join("brain/INDEX.md").display()
    )));
    assert!(sent[0].contains(
        "]\n══════════════════════════════════════════════\nNotes live in the test brain.\n\n"
    ));
    assert!(sent[0].contains(&format!(
        "\nCurrent working directory: {}\n",
        dir.0.display()
    )));
    assert_eq!(sent[1], sent[0]);
    assert!(sent[2].starts_with("You are the changed assistant.\n\n"));
    assert!(sent[2].contains("\nA note written mid-session.\n\n"));
    assert_eq!(stored().as_ref(), Some(&sent[2]));
}

#[test]
fn a_session_of_a_version_1_journal_gets_its_prompt_built_and_stored_on_its_next_turn() {
    let fake = FakeGateway::start(vec![text_step("again"), text_step("still")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, key TEXT NOT NULL);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
INSERT INTO sessions (key) VALUES ('chat');
INSERT INTO messages VALUES (1, 0, 1, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"earlier\"}]}');
INSERT INTO messages VALUES (1, 1, 1, '{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"noted\"}]}');
INSERT INTO steps VALUES (1, 1, 0, 's1-t1-0', NULL, NULL);
PRAGMA user_version=1;",
        )
        .unwrap();
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));
    for prompt in ["are you there", "and now"] {
        let out = engine(&dir.0)
            .args(run_args("chat", prompt))
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }

    let bodies = turns(&fake);
    let sent = systems(&fake);
    assert!(sent[0].starts_with("You are the test assistant.\n\n"));
    assert_eq!(sent[1], sent[0]);
    assert_eq!(
        texts(&bodies[0]),
        [
            ("user".to_owned(), "earlier".to_owned()),
            ("assistant".to_owned(), "noted".to_owned()),
            ("user".to_owned(), "are you there".to_owned())
        ]
    );
    assert_eq!(request(&bodies[0]).request_id, "s1-t2-0");
    let session = Journal::open(&db).unwrap().session("chat").unwrap();
    assert_eq!(
        (session.id, session.system_prompt),
        (1, Some(sent[0].clone()))
    );
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn the_char_limits_come_from_the_environment_then_the_config_file() {
    let fake = FakeGateway::start(vec![text_step("ok")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let text = format!(
        "{}memory_char_limit = 10\nuser_char_limit = 7\n",
        config_text(fake.socket(), &db, &dir.0)
    );
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .env("ULEI_CONFIG", write_config(&dir.0, &text))
        .env("ULEI_ENGINE_USER_CHAR_LIMIT", "5")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let sent = &systems(&fake)[0];
    assert!(sent.contains("\nMEMORY (your personal notes) [100% — 29/10 chars]\n"));
    assert!(sent.contains("\nNotes live in the test brain.\n\n"));
    assert!(sent.contains("\nUSER PROFILE (who the user is) [100% — 33/5 chars]\n"));
    assert!(sent.contains("\nThe tester prefers short answers.\n\nConversation started: "));
}

#[test]
fn a_prompt_that_cannot_be_built_fails_the_turn_before_the_gateway() {
    let fake = FakeGateway::start(vec![]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));
    let soul = dir.0.join("brain/SOUL.md");
    let cases = [
        (
            Some(("ULEI_ENGINE_MEMORY_CHAR_LIMIT", "many")),
            "ulei-engine: engine.memory_char_limit (ULEI_ENGINE_MEMORY_CHAR_LIMIT) must be a \
             non-negative integer\n"
                .to_owned(),
        ),
        (
            Some(("ULEI_ENGINE_TIMEZONE", "Nowhere/Zone")),
            "ulei-engine: time zone \"Nowhere/Zone\" is not in the zone database\n".to_owned(),
        ),
        (
            None,
            format!(
                "ulei-engine: SOUL.md {} is missing or blank\n",
                soul.display()
            ),
        ),
    ];
    for (env, want) in cases {
        if env.is_none() {
            fs::remove_file(&soul).unwrap();
        }
        let mut command = engine(&dir.0);
        command
            .args(run_args("chat", "hi"))
            .env("ULEI_CONFIG", &config);
        if let Some((name, value)) = env {
            command.env(name, value);
        }
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{want}");
        assert_eq!(stderr(&out), want);
    }
    assert_eq!(turns(&fake).len(), 0);
    let mut journal = Journal::open(&db).unwrap();
    assert_eq!(journal.messages(1).unwrap(), []);
    assert_eq!(journal.session("chat").unwrap().system_prompt, None);
}
