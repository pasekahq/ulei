//! The built `ulei-engine` binary: `--version`, usage errors, and `run` against the fake
//! gateway with settings from a config file or the environment, and a signal during its turn.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;
mod fake_mcp;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use fake_gateway::{
    Answer, FakeGateway, SUMMARY_REQUEST, Scratch, call, cancels, dead, gateway_error, json_bytes,
    pid_file, request, route, stop, text, text_step, thinking, tool_step, turns, untagged,
    usage_of,
};
use fake_mcp::{FakeMcp, Reply, Spec};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{BlockType, Error as GatewayError, ErrorClass, StopReason};
use ulei_engine::journal::{Journal, SCHEMA_VERSION};

const BIN: &str = env!("CARGO_BIN_EXE_ulei-engine");

/// The binary in `cwd` with no inherited engine settings; with neither `--config` nor
/// `ULEI_CONFIG` it looks for `cwd/ulei/ulei.toml`.
/// The built-in tools every turn advertises after the five file and shell tools, in order.
const LATER_TOOLS: [&str; 8] = [
    "skills_list",
    "skill_view",
    "memory",
    "session_search",
    "forget",
    "archive_search",
    "archive_read",
    "set_model",
];

fn engine(cwd: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(cwd)
        .env_remove("ULEI_CONFIG")
        .env("XDG_CONFIG_HOME", cwd)
        .env_remove("ULEI_QB_SOCKET")
        .env_remove("ULEI_GARDEN_SOCKET")
        .env_remove("ULEI_GARDEN_CORE_FILE")
        .env_remove("ULEI_GARDEN_CORE_CHAR_LIMIT")
        .env_remove("ULEI_MEMORY_SOCKET")
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
        .env_remove("ULEI_ENGINE_SKILLS_INDEX")
        .env_remove("ULEI_ENGINE_TASK_BOARD_FILE")
        .env_remove("ULEI_ENGINE_MAIN_CHANNEL")
        .env_remove("ULEI_ENGINE_TURN_REMINDER_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_ROUTE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_LOG")
        .env_remove("ULEI_ENGINE_NO_ASK_GUARD_TIMEOUT")
        .env_remove("ULEI_ENGINE_MCP_SERVERS")
        .env_remove("ULEI_ENGINE_PROMPT_CACHE")
        .env_remove("ULEI_ENGINE_HARDLINE_MALFORMED_MODE")
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

/// An empty config file in `dir`, for a run that takes its settings from the environment.
fn empty_config(dir: &Path) -> PathBuf {
    let path = dir.join("empty.toml");
    fs::write(&path, "").unwrap();
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
        .map(|s| untagged(&s.request_id).to_owned())
        .collect();
    assert_eq!(ids, ["s1-t1-0", "s1-t1-1"]);
    drop(journal);

    // Settings from the environment only: the next turn sends the stored conversation.
    let out = engine(&work)
        .args(run_args("chat", "are you there"))
        .env("ULEI_CONFIG", empty_config(&dir.0))
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
    assert_eq!(untagged(&request(&bodies[2]).request_id), "s1-t2-0");
    assert_eq!(request(&bodies[2]).session_key.as_deref(), Some("chat"));
    assert_eq!(
        texts(&bodies[3]),
        [("user".to_owned(), "start over".to_owned())]
    );
    assert_eq!(untagged(&request(&bodies[3]).request_id), "s2-t1-0");
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
        .env("ULEI_CONFIG", empty_config(&dir.0))
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
        .env("ULEI_CONFIG", empty_config(&dir.0))
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
            "ulei-engine: config {path} line 2: qb.socket must be a string\n",
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
        let want = want.replace("{path}", config.to_str().unwrap());
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
        ("socket = \"again\"", "duplicate key"),
        ("[qb]", "duplicate section"),
    ];
    for (line, reason) in cases {
        let config = write_config(
            &dir.0,
            &format!("# settings\n[qb]\nsocket = \"/x\"\n{line}\n"),
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
fn the_hardline_floor_blocks_terminal_under_the_plain_config() {
    let dir = Scratch::new();
    let ran = dir.0.join("ran");
    // Nothing exists under this home, and the shell functions shadow the programs, so either
    // command would only create the marker.
    let home = dir.0.join("absent/agent");
    let commands = [
        format!("mkfs() {{ touch {}; }}; mkfs", ran.display()),
        format!(
            "rm() {{ touch {}; }}\nrm -rf {}/",
            ran.display(),
            home.display()
        ),
    ];
    for (i, command) in commands.iter().enumerate() {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call("c1", "terminal", json!({"command": command}))]),
            text_step("seen"),
        ]);
        let text = config_text(
            fake.socket(),
            &dir.0.join(format!("journal-{i}.sqlite3")),
            &dir.0,
        );
        let out = engine(&dir.0)
            .args(run_args("chat", "format"))
            .env("ULEI_CONFIG", write_config(&dir.0, &text))
            .env("HOME", &home)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

        let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
        let result = sent["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap();
        let result: Value = serde_json::from_str(result).unwrap();
        assert_eq!(result["status"], "blocked", "{command}");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .starts_with("BLOCKED (hardline): "),
            "{command}"
        );
        assert!(!ran.exists(), "{command}");
    }
}

