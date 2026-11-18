//! The built `ulei-engine` binary: `--version`, usage errors, and `run` against the fake
//! gateway with settings from a config file or the environment.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;
mod fake_mcp;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fake_gateway::{
    Answer, FakeGateway, SUMMARY_REQUEST, Scratch, call, gateway_error, request, text_step,
    tool_step, turns,
};
use fake_mcp::{FakeMcp, Reply, Spec};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::ErrorClass;
use ulei_engine::journal::{Journal, SCHEMA_VERSION};

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
        .env_remove("ULEI_ENGINE_SKILLS_DIR")
        .env_remove("ULEI_ENGINE_DISABLED_SKILLS")
        .env_remove("ULEI_ENGINE_MAIN_CHANNEL")
        .env_remove("ULEI_ENGINE_TURN_REMINDER_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_ROUTE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_LOG")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_TIMEOUT")
        .env_remove("ULEI_ENGINE_MCP_SERVERS")
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

/// Synthetic SOUL.md, MEMORY.md and USER.md written to `dir/brain`, the INDEX.md path the
/// prompt names and the skills directory; `(key in [engine], environment variable, path)` of
/// each.
fn brain(dir: &Path) -> [(&'static str, &'static str, PathBuf); 5] {
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
        ("skills_dir", "ULEI_ENGINE_SKILLS_DIR", brain.join("skills")),
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

    let usage_errors: [&[&str]; 11] = [
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
        &[
            "run",
            "--session",
            "s",
            "--route",
            "r",
            "--prompt",
            "p",
            "--event",
            "--event",
        ],
        &[
            "run",
            "--session",
            "s",
            "--route",
            "r",
            "--prompt",
            "p",
            "--channel",
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
    assert_eq!(version, SCHEMA_VERSION);
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
fn the_skills_come_from_the_configured_directory_without_the_disabled_ones() {
    let fake = FakeGateway::start(vec![text_step("ok"), text_step("ok")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    for (skill, description) in [("bread", "Bake the bread."), ("soup", "Cook the soup.")] {
        let skill_dir = dir.0.join("brain/skills/kitchen").join(skill);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill}\ndescription: {description}\n---\n"),
        )
        .unwrap();
    }
    let config = write_config(
        &dir.0,
        &format!(
            "{}disabled_skills = \" soup , \"\n",
            config_text(fake.socket(), &db, &dir.0)
        ),
    );
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = engine(&dir.0)
        .args(run_args("other", "hi"))
        .env("ULEI_CONFIG", &config)
        .env("ULEI_ENGINE_DISABLED_SKILLS", "bread")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let sent = systems(&fake);
    assert!(sent[0].contains(
        "<available_skills>\n  kitchen:\n    - bread: Bake the bread.\n</available_skills>"
    ));
    assert!(sent[1].contains(
        "<available_skills>\n  kitchen:\n    - soup: Cook the soup.\n</available_skills>"
    ));
    let tools: Vec<String> = request(&turns(&fake)[0])
        .tools
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(tools[5..], ["skills_list", "skill_view"]);
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

/// The config of [`config_text`] with `main-chat` as the main channel and, when `reminder` is
/// given, a reminder file of that text.
fn reminder_config(fake: &FakeGateway, db: &Path, dir: &Path, reminder: Option<&str>) -> PathBuf {
    let mut text = format!(
        "{}main_channel = \"main-chat\"\n",
        config_text(fake.socket(), db, dir)
    );
    if let Some(reminder) = reminder {
        let file = db.with_extension("reminder");
        fs::write(&file, reminder).unwrap();
        text = format!("{text}turn_reminder_file = \"{}\"\n", file.display());
    }
    let config = db.with_extension("toml");
    fs::write(&config, text).unwrap();
    config
}

/// Every message row and session row of the journal at `db`, as stored.
fn journal_rows(db: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut rows = Vec::new();
    for query in [
        "SELECT session || '|' || seq || '|' || turn || '|' || message FROM messages ORDER BY session, seq",
        "SELECT id || '|' || key || '|' || system_prompt FROM sessions ORDER BY id",
    ] {
        let mut statement = connection.prepare(query).unwrap();
        let found = statement.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.extend(found.map(Result::unwrap));
    }
    rows
}

#[test]
fn the_reminder_reaches_the_requests_of_a_main_channel_owner_turn_and_never_the_journal() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        text_step("written"),
        text_step("again"),
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "a.txt", "content": "a"}),
        )]),
        text_step("written"),
        text_step("again"),
    ]);
    let dir = Scratch::new();
    let on = dir.0.join("on.sqlite3");
    let off = dir.0.join("off.sqlite3");
    for (db, reminder) in [(&on, Some("Synthetic reminder.")), (&off, None)] {
        let config = reminder_config(&fake, db, &dir.0, reminder);
        for prompt in ["first", "second"] {
            let mut args = run_args("chat", prompt);
            args.extend(["--channel".into(), "main-chat".into()]);
            let out = engine(&dir.0)
                .args(args)
                .env("ULEI_CONFIG", &config)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        }
        fs::remove_file(dir.0.join("a.txt")).unwrap();
    }

    let bodies = turns(&fake);
    let user = |text: &str| ("user".to_owned(), text.to_owned());
    for body in &bodies[..2] {
        assert_eq!(texts(body)[0], user("first\n\nSynthetic reminder."));
    }
    let second = texts(&bodies[2]);
    assert_eq!(second[0], user("first"));
    assert_eq!(second[4], user("second\n\nSynthetic reminder."));
    for body in &bodies[3..] {
        assert!(!String::from_utf8_lossy(body).contains("Synthetic"));
    }
    let with: Value = serde_json::from_slice(&bodies[2]).unwrap();
    let without: Value = serde_json::from_slice(&bodies[5]).unwrap();
    assert_eq!(with["system"], without["system"]);
    assert_eq!(journal_rows(&on), journal_rows(&off));
}

