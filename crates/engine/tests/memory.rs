//! The `memory` tool through the registry: the single-op and batch shapes on both stores, the
//! limits, the match and missing-field answers, the per-turn failure cap, the lock that rereads
//! the store and the atomic replace of the file.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fake_gateway::Scratch;
use rustix::fs::{FlockOperation, flock};
use serde_json::{Value, json};
use ulei_engine::prompt::Brain;
use ulei_engine::prompt::core::Core;
use ulei_engine::skills::Skills;
use ulei_engine::tools::{Context, Interrupt, Memory, Output, Registry};

const ALPHA_BETA: &str = "Synthetic alpha fact.\n§\nSynthetic beta fact.";
const NOTE: &str = "\"note\": \"Write saved. This update is complete — do not repeat it.\"}";

/// A registry holding the `memory` tool of one turn over the stores in a scratch directory.
struct Stores {
    dir: Scratch,
    registry: Registry,
}

impl Stores {
    /// `MEMORY.md` holding `memory` under `memory_limit` chars, an absent `USER.md` under
    /// `user_limit`.
    fn new(memory: &str, memory_limit: usize, user_limit: usize) -> Self {
        let dir = Scratch::new();
        let brain = Brain {
            soul: dir.path("SOUL.md"),
            index: dir.path("INDEX.md"),
            memory: dir.path("memories/MEMORY.md"),
            user: dir.path("memories/USER.md"),
            memory_char_limit: memory_limit,
            user_char_limit: user_limit,
            skills: Skills {
                dir: dir.path("skills"),
                disabled: Vec::new(),
            },
            skills_index: false,
            core: Core::defaults(None),
        };
        fs::create_dir_all(dir.path("memories")).unwrap();
        fs::write(&brain.memory, memory).unwrap();
        let mut registry = Registry::new();
        registry.register(Box::new(Memory::new(&brain))).unwrap();
        Self { dir, registry }
    }

    fn call(&self, input: Value) -> Output {
        let Value::Object(input) = input else {
            panic!("not an object: {input}")
        };
        let interrupt = Interrupt::new();
        let ctx = Context {
            cwd: &self.dir.0,
            interrupt: &interrupt,
        };
        self.registry.execute("memory", &input, ctx)
    }

    fn answer(&self, input: Value) -> String {
        self.call(input).content
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path("memories").join(name)
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap()
    }
}

fn success(target: &str, usage: &str, count: usize, message: &str) -> String {
    format!(
        "{{\"success\": true, \"done\": true, \"target\": \"{target}\", \"usage\": \"{usage}\", \
         \"entry_count\": {count}, \"message\": \"{message}\", {NOTE}"
    )
}

#[test]
fn add_replace_and_remove_write_the_memory_store_and_answer_with_its_usage() {
    let s = Stores::new("", 3500, 2200);
    let added =
        s.call(json!({"action": "add", "target": "memory", "content": "Synthetic alpha fact."}));
    assert_eq!(
        added.content,
        success("memory", "0% — 21/3,500 chars", 1, "Entry added.")
    );
    assert!(!added.is_error);
    assert_eq!(
        s.answer(json!({"action": "add", "content": "  Synthetic beta fact.  "})),
        success("memory", "1% — 44/3,500 chars", 2, "Entry added.")
    );
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);
    assert_eq!(
        s.answer(json!({"action": "add", "content": "Synthetic alpha fact."})),
        success(
            "memory",
            "1% — 44/3,500 chars",
            2,
            "Entry already exists (no duplicate added)."
        )
    );
    assert_eq!(
        s.answer(
            json!({"action": "replace", "old_text": "alpha", "content": "Synthetic gamma fact."})
        ),
        success("memory", "1% — 44/3,500 chars", 2, "Entry replaced.")
    );
    assert_eq!(
        s.file("MEMORY.md"),
        "Synthetic gamma fact.\n§\nSynthetic beta fact."
    );
    assert_eq!(
        s.answer(json!({"action": "remove", "target": "memory", "old_text": "beta"})),
        success("memory", "0% — 21/3,500 chars", 1, "Entry removed.")
    );
    assert_eq!(s.file("MEMORY.md"), "Synthetic gamma fact.");
    assert!(!s.path("USER.md").exists());
}

