// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `memory` tool: `add`, `replace` and `remove` on the MEMORY and USER stores, one call at a
//! time or as an all-or-nothing batch, each store bounded by its char limit as a whole. A write
//! rereads the file under its lock and replaces it atomically; a `replace`, `remove` or batch
//! leaves a file that does not look tool-written as it is and backs it up. The prompt of a
//! running session keeps the snapshot it was built with.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{
    Context, MEMORY, Output, Tool, bound, create_exclusive, failure, file_name, json_array,
    json_ascii, json_object, json_str, object_schema, os_error_text, py_str, py_strip,
    python_os_error, random_hex, realpath, sanitize_tool_error, thousands, truthy, type_name,
};
use crate::prompt::Brain;
use crate::prompt::memory::{ENTRY_DELIMITER, Target, char_count, entries, parse, raw, usage_pct};

/// Failed consolidation attempts of one turn after which a failure tells the model to stop.
const MAX_CONSOLIDATION_FAILURES_PER_TURN: u32 = 3;

const DESCRIPTION: &str = "Save durable facts to persistent memory that survive across \
    sessions. Memory is injected into every future turn, so keep entries compact and \
    high-signal.\n\nHOW: make ALL your changes in ONE call via an 'operations' array (each \
    item: {action, content?, old_text?}). The batch applies atomically and the char limit is \
    checked only on the FINAL result — so a single call can remove/replace stale entries to \
    free room AND add new ones, even when an add alone would overflow. The response reports \
    current/limit chars and confirms completion; one batch call finishes the update, so don't \
    repeat it. Use the bare action/content/old_text fields only for a single lone change.\n\n\
    WHEN: save proactively when the user states a preference, correction, or personal detail, \
    or you learn a stable fact about their environment, conventions, or workflow. Priority: \
    user preferences & corrections > environment facts > procedures. The best memory stops \
    the user repeating themselves.\n\nIF FULL: an add is rejected with the current entries \
    shown. Reissue as ONE batch that removes or shortens enough stale entries and adds the new \
    one together.\n\nTARGETS: 'user' = who the user is (name, role, preferences, style). \
    'memory' = your notes (environment, conventions, tool quirks, lessons).\n\nSKIP: \
    trivial/obvious info, easily re-discovered facts, raw data dumps, task progress, \
    completed-work logs, temporary TODO state (use session_search for those). Reusable \
    procedures belong in a skill, not memory.";

/// The MEMORY and USER stores; the registry lives for one turn, so the failure count is the
/// turn's.
pub struct Memory {
    notes: Store,
    profile: Store,
    failures: AtomicU32,
}

struct Store {
    path: PathBuf,
    limit: usize,
}

/// What a write makes of the entries it reread: entries to save, or entries left as they are.
enum Change {
    Save(Vec<String>, String),
    Keep(Vec<String>, &'static str),
}

impl Memory {
    /// The stores at the paths and char limits of `brain`.
    pub fn new(brain: &Brain) -> Self {
        Self {
            notes: Store {
                path: brain.memory.clone(),
                limit: brain.memory_char_limit,
            },
            profile: Store {
                path: brain.user.clone(),
                limit: brain.user_char_limit,
            },
            failures: AtomicU32::new(0),
        }
    }

    fn store(&self, target: Target) -> &Store {
        match target {
            Target::Memory => &self.notes,
            Target::User => &self.profile,
        }
    }

    /// Runs `change` on the entries of `target` reread under its lock and saves what it yields.
    /// With `guarded`, a file that [`drift`] finds not tool-written is refused instead.
    fn write(
        &self,
        target: Target,
        guarded: bool,
        change: impl FnOnce(Vec<String>) -> Result<Change, Output>,
    ) -> Output {
        let store = self.store(target);
        let path = &store.path;
        let _lock = match lock(path) {
            Ok(lock) => lock,
            Err(out) => return out,
        };
        let Ok(raw) = raw(path) else {
            return failure([("error", json_str(&read_failed(path)))]);
        };
        if guarded && let Some(backup) = drift(store, &raw) {
            return drift_error(path, &backup);
        }
        match change(parse(&raw)) {
            Ok(Change::Keep(entries, message)) => self.success(target, &entries, message),
            Ok(Change::Save(entries, message)) => match save(path, &entries) {
                Ok(()) => self.success(target, &entries, &message),
                Err(out) => out,
            },
            Err(out) => out,
        }
    }