#[test]
fn another_channel_an_event_no_reminder_file_and_an_empty_one_append_nothing() {
    let fake = FakeGateway::start((0..5).map(|_| text_step("ok")).collect());
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let reminding = reminder_config(&fake, &db, &dir.0, Some("Synthetic reminder."));
    let silent = reminder_config(&fake, &dir.0.join("silent.sqlite3"), &dir.0, Some(""));
    let off = reminder_config(&fake, &dir.0.join("off.sqlite3"), &dir.0, None);
    let cases: [(&Path, &[&str]); 5] = [
        (&reminding, &["--channel", "side-chat"]),
        (&reminding, &["--channel", "main-chat", "--event"]),
        (&reminding, &[]),
        (&off, &["--channel", "main-chat"]),
        (&silent, &["--channel", "main-chat"]),
    ];
    for (n, (config, flags)) in cases.iter().enumerate() {
        let out = engine(&dir.0)
            .args(run_args(&format!("chat-{n}"), "hello"))
            .args(*flags)
            .env("ULEI_CONFIG", config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr(&out));
    }
    for (body, (_, flags)) in turns(&fake).iter().zip(cases) {
        assert_eq!(
            texts(body),
            [("user".to_owned(), "hello".to_owned())],
            "{flags:?}"
        );
    }
}