#[test]
fn a_malformed_grep_runs_with_a_warning_line_and_block_mode_refuses_it() {
    let dir = Scratch::new();
    fs::write(dir.0.join("app.py"), "@route.get('/api/items')\n").unwrap();
    // The escaped quote in the pattern leaves the grep words unparseable once escapes resolve.
    let command = format!(
        r#"cd {} && grep -n -E "@route\.(get|post)\(['\"]/api/[a-z]+" app.py | head -10"#,
        dir.0.display()
    );
    let warning = format!(
        "ulei-engine: Hardline downgraded to warning: command parser limit or malformed \
         executable payload (command: {})\n",
        command.chars().take(200).collect::<String>()
    );
    let cases = [
        (None, None, false),
        (Some("block"), None, true),
        (None, Some("block"), true),
        (Some("block"), Some("warn"), false),
    ];
    for (i, (file, variable, blocked)) in cases.into_iter().enumerate() {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call("c1", "terminal", json!({"command": command}))]),
            text_step("seen"),
        ]);
        let mut text = config_text(
            fake.socket(),
            &dir.0.join(format!("journal-{i}.sqlite3")),
            &dir.0,
        );
        if let Some(mode) = file {
            text = format!("{text}hardline_malformed_mode = \"{mode}\"\n");
        }
        let mut run = engine(&dir.0);
        run.args(run_args("chat", "grep"))
            .env("ULEI_CONFIG", write_config(&dir.0, &text));
        if let Some(mode) = variable {
            run.env("ULEI_ENGINE_HARDLINE_MALFORMED_MODE", mode);
        }
        let out = run.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

        let sent: Value = serde_json::from_slice(&turns(&fake)[1]).unwrap();
        let result = sent["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap();
        let result: Value = serde_json::from_str(result).unwrap();
        if blocked {
            assert_eq!(result["status"], "blocked", "{file:?} {variable:?}");
            assert!(
                result["error"].as_str().unwrap().starts_with(
                    "BLOCKED (hardline): command parser limit or malformed executable payload. "
                ),
                "{result}"
            );
            assert_eq!(stderr(&out), "", "{file:?} {variable:?}");
        } else {
            assert_eq!(result["exit_code"], 0, "{result}");
            assert_eq!(result["output"], "1:@route.get('/api/items')", "{result}");
            assert_eq!(stderr(&out), warning, "{file:?} {variable:?}");
        }
    }
}

#[test]
fn the_malformed_grep_warning_masks_the_secrets_of_the_command_as_redaction_says() {
    let dir = Scratch::new();
    fs::write(dir.0.join("app.py"), "@route.get('/api/items')\n").unwrap();
    let token = "fake".repeat(6);
    let command = format!(
        r#"cd {} && grep -n -E "@route\.(get|post)\(['\"]/x" app.py; echo 'Authorization: Bearer {token}'"#,
        dir.0.display()
    );
    assert!(command.chars().count() < 200, "{command}");
    let warning = |shown: &str| {
        format!(
            "ulei-engine: Hardline downgraded to warning: command parser limit or malformed \
             executable payload (command: {})\n",
            command.replace(&token, shown)
        )
    };
    for (i, (setting, shown)) in [
        ("", "fakefa...fake"),
        ("\n[security]\nredact_secrets = false\n", token.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call("c1", "terminal", json!({"command": command}))]),
            text_step("seen"),
        ]);
        let text = config_text(
            fake.socket(),
            &dir.0.join(format!("journal-{i}.sqlite3")),
            &dir.0,
        );
        let out = engine(&dir.0)
            .args(run_args("chat", "grep"))
            .env(
                "ULEI_CONFIG",
                write_config(&dir.0, &format!("{text}{setting}")),
            )
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stderr(&out), warning(shown), "{setting:?}");
    }
}

#[test]
fn an_unknown_malformed_grep_mode_fails_the_turn_naming_the_key() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(Vec::new());
    let text = format!(
        "{}hardline_malformed_mode = \"off\"\n",
        config_text(fake.socket(), &dir.0.join("journal.sqlite3"), &dir.0)
    );
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .env("ULEI_CONFIG", write_config(&dir.0, &text))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "ulei-engine: engine.hardline_malformed_mode (ULEI_ENGINE_HARDLINE_MALFORMED_MODE) must \
         be warn or block\n"
    );
    assert_eq!(turns(&fake).len(), 0);
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
            .env("ULEI_CONFIG", empty_config(&dir.0))
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
    assert!(sent[0].starts_with("You are the test assistant.\n\n# Safeguards\n"));
    assert!(sent[0].contains(&format!(
        "Say so instead.\n\nKnowledge base index: {}. ",
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
fn a_session_resumed_from_another_directory_resends_a_refused_step_without_thinking_and_keeps_it_journaled()
 {
    let refused = GatewayError {
        message: Some("vendor answered HTTP 400: messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.".into()),
        ..gateway_error(ErrorClass::BadRequest)
    };
    let fake = FakeGateway::start(vec![
        Answer::Events(vec![
            route(),
            thinking("sig-first"),
            text("one"),
            stop(StopReason::End),
        ]),
        Answer::Reject(400, refused),
        text_step("two"),
    ]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = write_config(&dir.0, &config_text(fake.socket(), &db, &dir.0));
    let elsewhere = dir.0.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    for (cwd, prompt, answer) in [(&dir.0, "first", "one\n"), (&elsewhere, "second", "two\n")] {
        let out = engine(cwd)
            .args(run_args("chat", prompt))
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stdout(&out), answer);
    }

    let thinking_blocks = |body: &[u8]| {
        request(body)
            .messages
            .iter()
            .flat_map(|m| m.content.clone())
            .filter(|b| b.r#type == BlockType::Thinking)
            .count()
    };
    let sent = turns(&fake);
    assert_eq!(sent.len(), 3);
    // The prompt was rebuilt for the new working directory, which unbinds the old block.
    assert_ne!(request(&sent[1]).system, request(&sent[0]).system);
    assert_eq!(thinking_blocks(&sent[1]), 1);
    assert_eq!(thinking_blocks(&sent[2]), 0);
    assert_eq!(request(&sent[2]).request_id, request(&sent[1]).request_id);
    let journal = Journal::open(&db).unwrap();
    let stored = journal.messages(1).unwrap();
    assert_eq!(stored.len(), 4);
    assert_eq!(stored[1].content[0].r#type, BlockType::Thinking);
    assert_eq!(stored[1].content[0].opaque.as_deref(), Some("sig-first"));
}

#[test]
fn a_memory_write_keeps_the_prompt_of_its_session_and_a_new_session_sees_it() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "memory",
            json!({"action": "add", "target": "user", "content": "The tester likes green tea."}),
        )]),
        text_step("saved"),
        text_step("again"),
        text_step("fresh"),
    ]);
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

    run("remember my tea", false);
    run("and now", false);
    run("hello", true);

    assert_eq!(
        fs::read_to_string(dir.0.join("brain/USER.md")).unwrap(),
        "The tester prefers short answers.\n§\nThe tester likes green tea."
    );
    let result = request(&turns(&fake)[1]).messages[2].content[0]
        .content
        .clone()
        .unwrap();
    assert!(
        result.contains("\"usage\": \"4% — 63/1,375 chars\", \"entry_count\": 2, "),
        "{result}"
    );
    let sent = systems(&fake);
    assert_eq!(sent[1], sent[0]);
    assert_eq!(sent[2], sent[0]);
    assert!(!sent[0].contains("green tea"));
    assert!(sent[3].contains(
        "\nUSER PROFILE (who the user is) [4% — 63/1,375 chars]\n\
         ══════════════════════════════════════════════\n\
         The tester prefers short answers.\n§\nThe tester likes green tea.\n"
    ));
}

