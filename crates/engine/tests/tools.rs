//! The built-in tools through the registry: input validation, shell limits and interrupts,
//! and the file tools.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use ulei_engine::tools::{
    Context, Interrupt, MAX_OUTPUT, MAX_READ, MAX_TIMEOUT_MS, Output, Registry,
};

/// A fresh directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ulei-tools-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn input(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn call(dir: &Path, interrupt: &Interrupt, tool: &str, value: Value) -> Output {
    let ctx = Context {
        cwd: dir,
        interrupt,
    };
    Registry::builtin().execute(tool, &input(value), ctx)
}

fn run(dir: &Path, tool: &str, value: Value) -> Output {
    call(dir, &Interrupt::new(), tool, value)
}

/// The process is gone or a zombie waiting for its reaper.
fn dead(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"));
        let gone = stat.as_ref().map_or(true, |s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        });
        if gone || Instant::now() > deadline {
            return gone;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn registry_advertises_the_four_builtin_tools() {
    let names: Vec<String> = Registry::builtin()
        .definitions()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["shell", "read", "write", "edit"]);
}

#[test]
fn invalid_input_is_an_error_output() {
    let dir = Scratch::new();
    let cases = [
        ("shell", json!({}), "missing field `command`"),
        (
            "shell",
            json!({"command": 5}),
            "field `command` must be a string",
        ),
        (
            "shell",
            json!({"command": "true", "x": 1}),
            "unknown field `x`",
        ),
        ("read", json!({"file_path": "a", "offset": -1}), "`offset`"),
        ("read", json!({"file_path": "a", "limit": 0}), "`limit`"),
        (
            "write",
            json!({"file_path": "a"}),
            "missing field `content`",
        ),
        (
            "edit",
            json!({"file_path": "a", "old_string": "x", "new_string": "y", "replace_all": "yes"}),
            "`replace_all` must be a boolean",
        ),
        ("grep", json!({}), "unknown tool `grep`"),
    ];
    for (tool, value, expected) in cases {
        let out = run(&dir.0, tool, value);
        assert!(out.is_error, "{tool}: {out:?}");
        assert!(out.content.contains(expected), "{tool}: {out:?}");
    }
}

#[test]
fn shell_reports_output_and_exit_status_in_the_working_directory() {
    let dir = Scratch::new();
    let out = run(
        &dir.0,
        "shell",
        json!({"command": "pwd; echo err >&2; exit 3"}),
    );
    assert!(!out.is_error, "{out:?}");
    assert!(
        out.content.contains(&dir.0.display().to_string()),
        "{out:?}"
    );
    assert!(out.content.contains("err\n"), "{out:?}");
    assert!(out.content.ends_with("[exit code 3]"), "{out:?}");
}

#[test]
fn shell_caps_output_with_a_marker() {
    let dir = Scratch::new();
    let total = MAX_OUTPUT + 1000;
    let out = run(
        &dir.0,
        "shell",
        json!({"command": format!("head -c {total} /dev/zero | tr '\\0' a")}),
    );
    assert!(!out.is_error, "{out:?}");
    assert!(out.content.starts_with(&"a".repeat(MAX_OUTPUT)));
    assert!(
        out.content
            .contains("[output truncated: 1000 more bytes dropped]")
    );
    assert!(out.content.len() < MAX_OUTPUT + 100);
}

#[test]
fn shell_rejects_a_timeout_out_of_range() {
    let dir = Scratch::new();
    for timeout in [0, MAX_TIMEOUT_MS + 1] {
        let out = run(
            &dir.0,
            "shell",
            json!({"command": "true", "timeout_ms": timeout}),
        );
        assert!(
            out.is_error && out.content.contains("timeout_ms"),
            "{out:?}"
        );
    }
}

#[test]
fn shell_timeout_kills_the_whole_process_group() {
    let dir = Scratch::new();
    let started = Instant::now();
    let out = run(
        &dir.0,
        "shell",
        json!({"command": "sleep 30 & echo $! > pid; echo before; wait", "timeout_ms": 200}),
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(out.is_error, "{out:?}");
    assert!(out.content.contains("before"), "{out:?}");
    assert!(out.content.contains("timed out after 200 ms"), "{out:?}");
    assert!(dead(fs::read_to_string(dir.path("pid")).unwrap().trim()));
}

#[test]
fn shell_interrupt_from_another_thread_kills_the_whole_process_group() {
    let dir = Scratch::new();
    let interrupt = Interrupt::new();
    let remote = interrupt.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        remote.interrupt();
    });
    let started = Instant::now();
    let out = call(
        &dir.0,
        &interrupt,
        "shell",
        json!({"command": "sleep 30 & echo $! > pid; wait"}),
    );
    trigger.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        out.is_error && out.content.contains("interrupted"),
        "{out:?}"
    );
    assert!(dead(fs::read_to_string(dir.path("pid")).unwrap().trim()));
}