    fn add(&self, target: Target, content: &str) -> Output {
        let content = py_strip(content);
        if content.is_empty() {
            return failure([("error", json_str("Content cannot be empty."))]);
        }
        let limit = self.store(target).limit;
        self.write(target, false, |mut entries| {
            if entries.iter().any(|e| e == content) {
                return Ok(Change::Keep(
                    entries,
                    "Entry already exists (no duplicate added).",
                ));
            }
            let current = char_count(&entries);
            entries.push(content.to_owned());
            if char_count(&entries) > limit {
                entries.pop();
                return Err(self.consolidation(over_limit(
                    &format!(
                        "Memory at {}/{} chars. Adding this entry ({} chars) would exceed the \
                         limit. Consolidate now: use 'replace' to merge overlapping entries \
                         into shorter ones or 'remove' stale or less important entries (see \
                         current_entries below), then retry this add — all in this turn.",
                        thousands(current),
                        thousands(limit),
                        content.chars().count()
                    ),
                    &entries,
                    limit,
                )));
            }
            Ok(Change::Save(entries, "Entry added.".into()))
        })
    }

    /// `replace` with `new_content`, or `remove` without it, of the entry holding `old_text`.
    fn edit(&self, target: Target, old_text: &str, new_content: Option<&str>) -> Output {
        let old_text = py_strip(old_text);
        let new_content = new_content.map(py_strip);
        if old_text.is_empty() {
            return failure([("error", json_str("old_text cannot be empty."))]);
        }
        if new_content == Some("") {
            return failure([(
                "error",
                json_str("new_content cannot be empty. Use 'remove' to delete entries."),
            )]);
        }
        let (verb, done) = match new_content {
            Some(_) => ("replace", "Entry replaced."),
            None => ("remove", "Entry removed."),
        };
        let limit = self.store(target).limit;
        self.write(target, true, |mut entries| {
            let matches = matching(&entries, old_text);
            let Some(&at) = matches.first() else {
                return Err(self.consolidation(failure([
                    (
                        "error",
                        json_str(&format!(
                            "No entry matched '{old_text}'. Check current_entries below and \
                             retry with the exact text of the entry you want to {verb}."
                        )),
                    ),
                    ("current_entries", strings(&entries)),
                ])));
            };
            if !same_entry(&entries, &matches) {
                let previews = matches.iter().map(|&i| json_str(&preview(&entries[i])));
                return Err(failure([
                    (
                        "error",
                        json_str(&format!(
                            "Multiple entries matched '{old_text}'. Be more specific."
                        )),
                    ),
                    ("matches", json_array(previews)),
                ]));
            }
            let Some(new_content) = new_content else {
                entries.remove(at);
                return Ok(Change::Save(entries, done.into()));
            };
            let mut replaced = entries.clone();
            new_content.clone_into(&mut replaced[at]);
            let new_total = char_count(&replaced);
            if new_total > limit {
                return Err(self.consolidation(over_limit(
                    &format!(
                        "Replacement would put memory at {}/{} chars. Shorten the new content, \
                         or 'remove' other stale or less important entries to make room (see \
                         current_entries below), then retry — all in this turn.",
                        thousands(new_total),
                        thousands(limit)
                    ),
                    &entries,
                    limit,
                )));
            }
            Ok(Change::Save(replaced, done.into()))
        })
    }

    /// Applies `operations` in order to a copy of the entries and saves it only when every
    /// operation applies and the result fits the limit.
    fn batch(&self, target: Target, operations: &[Value]) -> Output {
        let limit = self.store(target).limit;
        self.write(target, true, |entries| {
            let fail = |message: String| {
                self.consolidation(over_limit(
                    &format!("{message} No operations were applied (batch is all-or-nothing)."),
                    &entries,
                    limit,
                ))
            };
            let mut working = entries.clone();
            for (i, op) in operations.iter().enumerate() {
                let (action, content, old_text) = operation(op)?;
                let content = py_strip(content);
                let old_text = py_strip(old_text);
                let pos = format!(
                    "Operation {} ({})",
                    i + 1,
                    action
                        .filter(|a| truthy(a))
                        .map_or("unknown".into(), py_str)
                );
                let action = action.and_then(Value::as_str);
                if action == Some("add") {
                    if content.is_empty() {
                        return Err(fail(format!("{pos}: content is required.")));
                    }
                    if !working.iter().any(|e| e == content) {
                        working.push(content.to_owned());
                    }
                    continue;
                }
                if !matches!(action, Some("replace" | "remove")) {
                    return Err(fail(format!(
                        "{pos}: unknown action. Use add, replace, or remove."
                    )));
                }
                if old_text.is_empty() {
                    return Err(fail(format!("{pos}: old_text is required.")));
                }
                if action == Some("replace") && content.is_empty() {
                    return Err(fail(format!(
                        "{pos}: content is required (use action='remove' to delete)."
                    )));
                }
                let matches = matching(&working, old_text);
                let Some(&at) = matches.first() else {
                    return Err(fail(format!("{pos}: no entry matched '{old_text}'.")));
                };
                if !same_entry(&working, &matches) {
                    return Err(fail(format!(
                        "{pos}: '{old_text}' matched multiple distinct entries -- be more \
                         specific."
                    )));
                }
                if action == Some("replace") {
                    content.clone_into(&mut working[at]);
                } else {
                    working.remove(at);
                }
            }
            let new_total = char_count(&working);
            if new_total > limit {
                return Err(self.consolidation(over_limit(
                    &format!(
                        "After applying all {} operations, memory would be at {}/{} chars -- \
                         over the limit. Remove or shorten more entries in the same batch (see \
                         current_entries below), then retry.",
                        operations.len(),
                        thousands(new_total),
                        thousands(limit)
                    ),
                    &entries,
                    limit,
                )));
            }
            Ok(Change::Save(
                working,
                format!("Applied {} operation(s).", operations.len()),
            ))
        })
    }