#[test]
fn forget_in_an_owner_turn_sends_its_refs_to_the_garden_socket_of_the_config() {
    let fake = FakeGateway::start(vec![
        text_step("hi"),
        tool_step(vec![call(
            "c1",
            "forget",
            json!({"refs": ["c8:1"], "reason": "Not needed."}),
        )]),
        text_step("done"),
    ]);
    let garden = FakeGateway::start(vec![]);
    garden.respond(
        "POST /v1/forget",
        Box::new(|_| {
            json_bytes(
                200,
                br#"{"items": [{"ref": "c8:1", "marks": 0, "queue": 3}]}"#,
            )
        }),
    );
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, None);
    let text = fs::read_to_string(&config).unwrap();
    let garden_section = format!("[garden]\nsocket = \"{}\"\n", garden.socket().display());
    fs::write(&config, format!("{text}{garden_section}")).unwrap();
    for prompt in ["hello", "forget my hello"] {
        let mut args = run_args("chat", prompt);
        args.extend(["--channel".into(), "main-chat".into()]);
        let out = engine(&dir.0)
            .args(args)
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }

    let result = request(&turns(&fake)[2]).messages[4].content[0]
        .content
        .clone()
        .unwrap();
    assert!(result.contains("\"queued\": 3"), "{result}");
    // Each owner turn asks the garden for pointers first; without a responder they fail and the
    // turns run without pointers.
    let sent: Vec<Value> = garden
        .requests()
        .iter()
        .filter(|r| r.target() != "POST /v1/pointers")
        .map(|r| {
            assert_eq!(r.target(), "POST /v1/forget");
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect();
    assert_eq!(
        sent,
        [json!({"refs": ["c8:1"], "reason": "Not needed.", "provenance": "s1-t2"})]
    );
}

#[test]
fn set_model_in_an_owner_turn_stores_a_listed_alias_and_refuses_an_unknown_or_a_foreign_one() {
    let routes: ulei_contracts::generated::c4g::RoutesAnswer = serde_json::from_value(json!({
        "routes": [
            {"alias": "chat", "state": "ready", "model": "claude-sonnet-5-5"},
            {"alias": "opus", "state": "ready"}
        ]
    }))
    .unwrap();
    let fake = FakeGateway::with_routes(
        vec![
            tool_step(vec![call("c1", "set_model", json!({}))]),
            tool_step(vec![call("c2", "set_model", json!({"model": "gpt"}))]),
            tool_step(vec![call("c3", "set_model", json!({"model": "opus"}))]),
            text_step("switched"),
            tool_step(vec![call("c4", "set_model", json!({"model": "chat"}))]),
            text_step("not yours"),
        ],
        routes,
    );
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, None);
    for channel in ["main-chat", "other"] {
        let mut args = run_args("chat", "switch to opus");
        args.extend(["--channel".into(), channel.into()]);
        let out = engine(&dir.0)
            .args(args)
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }

    let result = |turn: usize| {
        let sent = request(&turns(&fake)[turn]);
        sent.messages.last().unwrap().content[0]
            .content
            .clone()
            .unwrap()
    };
    let listed = result(1);
    assert!(listed.contains("\"current\": \"main\""), "{listed}");
    assert!(listed.contains("\"alias\": \"opus\""), "{listed}");
    let unknown = result(2);
    assert!(unknown.contains("unknown model alias"), "{unknown}");
    let set = result(3);
    assert!(set.contains("\"model\": \"opus\""), "{set}");
    assert!(set.contains("\"previous\": \"main\""), "{set}");
    let foreign = result(5);
    assert!(
        foreign.contains("runs only when the owner asks"),
        "{foreign}"
    );
    let journal = Journal::open(&db).unwrap();
    assert_eq!(
        journal
            .route("chat", std::time::Duration::from_secs(1))
            .unwrap()
            .as_deref(),
        Some("opus")
    );
}