#[test]
fn shell_kills_background_processes_when_the_command_exits() {
    let dir = Scratch::new();
    let started = Instant::now();
    let out = run(
        &dir.0,
        "shell",
        json!({"command": "sleep 30 & echo $! > pid; echo started"}),
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(!out.is_error, "{out:?}");
    assert!(out.content.ends_with("[exit code 0]"), "{out:?}");
    assert!(dead(fs::read_to_string(dir.path("pid")).unwrap().trim()));
}

#[test]
fn an_interrupted_handle_runs_no_further_tool() {
    let dir = Scratch::new();
    let interrupt = Interrupt::new();
    interrupt.interrupt();
    let out = call(&dir.0, &interrupt, "shell", json!({"command": "touch ran"}));
    assert!(
        out.is_error && out.content.contains("interrupted"),
        "{out:?}"
    );
    assert!(!dir.path("ran").exists());
}

#[test]
fn read_returns_text_with_offset_and_limit() {
    let dir = Scratch::new();
    fs::write(dir.path("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    let whole = run(&dir.0, "read", json!({"file_path": "f.txt"}));
    assert_eq!(whole, Output::ok("one\ntwo\nthree\nfour\n"));
    let window = run(
        &dir.0,
        "read",
        json!({"file_path": "f.txt", "offset": 2, "limit": 2}),
    );
    assert_eq!(window, Output::ok("two\nthree\n"));
    let past = run(&dir.0, "read", json!({"file_path": "f.txt", "offset": 9}));
    assert_eq!(past, Output::ok(""));
}

#[test]
fn read_caps_bytes_with_a_marker() {
    let dir = Scratch::new();
    let line = format!("{}\n", "x".repeat(999));
    fs::write(dir.path("big.txt"), line.repeat(MAX_READ / 1000 + 5)).unwrap();
    let out = run(&dir.0, "read", json!({"file_path": "big.txt"}));
    assert!(!out.is_error);
    let (text, marker) = out.content.rsplit_once('\n').unwrap();
    assert_eq!(text.len(), MAX_READ);
    let next = MAX_READ / 1000 + 1;
    assert_eq!(
        marker,
        format!("[truncated at {MAX_READ} bytes; continue with offset {next}]")
    );
}

#[test]
fn read_fails_on_a_missing_file_and_on_non_utf8_content() {
    let dir = Scratch::new();
    fs::write(dir.path("bin"), b"ok\n\xff\xfe\n").unwrap();
    let missing = run(&dir.0, "read", json!({"file_path": "nope"}));
    assert!(
        missing.is_error && missing.content.contains("cannot read"),
        "{missing:?}"
    );
    let binary = run(&dir.0, "read", json!({"file_path": "bin"}));
    assert!(
        binary.is_error && binary.content.contains("not UTF-8"),
        "{binary:?}"
    );
}

#[test]
fn write_creates_parents_and_replaces() {
    let dir = Scratch::new();
    let out = run(
        &dir.0,
        "write",
        json!({"file_path": "a/b/c.txt", "content": "first"}),
    );
    assert!(!out.is_error, "{out:?}");
    run(
        &dir.0,
        "write",
        json!({"file_path": "a/b/c.txt", "content": "second"}),
    );
    assert_eq!(fs::read_to_string(dir.path("a/b/c.txt")).unwrap(), "second");
}

#[test]
fn edit_replaces_a_unique_string() {
    let dir = Scratch::new();
    fs::write(dir.path("e.txt"), "alpha beta gamma").unwrap();
    let out = run(
        &dir.0,
        "edit",
        json!({"file_path": "e.txt", "old_string": "beta", "new_string": "BETA"}),
    );
    assert!(!out.is_error, "{out:?}");
    assert_eq!(
        fs::read_to_string(dir.path("e.txt")).unwrap(),
        "alpha BETA gamma"
    );
}

#[test]
fn edit_fails_on_absent_or_repeated_string_unless_replace_all() {
    let dir = Scratch::new();
    fs::write(dir.path("e.txt"), "a-a-a").unwrap();
    let edit = |old: &str, replace_all: bool| {
        run(
            &dir.0,
            "edit",
            json!({"file_path": "e.txt", "old_string": old, "new_string": "b", "replace_all": replace_all}),
        )
    };
    let absent = edit("z", false);
    assert!(
        absent.is_error && absent.content.contains("not found"),
        "{absent:?}"
    );
    let repeated = edit("a", false);
    assert!(
        repeated.is_error && repeated.content.contains("occurs 3 times"),
        "{repeated:?}"
    );
    assert_eq!(fs::read_to_string(dir.path("e.txt")).unwrap(), "a-a-a");
    assert!(!edit("a", true).is_error);
    assert_eq!(fs::read_to_string(dir.path("e.txt")).unwrap(), "b-b-b");
}

#[test]
fn edit_fails_on_a_missing_file_and_on_non_utf8_content() {
    let dir = Scratch::new();
    fs::write(dir.path("bin"), b"\xff").unwrap();
    for file in ["nope", "bin"] {
        let out = run(
            &dir.0,
            "edit",
            json!({"file_path": file, "old_string": "x", "new_string": "y"}),
        );
        assert!(out.is_error, "{file}: {out:?}");
    }
}