    /// The answer to a `replace` or `remove` without `old_text`: the entries to pick from.
    fn missing_old_text(&self, target: Target, action: &str) -> Output {
        let store = self.store(target);
        let Ok(entries) = entries(&store.path) else {
            return failure([("error", json_str(&read_failed(&store.path)))]);
        };
        failure([
            (
                "error",
                json_str(&format!(
                    "'{action}' needs old_text -- a short unique substring of the entry to \
                     {action}. None was provided. Reissue the {action} with old_text set to \
                     part of one of the current_entries below."
                )),
            ),
            ("current_entries", strings(&entries)),
            ("usage", json_str(&usage(char_count(&entries), store.limit))),
        ])
    }

    /// The terminal answer to a landed write; it ends the turn's run of failures.
    fn success(&self, target: Target, entries: &[String], message: &str) -> Output {
        self.failures.store(0, Ordering::Relaxed);
        let limit = self.store(target).limit;
        let current = char_count(entries);
        Output::ok(json_object([
            ("success", "true".into()),
            ("done", "true".into()),
            ("target", json_str(target.name())),
            (
                "usage",
                json_str(&format!(
                    "{}% — {} chars",
                    usage_pct(current, limit),
                    usage(current, limit)
                )),
            ),
            ("entry_count", entries.len().to_string()),
            ("message", json_str(message)),
            (
                "note",
                json_str("Write saved. This update is complete — do not repeat it."),
            ),
        ]))
    }

