//! The policy chain around tool calls and turn ends, and the `state-db-guard` policy.

#![allow(missing_docs, clippy::unwrap_used)]

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};
use ulei_engine::policy::{Chain, Policy, StateDbGuard, StopDecision, Verdict};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Context, Interrupt, Output, Registry};
use ulei_engine::wire::c4g::{Message, Role, ToolCall};

const DB: &str = "/srv/agent/state/session.db";

fn tool_call(name: &str, input: Value) -> ToolCall {
    let Value::Object(input) = input else {
        panic!("not an object")
    };
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        input,
    }
}

/// Records each hook it sees and answers with fixed decisions.
struct Scripted {
    name: &'static str,
    log: Rc<RefCell<Vec<String>>>,
    deny: bool,
    resume: Option<&'static str>,
    suffix: &'static str,
}

impl Scripted {
    fn new(name: &'static str, log: &Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            name,
            log: Rc::clone(log),
            deny: false,
            resume: None,
            suffix: "",
        }
    }
}

impl Policy for Scripted {
    fn name(&self) -> &'static str {
        self.name
    }

    fn pre_tool(&mut self, _call: &ToolCall, _cwd: &Path) -> Verdict {
        self.log.borrow_mut().push(format!("{}:pre", self.name));
        if self.deny {
            Verdict::Deny("not now".into())
        } else {
            Verdict::Allow
        }
    }

    fn post_tool(&mut self, _call: &ToolCall, output: &mut Output) {
        self.log.borrow_mut().push(format!("{}:post", self.name));
        output.content.push_str(self.suffix);
    }

    fn on_stop(&mut self, _turn: &Message) -> StopDecision {
        self.log.borrow_mut().push(format!("{}:stop", self.name));
        self.resume
            .map_or(StopDecision::Stop, |m| StopDecision::Continue(m.into()))
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ulei-policy-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn first_denial_wins_and_the_tool_does_not_run() {
    let dir = scratch("deny");
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut first = Scripted::new("first", &log);
    first.deny = true;
    let mut chain = Chain::new(vec![
        Box::new(first),
        Box::new(Scripted::new("second", &log)),
    ]);
    let interrupt = Interrupt::new();
    let out = chain.run_tool(
        &Registry::builtin(&dir, Redactor { enabled: true }, None),
        &tool_call("terminal", json!({"command": "touch ran"})),
        Context {
            cwd: &dir,
            interrupt: &interrupt,
        },
    );
    assert_eq!(out, Output::error("denied by policy first: not now"));
    assert_eq!(*log.borrow(), ["first:pre"]);
    assert!(!dir.join("ran").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_denied_call_of_another_tool_ends_the_run_of_repeated_searches() {
    let dir = scratch("deny-repeats");
    fs::write(dir.join("f.txt"), "needle\n").unwrap();
    let tools = Registry::builtin(&dir.join("spill"), Redactor { enabled: true }, None);
    let mut chain = Chain::new(vec![Box::new(guard())]);
    let interrupt = Interrupt::new();
    let ctx = Context {
        cwd: &dir,
        interrupt: &interrupt,
    };
    let search = tool_call("search_files", json!({"pattern": "needle"}));
    let warned = |chain: &mut Chain| {
        let out = chain.run_tool(&tools, &search, ctx);
        assert!(!out.is_error, "{out:?}");
        out.content.contains("_warning")
    };
    assert!(!warned(&mut chain));
    assert!(!warned(&mut chain));
    let denied = chain.run_tool(
        &tools,
        &tool_call("terminal", json!({"command": format!("sqlite3 {DB}")})),
        ctx,
    );
    assert!(
        denied.is_error && denied.content.contains("denied by policy state-db-guard"),
        "{denied:?}"
    );
    assert!(!warned(&mut chain));
    assert!(!warned(&mut chain));
    assert!(warned(&mut chain));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_policy_amends_the_output_of_a_call_that_ran() {
    let dir = scratch("amend");
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut first = Scripted::new("first", &log);
    first.suffix = " +first";
    let mut second = Scripted::new("second", &log);
    second.suffix = " +second";
    let mut chain = Chain::new(vec![Box::new(first), Box::new(second)]);
    let interrupt = Interrupt::new();
    let out = chain.run_tool(
        &Registry::builtin(&dir, Redactor { enabled: true }, None),
        &tool_call("terminal", json!({"command": "printf hi"})),
        Context {
            cwd: &dir,
            interrupt: &interrupt,
        },
    );
    assert_eq!(
        out,
        Output::ok(r#"{"output": "hi", "exit_code": 0, "error": null} +first +second"#)
    );
    assert_eq!(
        *log.borrow(),
        ["first:pre", "second:pre", "first:post", "second:post"]
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn first_request_to_continue_wins_at_stop() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let turn = Message {
        role: Role::Assistant,
        content: Vec::new(),
    };
    let mut quiet = Chain::new(vec![Box::new(Scripted::new("a", &log))]);
    assert_eq!(quiet.on_stop(&turn), StopDecision::Stop);

    let mut b = Scripted::new("b", &log);
    b.resume = Some("keep going");
    let mut c = Scripted::new("c", &log);
    c.resume = Some("other");
    let mut chain = Chain::new(vec![
        Box::new(Scripted::new("a", &log)),
        Box::new(b),
        Box::new(c),
    ]);
    log.borrow_mut().clear();
    assert_eq!(
        chain.on_stop(&turn),
        StopDecision::Continue("keep going".into())
    );
    assert_eq!(*log.borrow(), ["a:stop", "b:stop"]);
}

fn guard() -> StateDbGuard {
    StateDbGuard::new(PathBuf::from(DB)).unwrap()
}

fn terminal(command: &str) -> Verdict {
    guard().pre_tool(
        &tool_call("terminal", json!({"command": command})),
        Path::new("/work"),
    )
}

fn assert_denied(verdict: &Verdict, context: &str) {
    match verdict {
        Verdict::Deny(reason) => assert!(reason.contains(DB), "{context}: {reason}"),
        Verdict::Allow => panic!("{context}: allowed"),
    }
}

#[test]
fn guard_rejects_a_relative_path() {
    assert!(StateDbGuard::new(PathBuf::from("session.db")).is_err());
}

#[test]
fn guard_denies_opening_the_database_through_sqlite() {
    let commands = [
        format!("sqlite3 {DB} .tables"),
        "cd /srv/agent/state && sqlite3 session.db 'select 1'".to_owned(),
        format!("sqlite3 -readonly \"{DB}\""),
        format!("python3 -c \"import sqlite3; sqlite3.connect('{DB}')\""),
        "perl -MDBI -e 'DBI->connect(\"dbi:SQLite:dbname=session.db\")'".to_owned(),
        format!("cp {DB} /tmp/copy.db; sqlite3 {DB}"),
        format!("ls {DB} && sqlite3 {DB}-wal"),
        format!("echo .dump | sqlite3 {DB}"),
        "DB=session.db; sqlite3 \"$DB\"".to_owned(),
        format!("ls \"$(sqlite3 {DB} .dump)\""),
        "sqlite3 /srv/agent/state/*.db".to_owned(),
        format!("cp /tmp/backup.db {DB}"),
        format!("ls > {DB}"),
        format!("sqlite3 '{DB}"),
        format!("ls {DB} | xargs sqlite3"),
        "timeout 5 sqlite3 /srv/agent/state/*.db .tables".to_owned(),
        "nice sqlite3 $(ls /srv/agent/state/*.db)".to_owned(),
        "find /srv -name '*.db' -exec sqlite3 {} .tables ';'".to_owned(),
        format!("cp {DB} /tmp/copy.db && sqlite3 /tmp/copy.db .tables"),
        format!("cp -l {DB} /tmp/x.db; sqlite3 /tmp/x.db .tables"),
        format!("cp -l {DB} /tmp/x.db"),
        format!("cp -s {DB} /tmp/x.db"),
        format!("cp --link {DB} /tmp/x.db"),
        format!("cp --symbolic-link {DB} /tmp/x.db"),
        format!("cp -al {DB} /tmp/x.db"),
        format!("cp -pv {DB} /tmp/x.db"),
        format!("cp -t /tmp {DB}"),
    ];
    for command in &commands {
        assert_denied(&terminal(command), command);
    }
}

#[test]
fn guard_allows_copy_inspect_and_unrelated_sqlite() {
    let commands = [
        format!("cp {DB} /tmp/copy.db"),
        format!("ls -l {DB}"),
        format!("stat {DB}"),
        format!("du -h {DB}"),
        format!("sha256sum {DB} {DB}-wal"),
        "sqlite3 /tmp/other.db .tables".to_owned(),
        "sqlite3 session.db.bak .tables".to_owned(),
        format!("cp -p -v --preserve=timestamps {DB} /tmp/copy.db"),
        format!("ls -l {DB} 2>/dev/null"),
        "timeout 5 sqlite3 /tmp/other.db .tables".to_owned(),
        "python3 -c 'import sqlite3; sqlite3.connect(\"/tmp/other.db\")'".to_owned(),
        "git status".to_owned(),
    ];
    for command in &commands {
        assert_eq!(terminal(command), Verdict::Allow, "{command}");
    }
}

#[test]
fn guard_allows_copy_then_sqlite_on_the_copy_in_two_calls() {
    let mut guard = guard();
    let cwd = Path::new("/work");
    for command in [
        format!("cp {DB} /tmp/copy.db"),
        "sqlite3 /tmp/copy.db .tables".to_owned(),
    ] {
        let call = tool_call("terminal", json!({"command": command}));
        assert_eq!(guard.pre_tool(&call, cwd), Verdict::Allow, "{command}");
    }
}

#[test]
fn guard_denies_writing_the_database() {
    let cwd = Path::new("/srv/agent/state");
    let cases = [
        ("write_file", json!({"path": DB, "content": ""})),
        (
            "write_file",
            json!({"path": format!("{DB}-journal"), "content": ""}),
        ),
        (
            "patch",
            json!({"path": "session.db", "old_string": "a", "new_string": "b"}),
        ),
        (
            "patch",
            json!({"path": "../state/./session.db", "old_string": "a", "new_string": "b"}),
        ),
    ];
    for (tool, input) in cases {
        let verdict = guard().pre_tool(&tool_call(tool, input.clone()), cwd);
        assert!(matches!(verdict, Verdict::Deny(_)), "{tool} {input}");
    }
    let other = tool_call("write_file", json!({"path": "notes.db", "content": ""}));
    assert_eq!(guard().pre_tool(&other, cwd), Verdict::Allow);
    let read = tool_call("read_file", json!({"path": DB}));
    assert_eq!(guard().pre_tool(&read, cwd), Verdict::Allow);
}

#[test]
fn guard_denies_writing_the_database_through_a_symlinked_directory() {
    let dir = scratch("link");
    fs::create_dir_all(dir.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("alias")).unwrap();
    let mut guard = StateDbGuard::new(dir.join("real/session.db")).unwrap();
    let call = tool_call(
        "write_file",
        json!({"path": "alias/session.db", "content": ""}),
    );
    assert!(matches!(guard.pre_tool(&call, &dir), Verdict::Deny(_)));
    fs::remove_dir_all(&dir).unwrap();
}