#[test]
fn a_reminder_file_that_cannot_be_read_fails_the_start_and_naming_none_starts() {
    let fake = FakeGateway::start(vec![text_step("ok")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, None);
    let missing = dir.0.join("no-such.reminder");
    let directory = dir.0.join("brain");
    let cases = [
        (
            &missing,
            format!(
                "ulei-engine: engine.turn_reminder_file (ULEI_ENGINE_TURN_REMINDER_FILE) {}: No \
                 such file or directory (os error 2)\n",
                missing.display()
            ),
        ),
        (
            &directory,
            format!(
                "ulei-engine: engine.turn_reminder_file (ULEI_ENGINE_TURN_REMINDER_FILE) {}: Is a \
                 directory (os error 21)\n",
                directory.display()
            ),
        ),
    ];
    for (file, want) in cases {
        let out = engine(&dir.0)
            .args(run_args("chat", "hello"))
            .args(["--channel", "main-chat"])
            .env("ULEI_CONFIG", &config)
            .env("ULEI_ENGINE_TURN_REMINDER_FILE", file)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{want}");
        assert_eq!(stderr(&out), want);
    }
    assert!(!db.exists());
    assert_eq!(turns(&fake).len(), 0);

    let out = engine(&dir.0)
        .args(run_args("chat", "hello"))
        .args(["--channel", "main-chat"])
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
}

/// The config of [`config_text`] with `main-chat` as the main channel and the
/// `[engine.no_ask_guard]` lines `guard`; synthetic note and instructions files sit in `dir`.
fn guard_config(fake: &FakeGateway, db: &Path, dir: &Path, guard: &str) -> PathBuf {
    fs::write(dir.join("note.txt"), "[synthetic {chain}] Do it yourself.").unwrap();
    fs::write(
        dir.join("rules.txt"),
        "Rules.\n{tag}{owner}{tag}\n{tag}{reply}{tag}",
    )
    .unwrap();
    let config = db.with_extension("toml");
    fs::write(
        &config,
        format!(
            "{}main_channel = \"main-chat\"\n[engine.no_ask_guard]\n{guard}",
            config_text(fake.socket(), db, dir)
        ),
    )
    .unwrap();
    config
}

/// Every `[engine.no_ask_guard]` line with the files of [`guard_config`] in `dir`.
fn guard_lines(dir: &Path) -> String {
    format!(
        "route = \"judge\"\nnote_file = \"{}\"\ninstructions_file = \"{}\"\nlog = \"{}\"\n",
        dir.join("note.txt").display(),
        dir.join("rules.txt").display(),
        dir.join("guard.jsonl").display()
    )
}

#[test]
fn only_a_main_channel_owner_turn_with_the_guard_on_has_its_closing_question_classified() {
    let fake = FakeGateway::start(vec![
        text_step("Shall I start?"),
        text_step("{\"verdict\": \"extra\"}"),
        text_step("Started."),
        text_step("Shall I start?"),
        text_step("Shall I start?"),
        text_step("Shall I start?"),
    ]);
    let dir = Scratch::new();
    let on = guard_config(
        &fake,
        &dir.0.join("on.sqlite3"),
        &dir.0,
        &guard_lines(&dir.0),
    );
    let off = guard_config(&fake, &dir.0.join("off.sqlite3"), &dir.0, "");
    let cases: [(&Path, &[&str], &str); 4] = [
        (&on, &["--channel", "main-chat"], "Started.\n"),
        (
            &on,
            &["--channel", "main-chat", "--event"],
            "Shall I start?\n",
        ),
        (&on, &["--channel", "side-chat"], "Shall I start?\n"),
        (&off, &["--channel", "main-chat"], "Shall I start?\n"),
    ];
    for (n, (config, flags, want)) in cases.iter().enumerate() {
        let out = engine(&dir.0)
            .args(run_args(&format!("chat-{n}"), "Sort the photos."))
            .args(*flags)
            .env("ULEI_CONFIG", config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr(&out));
        assert_eq!(stdout(&out), *want, "{flags:?}");
    }

    let routes: Vec<String> = turns(&fake).iter().map(|b| request(b).route).collect();
    assert_eq!(routes, ["main", "judge", "main", "main", "main", "main"]);
    let judged = texts(&turns(&fake)[1]);
    assert!(judged[0].1.contains("Sort the photos."), "{judged:?}");
    let log: Vec<Value> = fs::read_to_string(dir.0.join("guard.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0]["action"], "suppressed_continue");
    assert_eq!(log[0]["session_id"], "chat-0");
    assert_eq!(log[1]["action"], "continuation_done");
    assert_eq!(log[1]["chain"], log[0]["chain"]);
    let chain = log[0]["chain"].as_str().unwrap();
    let mut journal = Journal::open(&dir.0.join("on.sqlite3")).unwrap();
    let session = journal.session("chat-0").unwrap();
    let stored: Vec<String> = journal
        .messages(session.id)
        .unwrap()
        .iter()
        .map(ulei_engine::agent::text)
        .collect();
    assert_eq!(
        stored,
        [
            "Sort the photos.".to_owned(),
            "Shall I start?".to_owned(),
            format!("[synthetic {chain}] Do it yourself."),
            "Started.".to_owned(),
        ]
    );
}

#[test]
fn the_guard_on_without_a_readable_template_fails_the_start_naming_the_setting_and_off_starts() {
    let fake = FakeGateway::start(vec![text_step("ok")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let missing = dir.0.join("no-such.txt");
    let directory = dir.0.join("brain");
    let lines = guard_lines(&dir.0);
    let without_note: String = lines
        .lines()
        .filter(|l| !l.starts_with("note_file"))
        .flat_map(|l| [l, "\n"])
        .collect();
    let bad_rules = dir.0.join("bad-rules.txt");
    fs::write(&bad_rules, "Rules {tag} {owner}").unwrap();
    let cases = [
        (
            &without_note,
            None,
            "ulei-engine: missing engine.no_ask_guard.note_file: set it in the config file or in \
             ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE\n"
                .to_owned(),
        ),
        (
            &lines,
            Some(("ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE", &missing)),
            format!(
                "ulei-engine: engine.no_ask_guard.note_file (ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE) \
                 {}: No such file or directory (os error 2)\n",
                missing.display()
            ),
        ),
        (
            &lines,
            Some(("ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE", &directory)),
            format!(
                "ulei-engine: engine.no_ask_guard.instructions_file \
                 (ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE) {}: Is a directory (os error 21)\n",
                directory.display()
            ),
        ),
        (
            &lines,
            Some(("ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE", &bad_rules)),
            "ulei-engine: engine.no_ask_guard.instructions_file \
             (ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE): no {reply} placeholder\n"
                .to_owned(),
        ),
    ];
    for (guard, env, want) in cases {
        let config = guard_config(&fake, &db, &dir.0, guard);
        let mut command = engine(&dir.0);
        command
            .args(run_args("chat", "hello"))
            .args(["--channel", "main-chat"])
            .env("ULEI_CONFIG", &config);
        if let Some((name, path)) = env {
            command.env(name, path);
        }
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{want}");
        assert_eq!(stderr(&out), want);
    }
    assert!(!db.exists());
    assert_eq!(turns(&fake).len(), 0);

    let config = guard_config(&fake, &db, &dir.0, "");
    let out = engine(&dir.0)
        .args(run_args("chat", "hello"))
        .args(["--channel", "main-chat"])
        .env("ULEI_CONFIG", &config)
        .env("ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE", &missing)
        .env("ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE", &missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
}

#[test]
fn mcp_header_values_stay_out_of_tool_results_the_journal_and_the_error_lines() {
    let token = "synthetic-token-c41d77";
    let leak = format!("rejected {token}");
    let mcp = FakeMcp::start(Spec::new(vec![json!({"name": "leak"})]).reply(
        "leak",
        Reply::Json(json!({"content": [{"type": "text", "text": leak}], "isError": true})),
    ));
    let refusing = FakeMcp::start(Spec {
        reject_initialize: Some(format!("{token} is not accepted")),
        ..Spec::new(Vec::new())
    });
    let fake = FakeGateway::start(vec![
        tool_step(vec![call("c1", "mcp__notes__leak", json!({}))]),
        text_step("done"),
    ]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let env_file = dir.0.join("notes.env");
    fs::write(&env_file, format!("Authorization=Bearer {token}\n")).unwrap();
    let servers = format!(
        "mcp_servers = \"notes, refusing\"\n[engine.mcp.notes]\nurl = \"{}\"\nenv_file = \"{}\"\n\
         [engine.mcp.refusing]\nurl = \"{}\"\nenv_file = \"{}\"\nconnect_timeout = \"0.5\"\n",
        mcp.url(),
        env_file.display(),
        refusing.url(),
        env_file.display()
    );
    let config = write_config(
        &dir.0,
        &(config_text(fake.socket(), &db, &dir.0) + &servers),
    );

    let out = engine(&dir.0)
        .args(run_args("chat", "check the notes"))
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "done\n");
    assert_eq!(
        stderr(&out),
        "ulei-engine: Failed to connect to MCP server 'refusing': [REDACTED] is not accepted\n"
    );
    let sent = String::from_utf8(turns(&fake)[1].clone()).unwrap();
    assert!(
        sent.contains(r#"{\"error\": \"rejected [REDACTED]\"}"#),
        "{sent}"
    );
    assert!(!sent.contains(token));
    assert!(
        !fs::read(&db)
            .unwrap()
            .windows(token.len())
            .any(|w| w == token.as_bytes())
    );
    let call = mcp
        .seen()
        .into_iter()
        .find(|s| s.method() == "tools/call")
        .unwrap();
    assert_eq!(
        call.header("Authorization"),
        Some(format!("Bearer {token}").as_str())
    );
}