#[test]
fn archive_search_asks_the_memory_socket_of_the_config_when_garden_forest_and_history_are_empty() {
    let fake = FakeGateway::start(vec![
        tool_step(vec![call("c1", "archive_search", json!({"query": "shed"}))]),
        text_step("done"),
    ]);
    let archive = FakeGateway::start(vec![]);
    archive.respond(
        "POST /v1/forest_search",
        Box::new(|_| json_bytes(200, br#"{"hits": []}"#)),
    );
    archive.respond(
        "POST /v1/search",
        Box::new(|_| {
            json_bytes(
                200,
                br#"{"hits": [{"id": "ab12", "entries": [{"source": "notes", "external_id": "n-1", "kind": "text/plain", "put_at": 1.0}], "snippet": "the >>>shed<<< roof"}], "synced": true}"#,
            )
        }),
    );
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, None);
    let text = fs::read_to_string(&config).unwrap();
    let memory_section = format!("[memory]\nsocket = \"{}\"\n", archive.socket().display());
    fs::write(&config, format!("{text}{memory_section}")).unwrap();
    let out = engine(&dir.0)
        .args(run_args("chat", "what about the shed?"))
        .env("ULEI_CONFIG", &config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let result = request(&turns(&fake)[1]).messages[2].content[0]
        .content
        .clone()
        .unwrap();
    assert!(result.contains("\"id\": \"ab12\""), "{result}");
    assert!(
        result.contains("\"garden_error\": \"the garden is not configured"),
        "{result}"
    );
    let sent: Vec<(String, Value)> = archive
        .requests()
        .iter()
        .map(|r| (r.target(), serde_json::from_slice(&r.body).unwrap()))
        .collect();
    assert_eq!(
        sent,
        [
            (
                "POST /v1/forest_search".to_owned(),
                json!({"q": "shed", "limit": 10})
            ),
            (
                "POST /v1/search".to_owned(),
                json!({"q": "shed", "limit": 10})
            )
        ]
    );
}

/// The text of the last message of the `n`-th turn request `fake` got.
fn sent_input(fake: &FakeGateway, n: usize) -> String {
    let sent = request(&turns(fake)[n]);
    sent.messages.last().unwrap().content[0]
        .text
        .clone()
        .unwrap()
}

/// The config of [`reminder_config`] with `garden` appended.
fn garden_config(fake: &FakeGateway, db: &Path, dir: &Path, garden: &str) -> PathBuf {
    let config = reminder_config(fake, db, dir, None);
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{text}[garden]\n{garden}")).unwrap();
    config
}

#[test]
fn the_garden_core_profile_ends_every_prompt_as_it_stands_and_an_owner_turn_carries_pointers() {
    let fake = FakeGateway::start(vec![text_step("one"), text_step("two"), text_step("three")]);
    let garden = pointing_garden();
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let core = dir.0.join("core.md");
    let config = garden_config(
        &fake,
        &db,
        &dir.0,
        &format!(
            "socket = \"{}\"\ncore_file = \"{}\"\ncore_char_limit = 100\n",
            garden.socket().display(),
            core.display()
        ),
    );
    let run = |prompt: &str, event: bool| {
        let mut args = run_args("chat", prompt);
        if event {
            args.push("--event".into());
        }
        let out = engine(&dir.0)
            .args(args)
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    };

    fs::write(&core, "Anna: birthday on 3 May\n").unwrap();
    run("When is Anna's birthday?", false);
    fs::write(&core, "Anna: birthday on 3 May\n§\nHome: Elm Street 4\n").unwrap();
    run("A background event.", true);
    fs::remove_file(&core).unwrap();
    run("Thanks.", false);

    let frame = "═".repeat(46);
    let header = "GARDEN CORE PROFILE (pinned facts about the owner, read-only)";
    let sent = systems(&fake);
    assert!(
        sent[0].ends_with(&format!(
            "\n\n{frame}\n{header} [23% — 23/100 chars]\n{frame}\nAnna: birthday on 3 May"
        )),
        "{}",
        sent[0]
    );
    assert!(
        sent[1].ends_with(&format!(
            "\n\n{frame}\n{header} [44% — 44/100 chars]\n{frame}\nAnna: birthday on 3 May\n§\nHome: Elm Street 4"
        )),
        "{}",
        sent[1]
    );
    assert!(!sent[2].contains(header), "{}", sent[2]);
    assert_eq!(
        sent[1].split(&format!("\n\n{frame}\n{header}")).next(),
        Some(sent[2].as_str())
    );
    let stored = Journal::open(&db).unwrap().session("chat").unwrap();
    assert_eq!(stored.system_prompt.as_deref(), Some(sent[2].as_str()));

    let first = sent_input(&fake, 0);
    assert!(
        first.starts_with("When is Anna's birthday?\n\n<memory-context>\n"),
        "{first}"
    );
    assert!(
        first.ends_with("\n- there is a fact about Anna (person)\n</memory-context>"),
        "{first}"
    );
    assert!(!sent_input(&fake, 1).contains("<memory-context>"));
    assert!(sent_input(&fake, 2).contains("<memory-context>"));
    let asked: Vec<Value> = garden
        .requests()
        .iter()
        .map(|r| {
            assert_eq!(r.target(), "POST /v1/pointers");
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect();
    assert_eq!(
        asked,
        [
            json!({"q": "When is Anna's birthday?", "private": true}),
            json!({"q": "Thanks.", "private": true})
        ]
    );
    let rows = journal_rows(&db);
    assert!(
        !rows.iter().any(|row| row.contains("memory-context")),
        "{rows:?}"
    );

    let tools = tool_names(&fake);
    assert_eq!(tools[5..13], LATER_TOOLS);
    assert_eq!(tools[13..], ["garden_search", "garden_propose"]);
}

/// A garden answering every `pointers` with one pointer to a fact about Anna.
fn pointing_garden() -> FakeGateway {
    let garden = FakeGateway::start(vec![]);
    garden.respond(
        "POST /v1/pointers",
        Box::new(|_| {
            json_bytes(
                200,
                br#"{"pointers": [{"fact_id": "f:1", "entity": "Anna", "kind": "person", "score": 1.0, "hint": "there is a fact about Anna (person)"}]}"#,
            )
        }),
    );
    garden
}

/// The tool names the first turn request `fake` got advertised, in order.
fn tool_names(fake: &FakeGateway) -> Vec<String> {
    request(&turns(fake)[0])
        .tools
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[test]
fn a_garden_that_is_down_or_slow_leaves_an_owner_turn_without_pointers_and_one_stderr_line() {
    // The slow garden answers after 10 s; each turn must have ended before that.
    let answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let slow = FakeGateway::start(vec![]);
    let flag = std::sync::Arc::clone(&answered);
    slow.respond(
        "POST /v1/pointers",
        Box::new(move |_| {
            std::thread::sleep(std::time::Duration::from_secs(10));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            json_bytes(200, br#"{"pointers": []}"#)
        }),
    );
    let dir = Scratch::new();
    for (socket, said) in [
        (dir.0.join("absent.sock"), "garden socket: "),
        (slow.socket().to_path_buf(), "garden timed out"),
    ] {
        let fake = FakeGateway::start(vec![text_step("fine")]);
        let db = dir.0.join("journal.sqlite3");
        let config = garden_config(
            &fake,
            &db,
            &dir.0,
            &format!("socket = \"{}\"\n", socket.display()),
        );
        let out = engine(&dir.0)
            .args(run_args("chat", "When is Anna's birthday?"))
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        let late = answered.load(std::sync::atomic::Ordering::SeqCst);

        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "fine\n");
        let lines = stderr(&out);
        assert_eq!(lines.lines().count(), 1, "{lines}");
        assert!(
            lines.starts_with(&format!("ulei-engine: garden pointers: {said}")),
            "{lines}"
        );
        assert!(lines.ends_with("; the turn runs without them\n"), "{lines}");
        assert_eq!(sent_input(&fake, 0), "When is Anna's birthday?");
        assert!(!late, "{said}: the turn waited for the garden's answer");
    }
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
    assert_eq!(untagged(&request(&bodies[0]).request_id), "s1-t2-0");
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
            "{}disabled_skills = \" soup , \"\nskills_index = true\n",
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
    assert_eq!(tools[5..], LATER_TOOLS);
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

/// A tool step that calls `read_file` on `path` and reports a prompt of `input` tokens.
fn read_step(id: &str, path: &Path, input: u64) -> Answer {
    Answer::Events(vec![
        route(),
        call(id, "read_file", json!({"path": path})),
        usage_of(input, 5),
        stop(StopReason::ToolUse),
    ])
}

/// The `cache` field of a recorded turn request; `None` when it is absent.
fn cache_field(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).unwrap();
    value
        .get("cache")
        .map(|cache| cache.as_str().unwrap().to_owned())
}

#[test]
fn the_prompt_cache_mode_reaches_the_turn_compaction_and_classifier_requests() {
    let dir = Scratch::new();
    let files: Vec<PathBuf> = (0..4)
        .map(|n| dir.0.join(format!("part-{n}.txt")))
        .collect();
    for (n, file) in files.iter().enumerate() {
        fs::write(file, format!("part {n} ").repeat(2300)).unwrap();
    }
    let cases = [
        (None, None, Some("auto_1h")),
        (Some("auto"), None, Some("auto")),
        (Some("auto"), Some("off"), None),
    ];
    for (i, (file, variable, want)) in cases.into_iter().enumerate() {
        let mut script: Vec<Answer> = files
            .iter()
            .enumerate()
            .map(|(n, path)| read_step(&format!("c{n}"), path, if n == 3 { 150_000 } else { 50 }))
            .collect();
        script.extend([
            text_step("## Goal\nRead the parts."),
            text_step("Shall I start?"),
            text_step("{\"verdict\": \"extra\"}"),
            text_step("Started."),
        ]);
        let fake = FakeGateway::start(script);
        let db = dir.0.join(format!("journal-{i}.sqlite3"));
        let sections = format!(
            "{}[engine.compression]\nthreshold_tokens = 10000\n[engine.routes.main]\ncontext_length = 200000\n",
            guard_lines(&dir.0)
        );
        let config = guard_config(&fake, &db, &dir.0, &sections);
        if let Some(mode) = file {
            let text = fs::read_to_string(&config).unwrap().replace(
                "main_channel = \"main-chat\"\n",
                &format!("main_channel = \"main-chat\"\nprompt_cache = \"{mode}\"\n"),
            );
            fs::write(&config, text).unwrap();
        }
        let mut command = engine(&dir.0);
        command
            .args(run_args("chat", "Read the parts."))
            .args(["--channel", "main-chat"])
            .env("ULEI_CONFIG", &config);
        if let Some(mode) = variable {
            command.env("ULEI_ENGINE_PROMPT_CACHE", mode);
        }
        let out = command.output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{file:?} {variable:?}: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "Started.\n", "{file:?} {variable:?}");

        let sent = turns(&fake);
        let routes: Vec<String> = sent.iter().map(|b| request(b).route).collect();
        assert_eq!(
            routes,
            [
                "main", "main", "main", "main", "compact", "main", "judge", "main"
            ],
            "{file:?} {variable:?}"
        );
        for body in &sent {
            assert_eq!(cache_field(body).as_deref(), want, "{file:?} {variable:?}");
        }
    }
}

#[test]
fn an_unknown_prompt_cache_mode_refuses_run_and_serve_naming_the_key() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(Vec::new());
    let db = dir.0.join("journal.sqlite3");
    let unknown = write_config(
        &dir.0,
        &format!(
            "{}prompt_cache = \"1h\"\n",
            config_text(fake.socket(), &db, &dir.0)
        ),
    );
    let plain = write_config(
        &dir.0.join("brain"),
        &config_text(fake.socket(), &db, &dir.0),
    );
    let want = "ulei-engine: engine.prompt_cache (ULEI_ENGINE_PROMPT_CACHE) must be auto_1h, \
                auto or off\n";
    for args in [run_args("chat", "hi"), vec!["serve".to_owned()]] {
        for (config, variable) in [(&unknown, None), (&plain, Some("AUTO"))] {
            let mut command = engine(&dir.0);
            command
                .args(&args)
                .env("ULEI_CONFIG", config)
                .env("ULEI_ENGINE_C6_SOCKET", dir.0.join("c6.sock"))
                .env("ULEI_ENGINE_OWNER_SESSION", "owner")
                .env("ULEI_ENGINE_DEFAULT_ROUTE", "main");
            if let Some(mode) = variable {
                command.env("ULEI_ENGINE_PROMPT_CACHE", mode);
            }
            let out = command.output().unwrap();
            // `serve` exits 2 on a refused config, so its unit does not restart it.
            let code = if args[0] == "serve" { 2 } else { 1 };
            assert_eq!(out.status.code(), Some(code), "{args:?} {variable:?}");
            assert_eq!(stdout(&out), "", "{args:?} {variable:?}");
            assert_eq!(stderr(&out), want, "{args:?} {variable:?}");
        }
    }
    assert_eq!(turns(&fake).len(), 0);
    assert!(!db.exists());
    assert!(!dir.0.join("c6.sock").exists());
}

#[test]
fn by_default_the_prompt_carries_the_core_and_neither_the_skills_index_nor_a_reminder() {
    let fake = FakeGateway::start(vec![text_step("ok")]);
    let dir = Scratch::new();
    let skill = dir.0.join("brain/skills/kitchen/bread");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: bread\ndescription: Bake the bread.\n---\n",
    )
    .unwrap();
    let board = dir.0.join("brain/task-board.md");
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, None);
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .args(["--channel", "main-chat"])
        .env("ULEI_CONFIG", &config)
        .env("ULEI_ENGINE_TASK_BOARD_FILE", &board)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let system = &systems(&fake)[0];
    let core = system
        .strip_prefix("You are the test assistant.\n\n# Safeguards\n")
        .unwrap();
    let (core, _) = core.split_once("\n\nKnowledge base index: ").unwrap();
    assert_eq!(core.lines().filter(|l| l.starts_with("Never ")).count(), 3);
    assert!(core.ends_with(&format!(
        "Task delegation: to file a task or hand one out, read {} with read_file. It gives the \
         address of the task board and the commands to use.",
        board.display()
    )));
    assert!(!system.contains("<available_skills>"));
    assert_eq!(
        texts(&turns(&fake)[0])[0],
        ("user".to_owned(), "hi".to_owned())
    );
}

#[test]
fn an_unknown_core_id_refuses_run_and_serve_naming_the_key() {
    let dir = Scratch::new();
    let fake = FakeGateway::start(Vec::new());
    let db = dir.0.join("journal.sqlite3");
    let text = format!(
        "{}\n[engine.core.no_money]\nenabled = false\n",
        config_text(fake.socket(), &db, &dir.0)
    );
    let line = text.lines().count() - 1;
    let config = write_config(&dir.0, &text);
    let want = format!(
        "ulei-engine: config {} line {line}: engine.core.no_money names an unknown id; known: \
         no_unasked_messages, no_unfiled_promises, no_irreversible_deletion, search, \
         terminal_and_files, task_delegation\n",
        config.display()
    );
    for args in [run_args("chat", "hi"), vec!["serve".to_owned()]] {
        let out = engine(&dir.0)
            .args(&args)
            .env("ULEI_CONFIG", &config)
            .env("ULEI_ENGINE_C6_SOCKET", dir.0.join("c6.sock"))
            .env("ULEI_ENGINE_OWNER_SESSION", "owner")
            .env("ULEI_ENGINE_DEFAULT_ROUTE", "main")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(refused_config_code(&args)),
            "{args:?}"
        );
        assert_eq!(stdout(&out), "", "{args:?}");
        assert_eq!(stderr(&out), want, "{args:?}");
    }
    assert_eq!(turns(&fake).len(), 0);
    assert!(!db.exists());
    assert!(!dir.0.join("c6.sock").exists());
}

