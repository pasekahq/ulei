//! The built-in tools through the registry: input validation, shell limits and interrupts,
//! the file tools and the search tools.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use ulei_engine::tools::{
    Context, Interrupt, MAX_GREP_OUTPUT, MAX_LINE, MAX_MATCHES, MAX_OUTPUT, MAX_READ,
    MAX_SCAN_BYTES, MAX_TIMEOUT_MS, Output, Registry,
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
fn registry_advertises_the_six_builtin_tools() {
    let names: Vec<String> = Registry::builtin()
        .definitions()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["shell", "read", "write", "edit", "grep", "glob"]);
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
        ("grep", json!({}), "missing field `pattern`"),
        (
            "grep",
            json!({"pattern": ["a"]}),
            "field `pattern` must be a string",
        ),
        (
            "grep",
            json!({"pattern": "a", "path": 1}),
            "field `path` must be a string",
        ),
        (
            "grep",
            json!({"pattern": "a", "case_insensitive": "yes"}),
            "`case_insensitive` must be a boolean",
        ),
        (
            "grep",
            json!({"pattern": "a", "files_only": 1}),
            "`files_only` must be a boolean",
        ),
        (
            "grep",
            json!({"pattern": "a", "glob": "[a"}),
            "unclosed `[`",
        ),
        (
            "grep",
            json!({"pattern": "(unclosed"}),
            "invalid regular expression",
        ),
        (
            "grep",
            json!({"pattern": "a", "path": "nope"}),
            "cannot read",
        ),
        ("glob", json!({}), "missing field `pattern`"),
        (
            "glob",
            json!({"pattern": "*", "path": false}),
            "field `path` must be a string",
        ),
        ("glob", json!({"pattern": "/etc/*"}), "must be relative"),
        ("glob", json!({"pattern": "a/[b"}), "unclosed `[`"),
        (
            "glob",
            json!({"pattern": "*", "path": "nope"}),
            "cannot read",
        ),
        ("web", json!({}), "unknown tool `web`"),
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

/// Creates each file with its content, parents included.
fn tree(dir: &Scratch, files: &[(&str, &str)]) {
    for (name, content) in files {
        let path = dir.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn ok(dir: &Path, tool: &str, value: Value) -> String {
    let out = run(dir, tool, value);
    assert!(!out.is_error, "{out:?}");
    out.content
}

#[test]
fn glob_matches_double_star_classes_and_question_marks_sorted_and_relative() {
    let dir = Scratch::new();
    tree(
        &dir,
        &[
            ("src/main.rs", ""),
            ("src/a/b/lib.rs", ""),
            ("src/a/notes.md", ""),
            ("top.rs", ""),
            ("x1.txt", ""),
            ("x2.txt", ""),
            ("x3.txt", ""),
            ("xa.txt", ""),
        ],
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "**/*.rs"})),
        "src/a/b/lib.rs\nsrc/main.rs\ntop.rs"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "src/**/*.rs"})),
        "src/a/b/lib.rs\nsrc/main.rs"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "x[1-2a].txt"})),
        "x1.txt\nx2.txt\nxa.txt"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "x[!1-2].txt"})),
        "x3.txt\nxa.txt"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "x?.txt"})),
        "x1.txt\nx2.txt\nx3.txt\nxa.txt"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "*", "path": "src/a"})),
        "b/\nnotes.md"
    );
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "*.none"})),
        "no matches"
    );
}

#[test]
fn glob_caps_the_count_with_a_marker() {
    let dir = Scratch::new();
    let total = MAX_MATCHES + 5;
    for i in 0..total {
        fs::write(dir.path(&format!("f{i:05}")), "").unwrap();
    }
    let content = ok(&dir.0, "glob", json!({"pattern": "f*"}));
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), MAX_MATCHES + 1);
    assert_eq!(lines[0], "f00000");
    assert_eq!(lines[MAX_MATCHES - 1], format!("f{:05}", MAX_MATCHES - 1));
    assert_eq!(
        lines[MAX_MATCHES],
        format!("[{MAX_MATCHES} of {total} matches shown; narrow `pattern` or `path`]")
    );
}

#[test]
fn glob_skips_git_and_does_not_enter_symlinked_directories() {
    let dir = Scratch::new();
    let outside = Scratch::new();
    tree(&dir, &[("real/a.rs", ""), (".git/config.rs", "")]);
    tree(&outside, &[("hidden.rs", "")]);
    std::os::unix::fs::symlink(&outside.0, dir.path("link")).unwrap();
    assert_eq!(
        ok(&dir.0, "glob", json!({"pattern": "**"})),
        "link\nreal/\nreal/a.rs"
    );
}