#[test]
fn add_replace_and_remove_write_the_user_store_counting_chars() {
    let s = Stores::new(ALPHA_BETA, 3500, 2200);
    let user = |input: Value| {
        let mut input = input;
        input["target"] = "user".into();
        s.answer(input)
    };
    assert_eq!(
        user(json!({"action": "add", "content": "Пользователь любит чай."})),
        success("user", "1% — 23/2,200 chars", 1, "Entry added.")
    );
    assert_eq!(
        user(json!({"action": "add", "content": "Synthetic user prefers mornings."})),
        success("user", "2% — 58/2,200 chars", 2, "Entry added.")
    );
    assert_eq!(
        user(
            json!({"action": "replace", "old_text": "чай", "content": "Пользователь любит кофе."})
        ),
        success("user", "2% — 59/2,200 chars", 2, "Entry replaced.")
    );
    assert_eq!(
        user(json!({"action": "remove", "old_text": "mornings"})),
        success("user", "1% — 24/2,200 chars", 1, "Entry removed.")
    );
    assert_eq!(s.file("USER.md"), "Пользователь любит кофе.");
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);
}

#[test]
fn an_add_or_replace_over_the_limit_is_refused_and_leaves_the_file_unchanged() {
    let s = Stores::new(ALPHA_BETA, 60, 60);
    let refused = s.call(
        json!({"action": "add", "content": "Synthetic entry that is far too long for the room."}),
    );
    assert_eq!(
        refused.content,
        "{\"success\": false, \"error\": \"Memory at 44/60 chars. Adding this entry (50 chars) \
         would exceed the limit. Consolidate now: use 'replace' to merge overlapping entries into \
         shorter ones or 'remove' stale or less important entries (see current_entries below), \
         then retry this add — all in this turn.\", \"current_entries\": [\"Synthetic alpha \
         fact.\", \"Synthetic beta fact.\"], \"usage\": \"44/60\"}"
    );
    assert!(refused.is_error);
    assert_eq!(
        s.answer(json!({"action": "replace", "old_text": "alpha", "content": "A replacement that grows the store past its limit."})),
        "{\"success\": false, \"error\": \"Replacement would put memory at 73/60 chars. Shorten \
         the new content, or 'remove' other stale or less important entries to make room (see \
         current_entries below), then retry — all in this turn.\", \"current_entries\": \
         [\"Synthetic alpha fact.\", \"Synthetic beta fact.\"], \"usage\": \"44/60\"}"
    );
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);

    let filler = "Synthetic filler. ".repeat(194).trim_end().to_owned();
    let s = Stores::new(&filler, 3500, 60);
    let answer =
        s.answer(json!({"action": "add", "content": "Synthetic entry that does not fit."}));
    assert!(answer.starts_with(
        "{\"success\": false, \"error\": \"Memory at 3,491/3,500 chars. Adding this entry (34 \
         chars) would exceed the limit."
    ));
    assert!(answer.ends_with("\"usage\": \"3,491/3,500\"}"));
    assert_eq!(s.file("MEMORY.md"), filler);
}