/// The bytes of the top-level field `name` of a recorded turn request, up to the field `next`.
fn field<'a>(body: &'a [u8], name: &str, next: &str) -> &'a [u8] {
    let find = |pattern: &str, from: usize| {
        body[from..]
            .windows(pattern.len())
            .position(|w| w == pattern.as_bytes())
            .map_or_else(|| panic!("no {pattern}"), |at| at + from)
    };
    let start = find(&format!("\"{name}\":"), 0);
    &body[start..find(&format!(",\"{next}\":"), start)]
}

#[test]
fn the_next_turn_repeats_the_system_text_tools_and_stored_history_of_the_last_byte_for_byte() {
    let mcp = FakeMcp::start(
        Spec::new(vec![
            json!({"name": "lookup", "description": "Look a topic up.", "inputSchema": {"type": "object", "properties": {"topic": {"type": "string"}}}}),
            json!({"name": "append", "description": "Append a note."}),
        ])
        .reply(
            "lookup",
            Reply::Json(json!({"content": [{"type": "text", "text": "shed: paint"}]})),
        ),
    );
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "mcp__notes__lookup",
            json!({"topic": "shed"}),
        )]),
        text_step("noted"),
        text_step("again"),
    ]);
    let dir = Scratch::new();
    let skill = dir.0.join("brain/skills/kitchen/bread");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: bread\ndescription: Bake the bread.\n---\n",
    )
    .unwrap();
    let db = dir.0.join("journal.sqlite3");
    let config = reminder_config(&fake, &db, &dir.0, Some("Synthetic reminder."));
    let text = format!(
        "{}skills_index = true\nmcp_servers = \"notes\"\n[engine.mcp.notes]\nurl = \"{}\"\n",
        fs::read_to_string(&config).unwrap(),
        mcp.url()
    );
    fs::write(&config, text).unwrap();
    let turn = |prompt: &str| {
        let out = engine(&dir.0)
            .args(run_args("chat", prompt))
            .args(["--channel", "main-chat"])
            .env("ULEI_CONFIG", &config)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    };

    turn("first");
    let stored = Journal::open(&db).unwrap().messages(1).unwrap();
    turn("second");

    let bodies = turns(&fake);
    assert_eq!(bodies.len(), 3);
    let (first, last, next) = (&bodies[0], &bodies[1], &bodies[2]);
    for body in [last, next] {
        assert_eq!(
            field(body, "system", "messages"),
            field(first, "system", "messages")
        );
        assert_eq!(
            field(body, "tools", "max_tokens"),
            field(first, "tools", "max_tokens")
        );
    }
    let tools: Vec<String> = request(next)
        .tools
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(tools[5..13], LATER_TOOLS);
    assert_eq!(
        tools[13..],
        [
            "mcp__notes__lookup",
            "mcp__notes__append",
            "mcp__notes__list_resources",
            "mcp__notes__read_resource",
            "mcp__notes__list_prompts",
            "mcp__notes__get_prompt"
        ]
    );
    let system = request(next).system.unwrap();
    assert!(system.contains("- bread: Bake the bread."));

    let sent = request(last).messages;
    let resent = request(next).messages;
    assert_eq!(stored.len(), sent.len() + 1);
    assert_eq!(resent[..stored.len()], stored[..]);
    assert_eq!(resent[1..sent.len()], sent[1..]);
    let user = |text: &str| ("user".to_owned(), text.to_owned());
    assert_eq!(texts(last)[0], user("first\n\nSynthetic reminder."));
    assert_eq!(texts(next)[0], user("first"));
    assert_eq!(
        texts(next)[stored.len()..],
        [user("second\n\nSynthetic reminder.")]
    );
    let reminders = String::from_utf8_lossy(next)
        .matches("Synthetic reminder.")
        .count();
    assert_eq!(reminders, 1);
}