    /// `out` while the turn's failed consolidations stay within the cap, then an answer that
    /// tells the model to stop retrying.
    fn consolidation(&self, out: Output) -> Output {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures <= MAX_CONSOLIDATION_FAILURES_PER_TURN {
            return out;
        }
        Output {
            content: json_object([
                ("success", "false".into()),
                ("done", "true".into()),
                (
                    "error",
                    json_str(&format!(
                        "Memory consolidation failed {failures} times this turn. Stop retrying \
                         memory calls — leave memory unchanged for now and continue with your \
                         reply to the user. The fact can be saved in a later turn."
                    )),
                ),
            ]),
            is_error: true,
        }
    }
}

impl Tool for Memory {
    fn definition(&self) -> c4g::Tool {
        let op = json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                "content": {
                    "type": "string",
                    "description": "Entry content for add/replace. Alias: 'new_text'."
                },
                "new_text": {"type": "string", "description": "Alias for 'content' in a batch op."},
                "old_text": {
                    "type": "string",
                    "description": "Substring identifying the entry for replace/remove."
                }
            },
            "required": ["action"]
        });
        c4g::Tool {
            name: MEMORY.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["add", "replace", "remove"],
                        "description": "The action to perform (single-op shape). Omit when \
                                        using 'operations'."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["memory", "user"],
                        "description": "Which memory store: 'memory' for personal notes, 'user' \
                                        for user profile."
                    },
                    "content": {
                        "type": "string",
                        "description": "The entry content. Required for 'add' and 'replace' \
                                        (single-op shape). Alias: 'new_text' is also accepted \
                                        (mirrors old_text)."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "REQUIRED for 'replace' and 'remove' (single-op shape): a \
                                        short unique substring identifying the existing entry \
                                        to modify. Omit only for 'add'."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Alias for 'content' (single-op shape). Provided so the \
                                        replace/remove old_text/new_text pairing works; if both \
                                        are set, 'content' wins."
                    },
                    "operations": {
                        "type": "array",
                        "description": "Batch shape: a list of operations applied atomically in \
                                        one call against the final char budget. Preferred when \
                                        making multiple changes or consolidating to make room. \
                                        Each item is {action, content?, old_text?}.",
                        "items": op
                    }
                }),
                &["target"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        let field = |name: &str| input.get(name).filter(|v| !v.is_null());
        let content = field("content").or_else(|| field("new_text"));
        let target = match field("target") {
            None => Target::Memory,
            Some(Value::String(t)) if t == "memory" => Target::Memory,
            Some(Value::String(t)) if t == "user" => Target::User,
            Some(other) => {
                let error = bound(format!(
                    "Invalid memory target '{}'. Use 'memory' or 'user'.",
                    py_str(other)
                ));
                return failure([("error", json_ascii(&error))]);
            }
        };
        if let Some(operations) = input.get("operations").filter(|ops| truthy(ops)) {
            let Value::Array(operations) = operations else {
                return Output::error_with(
                    "operations must be a list of {action, content?, old_text?} objects.",
                    [("success", "false".into())],
                );
            };
            return self.batch(target, operations);
        }
        let old_text = field("old_text").filter(|v| truthy(v));
        let content = content.filter(|v| truthy(v));
        let result = match input.get("action").and_then(Value::as_str) {
            Some("add") => match content {
                None => Err(required("Content is required for 'add' action.")),
                Some(content) => text(content).map(|content| self.add(target, content)),
            },
            Some(action @ ("replace" | "remove")) => match (old_text, content, action) {
                (None, _, _) => Ok(self.missing_old_text(target, action)),
                (Some(_), None, "replace") => {
                    Err(required("content is required for 'replace' action."))
                }
                (Some(old_text), Some(content), "replace") => {
                    text(old_text).and_then(|old| Ok(self.edit(target, old, Some(text(content)?))))
                }
                (Some(old_text), _, _) => text(old_text).map(|old| self.edit(target, old, None)),
            },
            _ => Err(required(&format!(
                "Unknown action '{}'. Use: add, replace, remove",
                input.get("action").map_or_else(String::new, py_str)
            ))),
        };
        result.unwrap_or_else(|out| out)
    }
}

/// The `action`, the content (`content`, else `new_text`, else empty) and the `old_text` of a
/// batch operation; a falsy operation has none of them.
fn operation(op: &Value) -> Result<(Option<&Value>, &str, &str), Output> {
    let op = match op {
        Value::Object(op) => op,
        other if truthy(other) => {
            return Err(Output::exception(
                "AttributeError",
                format!("'{}' object has no attribute 'get'", type_name(Some(other))),
            ));
        }
        _ => return Ok((None, "", "")),
    };
    let first_truthy = |names: &[&str]| -> Result<&str, Output> {
        match names.iter().find_map(|n| op.get(*n).filter(|v| truthy(v))) {
            None => Ok(""),
            Some(Value::String(text)) => Ok(text),
            Some(other) => Err(no_strip(other)),
        }
    };
    let content = first_truthy(&["content", "new_text"])?;
    let old_text = first_truthy(&["old_text"])?;
    Ok((op.get("action"), content, old_text))
}

/// The text of a field that the reference strips.
fn text(value: &Value) -> Result<&str, Output> {
    match value {
        Value::String(text) => Ok(text),
        other => Err(no_strip(other)),
    }
}

/// The error of a field that should hold text and holds another JSON value.
fn no_strip(value: &Value) -> Output {
    Output::exception(
        "AttributeError",
        format!(
            "'{}' object has no attribute 'strip'",
            type_name(Some(value))
        ),
    )
}

fn required(message: &str) -> Output {
    Output::error_with(message, [("success", "false".into())])
}

/// A failure carrying `error`, the reread `entries` and their usage of `limit`.
fn over_limit(error: &str, entries: &[String], limit: usize) -> Output {
    failure([
        ("error", json_str(error)),
        ("current_entries", strings(entries)),
        ("usage", json_str(&usage(char_count(entries), limit))),
    ])
}

/// `current/limit` with thousands separators.
fn usage(current: usize, limit: usize) -> String {
    format!("{}/{}", thousands(current), thousands(limit))
}

fn strings(entries: &[String]) -> String {
    json_array(entries.iter().map(|e| json_str(e)))
}

/// Indices of the entries that hold `old_text`.
fn matching(entries: &[String], old_text: &str) -> Vec<usize> {
    (0..entries.len())
        .filter(|&i| entries[i].contains(old_text))
        .collect()
}