#[test]
fn replace_and_remove_without_one_matching_entry_or_without_old_text_answer_the_reference_texts() {
    let s = Stores::new(ALPHA_BETA, 100, 60);
    let entries = "\"current_entries\": [\"Synthetic alpha fact.\", \"Synthetic beta fact.\"]";
    for verb in ["replace", "remove"] {
        assert_eq!(
            s.answer(json!({"action": verb, "old_text": "delta", "content": "x"})),
            format!(
                "{{\"success\": false, \"error\": \"No entry matched 'delta'. Check \
                 current_entries below and retry with the exact text of the entry you want to \
                 {verb}.\", {entries}}}"
            )
        );
    }
    assert_eq!(
        s.answer(json!({"action": "replace", "old_text": "Synthetic", "content": "x"})),
        "{\"success\": false, \"error\": \"Multiple entries matched 'Synthetic'. Be more \
         specific.\", \"matches\": [\"Synthetic alpha fact.\", \"Synthetic beta fact.\"]}"
    );
    for verb in ["replace", "remove"] {
        assert_eq!(
            s.answer(json!({"action": verb, "content": "x"})),
            format!(
                "{{\"success\": false, \"error\": \"'{verb}' needs old_text -- a short unique \
                 substring of the entry to {verb}. None was provided. Reissue the {verb} with \
                 old_text set to part of one of the current_entries below.\", {entries}, \
                 \"usage\": \"44/100\"}}"
            )
        );
    }
    assert_eq!(
        s.answer(json!({"action": "replace", "old_text": "alpha"})),
        "{\"error\": \"content is required for 'replace' action.\", \"success\": false}"
    );
    assert_eq!(
        s.answer(json!({"action": "add"})),
        "{\"error\": \"Content is required for 'add' action.\", \"success\": false}"
    );
    assert_eq!(
        s.answer(json!({"action": "add", "content": "   "})),
        "{\"success\": false, \"error\": \"Content cannot be empty.\"}"
    );
    assert_eq!(
        s.answer(json!({"action": "replace", "old_text": "  ", "content": "x"})),
        "{\"success\": false, \"error\": \"old_text cannot be empty.\"}"
    );
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);

    let long = format!("Synthetic {}", "a".repeat(90));
    let s = Stores::new(&format!("{long}\n§\nSynthetic short b."), 200, 60);
    assert_eq!(
        s.answer(json!({"action": "remove", "old_text": "Synthetic"})),
        format!(
            "{{\"success\": false, \"error\": \"Multiple entries matched 'Synthetic'. Be more \
             specific.\", \"matches\": [\"{}...\", \"Synthetic short b.\"]}}",
            &long[..80]
        )
    );
}

#[test]
fn a_batch_is_checked_against_its_final_budget_and_writes_nothing_when_it_fails() {
    let s = Stores::new(ALPHA_BETA, 60, 60);
    let usage = "\"usage\": \"44/60\"}";
    let entries = "\"current_entries\": [\"Synthetic alpha fact.\", \"Synthetic beta fact.\"]";
    assert!(
        s.answer(json!({"action": "add", "content": "Synthetic delta fact here."}))
            .starts_with("{\"success\": false, \"error\": \"Memory at 44/60 chars.")
    );
    assert_eq!(
        s.answer(json!({"operations": [
            {"action": "add", "content": "Synthetic epsilon."},
            {"action": "replace", "old_text": "zeta", "content": "x"}
        ]})),
        format!(
            "{{\"success\": false, \"error\": \"Operation 2 (replace): no entry matched 'zeta'. \
             No operations were applied (batch is all-or-nothing).\", {entries}, {usage}"
        )
    );
    assert_eq!(
        s.answer(json!({"operations": [
            {"action": "add", "content": "Synthetic epsilon."},
            {"action": "add", "content": "Synthetic zeta."}
        ]})),
        format!(
            "{{\"success\": false, \"error\": \"After applying all 2 operations, memory would be \
             at 83/60 chars -- over the limit. Remove or shorten more entries in the same batch \
             (see current_entries below), then retry.\", {entries}, {usage}"
        )
    );
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);
    assert_eq!(
        s.answer(json!({"target": "memory", "operations": [
            {"action": "remove", "old_text": "alpha"},
            {"action": "add", "content": "Synthetic delta fact here."}
        ]})),
        success("memory", "81% — 49/60 chars", 2, "Applied 2 operation(s).")
    );
    assert_eq!(
        s.file("MEMORY.md"),
        "Synthetic beta fact.\n§\nSynthetic delta fact here."
    );
}