/// `run` of `prompt` in a fresh session of `dir` against `fake`, with its output piped.
fn spawn_run(fake: &FakeGateway, dir: &Path, prompt: &str) -> Child {
    let config = write_config(
        dir,
        &config_text(fake.socket(), &dir.join("journal.sqlite3"), dir),
    );
    engine(dir)
        .args(run_args("chat", prompt))
        .args(["--config".as_ref(), config.as_os_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// `signal` to `child`, then its output once it exits.
fn interrupt(child: Child, signal: Signal) -> Output {
    kill_process(Pid::from_child(&child), signal).unwrap();
    child.wait_with_output().unwrap()
}

/// `role: block type, ...` of each journaled message of session 1 in `dir`.
fn journaled(dir: &Path) -> Vec<String> {
    Journal::open(&dir.join("journal.sqlite3"))
        .unwrap()
        .records(1)
        .unwrap()
        .into_iter()
        .map(|r| {
            let blocks = r.message["content"].as_array().unwrap().iter();
            let types: Vec<&str> = blocks.map(|b| b["type"].as_str().unwrap()).collect();
            format!(
                "{}: {}",
                r.message["role"].as_str().unwrap(),
                types.join(", ")
            )
        })
        .collect()
}

#[test]
fn sigint_sigterm_or_sighup_during_a_gateway_step_exits_130_and_journals_the_turn() {
    for signal in [Signal::INT, Signal::TERM, Signal::HUP] {
        let fake = FakeGateway::start(vec![Answer::Held(vec![
            route(),
            text("never"),
            stop(StopReason::End),
        ])]);
        let dir = Scratch::new();
        let child = spawn_run(&fake, &dir.0, "first");
        assert!(fake.wait_hanging("s1-t1-"));

        let out = interrupt(child, signal);
        assert_eq!(out.status.code(), Some(130), "{signal:?}: {}", stderr(&out));
        assert_eq!(stdout(&out), "");
        assert_eq!(stderr(&out), "ulei-engine: the turn was interrupted\n");
        let cancelled = cancels(&fake);
        assert_eq!(
            cancelled.iter().map(|id| untagged(id)).collect::<Vec<_>>(),
            ["s1-t1-0"]
        );
        assert_eq!(journaled(&dir.0), ["user: text"]);
    }
}

#[test]
fn sigint_during_a_shell_tool_kills_its_group_and_journals_the_turn() {
    let command = "(sleep 30 & echo $! > child.pid; wait) & echo $$ > shell.pid; wait";
    let fake = FakeGateway::start(vec![
        tool_step(vec![call("c1", "terminal", json!({"command": command}))]),
        text_step("unreachable"),
    ]);
    let dir = Scratch::new();
    let child = spawn_run(&fake, &dir.0, "run it");
    let (sleeper, shell) = (
        pid_file(&dir.0.join("child.pid")),
        pid_file(&dir.0.join("shell.pid")),
    );

    let out = interrupt(child, Signal::INT);
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    assert_eq!(stderr(&out), "ulei-engine: the turn was interrupted\n");
    assert!(dead(&sleeper), "the backgrounded child {sleeper}");
    assert!(dead(&shell), "the shell {shell}");
    assert_eq!(turns(&fake).len(), 1);
    assert_eq!(
        journaled(&dir.0),
        ["user: text", "assistant: tool_use", "user: tool_result"]
    );
    let result = Journal::open(&dir.0.join("journal.sqlite3"))
        .unwrap()
        .messages(1)
        .unwrap()[2]
        .content[0]
        .content
        .clone()
        .unwrap();
    assert!(result.contains("[Command interrupted]"), "{result}");
}

#[test]
fn every_setting_comes_from_its_variable_over_an_empty_file_and_over_the_values_of_a_file() {
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let empty = empty_config(&dir.0);
    let other = write_config(
        &dir.0,
        &format!(
            "{}c6_socket = \"{}\"\nowner_session = \"other\"\ndefault_route = \"other\"\n",
            config_text(
                &dir.0.join("no-such.sock"),
                &dir.0.join("other.sqlite3"),
                &dir.0
            ),
            dir.0.join("no-such-dir/c6.sock").display()
        ),
    );
    for (i, config) in [empty, other].iter().enumerate() {
        let c6 = dir.0.join(format!("c6-{i}.sock"));
        let fake = FakeGateway::start(vec![
            tool_step(vec![call(
                "c1",
                "read_file",
                json!({"path": "missing.txt"}),
            )]),
            text_step("Summary."),
        ]);
        let with_env = |command: &mut Command| {
            command
                .args(["--config".as_ref(), config.as_os_str()])
                .env("ULEI_QB_SOCKET", fake.socket())
                .env("ULEI_ENGINE_JOURNAL", &db)
                .env("ULEI_ENGINE_TERMINAL_OUTPUT", spill_dir(&db))
                .envs(brain_env(&dir.0))
                .env("ULEI_ENGINE_MAX_STEPS", "1")
                .env("ULEI_ENGINE_C6_SOCKET", &c6)
                .env("ULEI_ENGINE_OWNER_SESSION", "owner")
                .env("ULEI_ENGINE_DEFAULT_ROUTE", "main");
        };

        let mut run = engine(&dir.0);
        run.args(run_args("chat", "look"));
        with_env(&mut run);
        let out = run.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stdout(&out), "Summary.\n");
        let sent = turns(&fake);
        assert_eq!(sent.len(), 2, "{config:?}");
        assert_eq!(request(&sent[1]).tools, None, "{config:?}");
        assert!(db.exists(), "{config:?}");

        let mut serve = engine(&dir.0);
        serve.arg("serve");
        with_env(&mut serve);
        let mut child = serve
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        child.kill().unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            line,
            format!("ulei-engine ready {}\n", c6.display()),
            "{config:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn without_a_config_file_run_and_serve_refuse_to_start_and_the_file_in_the_xdg_directory_is_read() {
    let fake = FakeGateway::start(vec![text_step("found")]);
    let dir = Scratch::new();
    let db = dir.0.join("journal.sqlite3");
    let xdg = dir.0.join("ulei/ulei.toml");
    for args in [run_args("chat", "hi"), vec!["serve".to_owned()]] {
        let settings = |command: &mut Command| {
            command
                .args(&args)
                .env("ULEI_QB_SOCKET", fake.socket())
                .env("ULEI_ENGINE_JOURNAL", &db)
                .env("ULEI_ENGINE_TERMINAL_OUTPUT", spill_dir(&db))
                .envs(brain_env(&dir.0))
                .env("ULEI_ENGINE_C6_SOCKET", dir.0.join("c6.sock"))
                .env("ULEI_ENGINE_OWNER_SESSION", "owner")
                .env("ULEI_ENGINE_DEFAULT_ROUTE", "main");
        };
        let mut missing = engine(&dir.0);
        settings(&mut missing);
        let out = missing.output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(refused_config_code(&args)),
            "{args:?}"
        );
        assert_eq!(
            stderr(&out),
            format!(
                "ulei-engine: config {}: No such file or directory (os error 2)\n",
                xdg.display()
            ),
            "{args:?}"
        );
        let mut no_path = engine(&dir.0);
        settings(&mut no_path);
        let out = no_path
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HOME")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(refused_config_code(&args)),
            "{args:?}"
        );
        assert_eq!(
            stderr(&out),
            "ulei-engine: config: no --config and none of ULEI_CONFIG, XDG_CONFIG_HOME or HOME \
             is set\n",
            "{args:?}"
        );
    }
    assert!(!db.exists());

    fs::create_dir(dir.0.join("ulei")).unwrap();
    fs::write(&xdg, config_text(fake.socket(), &db, &dir.0)).unwrap();
    let out = engine(&dir.0)
        .args(run_args("chat", "hi"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "found\n");
}

/// The exit status of a refused config: 2 for `serve`, so its unit does not restart it, else 1.
fn refused_config_code(args: &[String]) -> i32 {
    if args[0] == "serve" { 2 } else { 1 }
}
