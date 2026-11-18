//! The plan of the tool calls of one model step: which run concurrently and which in order.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use ulei_engine::tools::{Segment, plan};
use ulei_engine::wire::c4g::ToolCall;

/// A fresh directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ulei-batch-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn call(name: &str, input: Value) -> ToolCall {
    let Value::Object(input) = input else {
        panic!("not an object")
    };
    ToolCall {
        id: String::new(),
        name: name.into(),
        input,
    }
}

fn read(path: &str) -> ToolCall {
    call("read_file", json!({"path": path}))
}

fn write(path: &str) -> ToolCall {
    call("write_file", json!({"path": path, "content": "x"}))
}

fn parallel(calls: std::ops::Range<usize>) -> Segment {
    Segment {
        parallel: true,
        calls,
    }
}

fn sequential(calls: std::ops::Range<usize>) -> Segment {
    Segment {
        parallel: false,
        calls,
    }
}

/// The plan of `calls` in a fresh working directory.
fn planned(calls: &[ToolCall]) -> Vec<Segment> {
    let dir = Scratch::new();
    plan(calls, &dir.0)
}

#[test]
fn readers_of_one_path_and_parallel_safe_tools_form_one_parallel_segment() {
    assert_eq!(
        planned(&[
            read("a.txt"),
            read("a.txt"),
            call("search_files", json!({"pattern": "x"})),
            call("web_search", json!({"query": "q"})),
            read("./a.txt"),
        ]),
        [parallel(0..5)]
    );
    assert_eq!(planned(&[write("a.txt"), write("b.txt")]), [parallel(0..2)]);
}

#[test]
fn a_write_followed_by_a_read_of_the_same_file_runs_in_order() {
    assert_eq!(
        planned(&[write("a.txt"), read("a.txt")]),
        [sequential(0..2)]
    );
    assert_eq!(
        planned(&[read("a.txt"), write("a.txt")]),
        [sequential(0..2)]
    );
    // The conflicting write opens a new run that later calls may join.
    assert_eq!(
        planned(&[read("a.txt"), read("b.txt"), write("a.txt"), read("c.txt")]),
        [parallel(0..2), parallel(2..4)]
    );
}

#[test]
fn a_writer_conflicts_with_any_path_above_or_below_it() {
    assert_eq!(
        planned(&[write("src/lib.rs"), read("src")]),
        [sequential(0..2)]
    );
    assert_eq!(
        planned(&[
            call("search_files", json!({"pattern": "x", "path": "src"})),
            write("src/deep/new.rs"),
        ]),
        [sequential(0..2)]
    );
    // A search without a path holds the working directory, so it waits for any write in it.
    assert_eq!(
        planned(&[
            write("a.txt"),
            call("search_files", json!({"pattern": "x"}))
        ]),
        [sequential(0..2)]
    );
    assert_eq!(
        planned(&[
            write("a.txt"),
            call("search_files", json!({"pattern": "x", "path": "  "})),
        ]),
        [sequential(0..2)]
    );
    assert_eq!(
        planned(&[write("src/a.rs"), read("srcs/a.rs")]),
        [parallel(0..2)]
    );
}

#[test]
fn paths_are_compared_after_resolving_symlinks_and_dot_dot() {
    let dir = Scratch::new();
    fs::create_dir(dir.0.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.0.join("real"), dir.0.join("alias")).unwrap();
    let absolute = dir.0.join("real/a.txt").display().to_string();
    assert_eq!(
        plan(&[write("alias/a.txt"), read(&absolute)], &dir.0),
        [sequential(0..2)]
    );
    assert_eq!(
        plan(&[write("x/../real/a.txt"), read("real/a.txt")], &dir.0),
        [sequential(0..2)]
    );
}

#[test]
fn barriers_split_the_parallel_runs_around_them() {
    assert_eq!(
        planned(&[
            read("a.txt"),
            read("b.txt"),
            call("terminal", json!({"command": "ls"})),
            read("c.txt"),
            read("d.txt"),
        ]),
        [parallel(0..2), sequential(2..3), parallel(3..5)]
    );
    // A destructive command is a barrier like any other terminal call.
    assert_eq!(
        planned(&[
            read("a.txt"),
            call("terminal", json!({"command": "rm -rf a.txt"})),
            read("b.txt"),
        ]),
        [sequential(0..3)]
    );
    assert_eq!(
        planned(&[
            call("clarify", json!({"question": "?"})),
            call("unknown_tool", json!({})),
            read("a.txt"),
            read("b.txt"),
        ]),
        [sequential(0..2), parallel(2..4)]
    );
}

#[test]
fn a_call_without_a_known_path_is_a_barrier() {
    assert_eq!(
        planned(&[
            read("a.txt"),
            call("write_file", json!({"content": "x"})),
            read("b.txt"),
            call("read_file", json!({"path": 7})),
            read("c.txt"),
            call("patch", json!({"mode": "patch", "patch": "no headers"})),
        ]),
        [sequential(0..6)]
    );
}

#[test]
fn a_v4a_patch_holds_its_header_paths_not_its_path_argument() {
    let patch = |body: &str| {
        call(
            "patch",
            json!({"mode": "patch", "path": "decoy.txt", "patch": body}),
        )
    };
    let update = "*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n*** End Patch\n";
    assert_eq!(
        planned(&[patch(update), read("decoy.txt"), read("b.txt")]),
        [parallel(0..3)]
    );
    assert_eq!(
        planned(&[patch(update), read("b.txt"), read("a.txt")]),
        [parallel(0..2), sequential(2..3)]
    );
    let moved = "*** Begin Patch\n*** Move File: m.txt -> n.txt\n*** End Patch\n";
    assert_eq!(
        planned(&[read("x.txt"), patch(moved), read("n.txt")]),
        [parallel(0..2), sequential(2..3)]
    );
    // In replace mode the path argument is the scope.
    let replace = call(
        "patch",
        json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
    );
    assert_eq!(planned(&[replace, read("a.txt")]), [sequential(0..2)]);
}