#[test]
fn a_failing_batch_operation_is_named_by_its_position_and_action() {
    let cases = [
        (
            json!([{"action": "move"}]),
            "Operation 1 (move): unknown action. Use add, replace, or remove.",
        ),
        (
            json!([{"action": "remove"}]),
            "Operation 1 (remove): old_text is required.",
        ),
        (
            json!([{"action": "replace", "old_text": "beta"}]),
            "Operation 1 (replace): content is required (use action='remove' to delete).",
        ),
        (
            json!([{"action": "add"}]),
            "Operation 1 (add): content is required.",
        ),
        (
            json!([null, {"old_text": "x"}]),
            "Operation 1 (unknown): unknown action. Use add, replace, or remove.",
        ),
        (
            json!([{"action": "remove", "old_text": "Synthetic"}]),
            "Operation 1 (remove): 'Synthetic' matched multiple distinct entries -- be more \
             specific.",
        ),
        (
            json!([{"action": "remove", "old_text": "delta"}]),
            "Operation 1 (remove): no entry matched 'delta'.",
        ),
    ];
    for (operations, error) in cases {
        let s = Stores::new(ALPHA_BETA, 100, 60);
        assert_eq!(
            s.answer(json!({"operations": operations})),
            format!(
                "{{\"success\": false, \"error\": \"{error} No operations were applied (batch is \
                 all-or-nothing).\", \"current_entries\": [\"Synthetic alpha fact.\", \
                 \"Synthetic beta fact.\"], \"usage\": \"44/100\"}}"
            )
        );
        assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);
    }
}

#[test]
fn new_text_stands_for_content_unless_content_is_set() {
    let s = Stores::new(ALPHA_BETA, 200, 60);
    assert_eq!(
        s.answer(json!({"action": "replace", "old_text": "alpha", "new_text": "Synthetic alpha, renamed."})),
        success("memory", "24% — 48/200 chars", 2, "Entry replaced.")
    );
    s.call(json!({"action": "replace", "old_text": "beta", "content": "Synthetic beta from content.", "new_text": "Ignored text."}));
    s.call(json!({"action": "add", "new_text": "Synthetic added by alias."}));
    assert_eq!(
        s.answer(json!({"operations": [
            {"action": "replace", "old_text": "renamed", "new_text": "Synthetic alpha, revised."},
            {"action": "add", "content": "Synthetic batch content.", "new_text": "Ignored batch text."}
        ]})),
        success("memory", "55% — 111/200 chars", 4, "Applied 2 operation(s).")
    );
    assert_eq!(
        s.file("MEMORY.md"),
        "Synthetic alpha, revised.\n§\nSynthetic beta from content.\n§\nSynthetic added by \
         alias.\n§\nSynthetic batch content."
    );
}

#[test]
fn the_fourth_failed_consolidation_of_a_turn_ends_the_retries_until_a_write_lands() {
    let s = Stores::new(ALPHA_BETA, 100, 60);
    let miss = || s.answer(json!({"action": "remove", "old_text": "delta"}));
    let no_match = "{\"success\": false, \"error\": \"No entry matched 'delta'. Check \
                    current_entries below and retry with the exact text of the entry you want to \
                    remove.\", \"current_entries\": [";
    for _ in 0..3 {
        assert!(miss().starts_with(no_match));
    }
    assert_eq!(
        miss(),
        "{\"success\": false, \"done\": true, \"error\": \"Memory consolidation failed 4 times \
         this turn. Stop retrying memory calls — leave memory unchanged for now and continue \
         with your reply to the user. The fact can be saved in a later turn.\"}"
    );
    assert_eq!(
        s.answer(json!({"action": "remove", "old_text": "beta"})),
        success("memory", "21% — 21/100 chars", 1, "Entry removed.")
    );
    assert_eq!(miss(), format!("{no_match}\"Synthetic alpha fact.\"]}}"));
}