#[test]
fn grep_prints_path_line_and_text_with_long_lines_shortened() {
    let dir = Scratch::new();
    let long = format!("needle {}", "é".repeat(MAX_LINE));
    tree(
        &dir,
        &[
            ("a.txt", "one\nneedle two\nthree\n"),
            ("sub/b.txt", &format!("x\n{long}\n")),
            ("sub/c.txt", "nothing here\n"),
        ],
    );
    let content = ok(&dir.0, "grep", json!({"pattern": "need?le"}));
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "{content}");
    assert_eq!(lines[0], "a.txt:2: needle two");
    let shown = lines[1].strip_prefix("sub/b.txt:2: ").unwrap();
    let (kept, marker) = shown.split_once(" [line truncated: ").unwrap();
    assert!(long.starts_with(kept) && kept.len() <= MAX_LINE && kept.len() > MAX_LINE - 2);
    assert_eq!(marker, format!("{} more bytes]", long.len() - kept.len()));
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "two", "path": "a.txt"})),
        "a.txt:2: needle two"
    );
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "x", "path": "sub/"})),
        "sub/b.txt:1: x"
    );
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "absent"})),
        "no matches"
    );
}

#[test]
fn grep_folds_case_on_request() {
    let dir = Scratch::new();
    tree(&dir, &[("a.txt", "Alpha\nalpha\nALPHA\n")]);
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "alpha"})),
        "a.txt:2: alpha"
    );
    assert_eq!(
        ok(
            &dir.0,
            "grep",
            json!({"pattern": "alpha", "case_insensitive": true})
        ),
        "a.txt:1: Alpha\na.txt:2: alpha\na.txt:3: ALPHA"
    );
}

#[test]
fn grep_files_only_lists_each_matching_file_once() {
    let dir = Scratch::new();
    tree(
        &dir,
        &[
            ("a.txt", "hit\nhit\n"),
            ("b.txt", "miss\n"),
            ("c/d.txt", "hit\n"),
        ],
    );
    assert_eq!(
        ok(
            &dir.0,
            "grep",
            json!({"pattern": "hit", "files_only": true})
        ),
        "a.txt\nc/d.txt"
    );
}

#[test]
fn grep_glob_filter_matches_names_or_paths() {
    let dir = Scratch::new();
    tree(
        &dir,
        &[
            ("a.rs", "hit\n"),
            ("a.md", "hit\n"),
            ("src/deep/b.rs", "hit\n"),
            ("doc/c.rs", "hit\n"),
        ],
    );
    let grep = |glob: &str| {
        ok(
            &dir.0,
            "grep",
            json!({"pattern": "hit", "glob": glob, "files_only": true}),
        )
    };
    assert_eq!(grep("*.rs"), "a.rs\ndoc/c.rs\nsrc/deep/b.rs");
    assert_eq!(grep("src/**/*.[r]s"), "src/deep/b.rs");
    assert_eq!(grep("*.txt"), "no matches");
}

#[test]
fn grep_skips_binary_files_git_and_symlinked_directories() {
    let dir = Scratch::new();
    let outside = Scratch::new();
    tree(&dir, &[("text.txt", "hit\n"), (".git/HEAD", "hit\n")]);
    fs::write(dir.path("blob.bin"), b"hit\n\0hit\n").unwrap();
    tree(&outside, &[("o.txt", "hit\n")]);
    std::os::unix::fs::symlink(&outside.0, dir.path("link")).unwrap();
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "hit"})),
        "text.txt:1: hit\n[1 binary file(s) skipped]"
    );
}

#[test]
fn grep_caps_output_with_a_marker() {
    let dir = Scratch::new();
    let line = format!("hit {}\n", "x".repeat(60));
    tree(&dir, &[("big.txt", &line.repeat(MAX_GREP_OUTPUT / 60))]);
    let content = ok(&dir.0, "grep", json!({"pattern": "hit"}));
    let (shown, marker) = content.rsplit_once('\n').unwrap();
    assert_eq!(
        marker,
        format!(
            "[output truncated at {MAX_GREP_OUTPUT} bytes; narrow `pattern`, `path` or `glob`]"
        )
    );
    assert!(shown.len() <= MAX_GREP_OUTPUT && shown.len() > MAX_GREP_OUTPUT - 100);
    assert!(shown.lines().all(|l| l.starts_with("big.txt:")));
}

#[test]
fn grep_stops_at_the_byte_scan_limit_with_a_marker() {
    let dir = Scratch::new();
    tree(&dir, &[("a.txt", "hit\n")]);
    let len = usize::try_from(MAX_SCAN_BYTES).unwrap();
    fs::write(dir.path("b.txt"), "x".repeat(len)).unwrap();
    assert_eq!(
        ok(&dir.0, "grep", json!({"pattern": "hit"})),
        format!(
            "a.txt:1: hit\n[scan stopped after {MAX_SCAN_BYTES} bytes read; \
             narrow `path` or `glob`]"
        )
    );
}