/// All `matches` are the same text, so acting on the first is safe.
fn same_entry(entries: &[String], matches: &[usize]) -> bool {
    matches.iter().all(|&i| entries[i] == entries[matches[0]])
}

/// The first 80 chars of `entry`, followed by `...` when it is longer.
fn preview(entry: &str) -> String {
    match entry.char_indices().nth(80) {
        Some((cut, _)) => format!("{}...", &entry[..cut]),
        None => entry.to_owned(),
    }
}

fn read_failed(path: &Path) -> String {
    format!(
        "Refusing to write {}: the file exists on disk but could not be read right now \
         (temporarily locked by another program, a permission change, invalid/corrupt text \
         encoding, or a filesystem error). Treating an unreadable file as empty and saving would \
         wipe existing memory, so the write is refused. Nothing was changed — retry in a moment.",
        file_name(path)
    )
}

/// The backup path of `raw`, the text of `store`, when it does not look tool-written: its
/// stripped entries joined do not give it back stripped, or one entry is longer than the store's
/// limit. The copy `<file>.bak.<unix seconds>` is written beside the store; a failed copy gives
/// the path with a note that it failed.
fn drift(store: &Store, raw: &str) -> Option<String> {
    let text = py_strip(raw);
    if text.is_empty() {
        return None;
    }
    let parsed: Vec<&str> = raw
        .split(ENTRY_DELIMITER)
        .map(py_strip)
        .filter(|e| !e.is_empty())
        .collect();
    if text == parsed.join(ENTRY_DELIMITER)
        && parsed.iter().all(|e| e.chars().count() <= store.limit)
    {
        return None;
    }
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs().cast_signed(),
        Err(before) => -before.duration().as_secs().cast_signed(),
    };
    let mut name = store.path.as_os_str().to_owned();
    name.push(format!(".bak.{secs}"));
    let backup = PathBuf::from(name);
    let copied = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&backup)
        .and_then(|mut file| file.write_all(raw.as_bytes()));
    let shown = backup.display().to_string();
    Some(match copied {
        Ok(()) => shown,
        Err(_) => format!("{shown} (BACKUP FAILED — file unchanged on disk)"),
    })
}

/// The refusal of a write to the store at `path` that [`drift`] backed up to `backup`.
fn drift_error(path: &Path, backup: &str) -> Output {
    failure([
        (
            "error",
            json_str(&format!(
                "Refusing to write {}: file on disk has content that wouldn't round-trip through \
                 the memory tool (likely added by the patch tool, a shell append, a manual edit, \
                 or a concurrent session). A snapshot was saved to {backup}. Resolve the drift \
                 first — either rewrite the file as a clean §-delimited list of entries, or move \
                 the extra content out — then retry. This guard exists to prevent silent data \
                 loss (issue #26045).",
                file_name(path)
            )),
        ),
        ("drift_backup", json_str(backup)),
        (
            "remediation",
            json_str(
                "Open the .bak file, integrate the missing entries into the memory tool one at a \
                 time via memory(action=add, content=...), then remove or rewrite the original \
                 file to a clean state.",
            ),
        ),
    ])
}

/// An exclusive lock on the sibling `<name>.lock` of `path`, held until the file is dropped, so
/// the store itself can be replaced by a rename.
fn lock(path: &Path) -> Result<File, Output> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock_path = PathBuf::from(name);
    let taken = lock_path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            File::options()
                .read(true)
                .append(true)
                .create(true)
                .open(&lock_path)
        })
        .and_then(|file| {
            flock(&file, FlockOperation::LockExclusive)?;
            Ok(file)
        });
    taken.map_err(|e| {
        Output::error(sanitize_tool_error(&format!(
            "Tool execution failed: {}",
            python_os_error(&e, Some(&lock_path))
        )))
    })
}

/// Replaces the store at `path` with `entries`.
fn save(path: &Path, entries: &[String]) -> Result<(), Output> {
    write_atomic(path, &entries.join(ENTRY_DELIMITER)).map_err(|e| {
        Output::exception(
            "RuntimeError",
            format!(
                "Failed to write memory file {}: {}",
                path.display(),
                os_error_text(&e, None)
            ),
        )
    })
}

/// Writes `text` to a synced temporary file of mode 0600 beside the file `path` names and renames
/// it over that file; a link at `path` stays and its target is replaced.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let is_link = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    let real = if is_link {
        realpath(path)
    } else {
        path.to_owned()
    };
    let dir = real.parent().unwrap_or(Path::new(""));
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".mem_{}.tmp", random_hex(4)?));
    let written = create_exclusive(&temp, 0o600).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &real)
    });
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}