#[test]
fn an_invalid_target_or_action_and_operations_that_are_no_list_are_refused() {
    let s = Stores::new(ALPHA_BETA, 100, 60);
    assert_eq!(
        s.answer(json!({"action": "add", "target": "заметки", "content": "x"})),
        "{\"success\": false, \"error\": \"Invalid memory target \
         '\\u0437\\u0430\\u043c\\u0435\\u0442\\u043a\\u0438'. Use 'memory' or 'user'.\"}"
    );
    assert_eq!(
        s.answer(json!({"action": "move", "content": "x"})),
        "{\"error\": \"Unknown action 'move'. Use: add, replace, remove\", \"success\": false}"
    );
    assert_eq!(
        s.answer(json!({"content": "x"})),
        "{\"error\": \"Unknown action ''. Use: add, replace, remove\", \"success\": false}"
    );
    assert_eq!(
        s.answer(json!({"action": "add", "operations": "not a list"})),
        "{\"error\": \"operations must be a list of {action, content?, old_text?} objects.\", \
         \"success\": false}"
    );
    assert_eq!(
        s.answer(json!({"action": "add", "content": 5})),
        "{\"error\": \"[TOOL_ERROR] Tool execution failed: AttributeError: 'int' object has no \
         attribute 'strip'\"}"
    );
    assert_eq!(s.file("MEMORY.md"), ALPHA_BETA);
    assert_eq!(
        s.answer(json!({"action": "add", "target": null, "content": "Synthetic default target."})),
        success("memory", "72% — 72/100 chars", 3, "Entry added.")
    );
}

#[test]
fn an_unreadable_store_is_never_written() {
    let s = Stores::new("", 100, 60);
    fs::write(s.path("MEMORY.md"), b"Synthetic \xff bytes.").unwrap();
    let refused = "{\"success\": false, \"error\": \"Refusing to write MEMORY.md: the file exists \
                   on disk but could not be read right now (temporarily locked by another \
                   program, a permission change, invalid/corrupt text encoding, or a filesystem \
                   error). Treating an unreadable file as empty and saving would wipe existing \
                   memory, so the write is refused. Nothing was changed — retry in a moment.\"}";
    assert_eq!(
        s.answer(json!({"action": "add", "content": "Synthetic new."})),
        refused
    );
    assert_eq!(
        s.answer(json!({"operations": [{"action": "add", "content": "Synthetic new."}]})),
        refused
    );
    assert_eq!(
        fs::read(s.path("MEMORY.md")).unwrap(),
        b"Synthetic \xff bytes."
    );
}

#[test]
fn a_write_waits_for_the_lock_and_keeps_an_edit_made_while_it_waited() {
    let s = Stores::new(ALPHA_BETA, 200, 60);
    let held = File::options()
        .read(true)
        .append(true)
        .create(true)
        .open(s.path("MEMORY.md.lock"))
        .unwrap();
    flock(&held, FlockOperation::LockExclusive).unwrap();
    thread::scope(|scope| {
        let writer =
            scope.spawn(|| s.answer(json!({"action": "add", "content": "Synthetic tool fact."})));
        thread::sleep(Duration::from_millis(300));
        assert!(!writer.is_finished());
        fs::write(
            s.path("MEMORY.md"),
            format!("{ALPHA_BETA}\n§\nSynthetic external fact."),
        )
        .unwrap();
        drop(held);
        assert_eq!(
            writer.join().unwrap(),
            success("memory", "47% — 94/200 chars", 4, "Entry added.")
        );
    });
    assert_eq!(
        s.file("MEMORY.md"),
        format!("{ALPHA_BETA}\n§\nSynthetic external fact.\n§\nSynthetic tool fact.")
    );
}

#[test]
fn a_write_replaces_the_file_behind_a_link_with_an_owner_only_file() {
    let s = Stores::new("", 100, 60);
    fs::write(s.dir.path("kept.md"), ALPHA_BETA).unwrap();
    fs::remove_file(s.path("MEMORY.md")).unwrap();
    symlink(s.dir.path("kept.md"), s.path("MEMORY.md")).unwrap();
    s.call(json!({"action": "remove", "old_text": "alpha"}));
    let link = fs::symlink_metadata(s.path("MEMORY.md")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(s.dir.path("kept.md")).unwrap(),
        "Synthetic beta fact."
    );
    let mode = fs::metadata(s.dir.path("kept.md"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let names: Vec<_> = fs::read_dir(&s.dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

const BLANK: &str = "Synthetic alpha fact.\n§\n\n§\nSynthetic beta fact.";
const PADDED: &str =
    "Synthetic alpha fact.\n§\n  Synthetic padded fact.  \n§\nSynthetic beta fact.";

/// A store whose second entry is over a limit of 100 chars.
fn oversized() -> String {
    format!(
        "{ALPHA_BETA} {}",
        "Synthetic text another writer appended. ".repeat(3)
    )
}

/// The unix seconds now.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The `.bak.` files beside the stores.
fn backups(s: &Stores) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(s.path(""))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(".bak."))
        .collect();
    names.sort();
    names
}

/// The refusal of a write to `file` whose snapshot is named `backup`.
fn drifted(file: &str, backup: &str) -> String {
    format!(
        "{{\"success\": false, \"error\": \"Refusing to write {file}: file on disk has content \
         that wouldn't round-trip through the memory tool (likely added by the patch tool, a \
         shell append, a manual edit, or a concurrent session). A snapshot was saved to \
         {backup}. Resolve the drift first — either rewrite the file as a clean §-delimited \
         list of entries, or move the extra content out — then retry. This guard exists to \
         prevent silent data loss (issue #26045).\", \"drift_backup\": \"{backup}\", \
         \"remediation\": \"Open the .bak file, integrate the missing entries into the memory \
         tool one at a time via memory(action=add, content=...), then remove or rewrite the \
         original file to a clean state.\"}}"
    )
}

/// Runs `input` on `target` of a fresh store holding `raw` and checks the refusal: the snapshot
/// `<file>.bak.<unix seconds>` of mode 0600 holds `snapshot`, the store keeps its bytes.
fn assert_refused(raw: &str, snapshot: &str, target: &str, input: &Value) {
    let s = Stores::new("", 100, 100);
    let file = if target == "user" {
        "USER.md"
    } else {
        "MEMORY.md"
    };
    fs::write(s.path(file), raw).unwrap();
    let mut input = input.clone();
    input["target"] = target.into();
    let before = now();
    let out = s.call(input.clone());
    let after = now();
    let names = backups(&s);
    assert_eq!(names.len(), 1, "{input} {names:?}");
    let secs: u64 = names[0]
        .strip_prefix(&format!("{file}.bak."))
        .unwrap()
        .parse()
        .unwrap();
    assert!((before..=after).contains(&secs), "{secs} {before} {after}");
    let backup = s.path(&names[0]);
    assert_eq!(
        out.content,
        drifted(file, &backup.display().to_string()),
        "{input}"
    );
    assert!(out.is_error);
    assert_eq!(fs::read_to_string(&backup).unwrap(), snapshot);
    let mode = fs::metadata(&backup).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(fs::read_to_string(s.path(file)).unwrap(), raw, "{input}");
}

#[test]
fn a_replace_remove_or_batch_on_a_store_that_does_not_round_trip_is_refused_with_a_snapshot() {
    let shapes = [
        json!({"action": "replace", "old_text": "alpha", "content": "Synthetic new fact."}),
        json!({"action": "remove", "old_text": "alpha"}),
        json!({"action": "remove", "old_text": "matches no entry"}),
        json!({"operations": [{"action": "add", "content": "Synthetic new fact."}]}),
        json!({"operations": [{"action": "merge"}]}),
    ];
    for raw in [BLANK, PADDED, &oversized()] {
        for input in &shapes {
            assert_refused(raw, raw, "memory", input);
        }
    }
    assert_refused(BLANK, BLANK, "user", &shapes[1]);
}

#[test]
fn a_remove_on_a_store_with_a_byte_order_mark_a_duplicate_a_blank_and_a_padded_entry_is_refused() {
    let raw = "Synthetic alpha fact.\n§\nSynthetic alpha fact.\n§\n\n§\n   Synthetic padded \
               fact.   \n§\nSynthetic beta fact.";
    let s = Stores::new(&format!("\u{feff}{raw}"), 3500, 1375);
    let out = s.answer(json!({"action": "remove", "target": "memory", "old_text": "beta"}));
    let names = backups(&s);
    assert_eq!(names.len(), 1, "{names:?}");
    let backup = s.path(&names[0]);
    assert_eq!(out, drifted("MEMORY.md", &backup.display().to_string()));
    assert_eq!(fs::read_to_string(&backup).unwrap(), raw);
    assert_eq!(s.file("MEMORY.md"), format!("\u{feff}{raw}"));
}

#[test]
fn an_add_appends_to_a_store_that_does_not_round_trip_without_a_snapshot() {
    let s = Stores::new(BLANK, 100, 100);
    assert_eq!(
        s.answer(json!({"action": "add", "content": "Synthetic new fact."})),
        success("memory", "66% — 66/100 chars", 3, "Entry added.")
    );
    assert_eq!(
        s.file("MEMORY.md"),
        format!("{ALPHA_BETA}\n§\nSynthetic new fact.")
    );

    let s = Stores::new(PADDED, 100, 100);
    s.call(json!({"action": "add", "content": "Synthetic new fact."}));
    assert_eq!(
        s.file("MEMORY.md"),
        "Synthetic alpha fact.\n§\nSynthetic padded fact.\n§\nSynthetic beta fact.\n§\n\
         Synthetic new fact."
    );

    let raw = oversized();
    let s = Stores::new(&raw, 100, 100);
    let out = s.call(json!({"action": "add", "content": "Synthetic new fact."}));
    assert!(out.is_error);
    assert!(
        out.content.starts_with(
            "{\"success\": false, \"error\": \"Memory at 164/100 chars. Adding this entry (19 \
             chars) would exceed the limit."
        ),
        "{}",
        out.content
    );
    assert_eq!(s.file("MEMORY.md"), raw);
    assert_eq!(backups(&s), Vec::<String>::new());
}

#[test]
fn a_byte_order_mark_duplicates_line_breaks_and_outer_whitespace_still_look_tool_written() {
    let stores = [
        format!("\u{feff}{ALPHA_BETA}"),
        format!("Synthetic alpha fact.\n§\n{ALPHA_BETA}"),
        "Synthetic alpha fact.\r\n§\r\nSynthetic beta fact.".to_owned(),
        "Synthetic alpha fact.\r§\rSynthetic beta fact.".to_owned(),
        format!("  {ALPHA_BETA}  \n\n"),
    ];
    for raw in stores {
        let s = Stores::new(&raw, 100, 100);
        assert_eq!(
            s.answer(json!({"action": "remove", "old_text": "alpha"})),
            success("memory", "20% — 20/100 chars", 1, "Entry removed."),
            "{raw:?}"
        );
        assert_eq!(s.file("MEMORY.md"), "Synthetic beta fact.");
        assert_eq!(backups(&s), Vec::<String>::new());
    }
}

#[test]
fn a_snapshot_that_cannot_be_written_is_named_as_failed_and_the_store_is_kept() {
    let s = Stores::new(BLANK, 100, 100);
    fs::write(s.path("MEMORY.md.lock"), "").unwrap();
    fs::set_permissions(s.path(""), fs::Permissions::from_mode(0o500)).unwrap();
    let out = s.answer(json!({"action": "remove", "old_text": "alpha"}));
    fs::set_permissions(s.path(""), fs::Permissions::from_mode(0o700)).unwrap();
    let answer: Value = serde_json::from_str(&out).unwrap();
    let backup = answer["drift_backup"].as_str().unwrap();
    let prefix = format!("{}.bak.", s.path("MEMORY.md").display());
    let secs = backup
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(" (BACKUP FAILED — file unchanged on disk)"))
        .unwrap();
    assert!(secs.bytes().all(|b| b.is_ascii_digit()), "{backup}");
    assert_eq!(out, drifted("MEMORY.md", backup));
    assert_eq!(s.file("MEMORY.md"), BLANK);
    assert_eq!(backups(&s), Vec::<String>::new());
}
