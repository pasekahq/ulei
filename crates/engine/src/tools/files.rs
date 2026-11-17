// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `write_file` and `patch`: UTF-8 text files, paths relative to the working directory or
//! under `~`.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::read::DEDUP_STATUS_MESSAGE;
use super::tracker::Tracker;
use super::{
    Args, Context, Output, PATCH, Tool, WRITE_FILE, is_py_space, json_array, json_object, json_str,
    object_schema, realpath, resolve, type_name,
};

/// `write_file`; a write makes the next `read_file` of the path return its content again.
pub(super) struct WriteFile {
    pub(super) tracker: Rc<RefCell<Tracker>>,
}

/// `patch`; a replacement makes the next `read_file` of the path return its content again.
pub(super) struct Patch {
    pub(super) tracker: Rc<RefCell<Tracker>>,
}

impl Tool for WriteFile {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: WRITE_FILE.into(),
            description: "Write content to a file, completely replacing existing content. Use \
                          this instead of echo/cat heredoc in terminal. Creates parent \
                          directories automatically. OVERWRITES the entire file \u{2014} use \
                          'patch' for targeted edits. The result's verified:true means the \
                          on-disk content hash was confirmed \u{2014} do NOT re-read the file to \
                          check the write landed."
                .into(),
            input_schema: object_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (will be created if it doesn't \
                                        exist, overwritten if it does)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete content to write to the file"
                    }
                }),
                &["path", "content"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let Some(path) = input
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return Output::error(
                "write_file: missing required field 'path'. Re-emit the tool call with both \
                 'path' and 'content' set.",
            );
        };
        let Some(content) = input.get("content") else {
            return Output::error(
                "write_file: missing required field 'content'. The tool call included a path but \
                 no content argument \u{2014} this is almost always a dropped-arg bug under \
                 context pressure. Re-emit the tool call with the full content payload.",
            );
        };
        let Some(content) = content.as_str() else {
            return Output::error(format!(
                "write_file: 'content' must be a string, got {}.",
                type_name(Some(content))
            ));
        };
        if is_read_display_text(content) {
            return Output::error(
                "Refusing to write internal read_file display text as file content. Strip \
                 read_file line-number prefixes or reconstruct the intended file contents \
                 before writing.",
            );
        }
        let target = resolve(ctx.cwd, path);
        let written = write(&target, content);
        self.tracker.borrow_mut().written(&realpath(&target));
        match written {
            Ok(text) => Output::ok(text),
            Err(e) => Output::error(e),
        }
    }
}

impl Tool for Patch {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: PATCH.into(),
            description:
                "Targeted find-and-replace edits in files. Use this instead of sed/awk in \
                          terminal.\n\nREPLACE MODE (mode='replace', default): find a unique \
                          string and replace it. REQUIRED PARAMETERS: mode, path, old_string, \
                          new_string."
                    .into(),
            input_schema: object_schema(
                json!({
                    "mode": {
                        "type": "string",
                        "enum": ["replace"],
                        "description": "Edit mode. 'replace' (default): requires path + \
                                        old_string + new_string.",
                        "default": "replace"
                    },
                    "path": {
                        "type": "string",
                        "description": "REQUIRED when mode='replace'. File path to edit."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "REQUIRED when mode='replace'. Exact text to find and \
                                        replace. Must be unique in the file unless \
                                        replace_all=true. Include surrounding context lines to \
                                        ensure uniqueness."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "REQUIRED when mode='replace'. Changed replacement text; \
                                        it must differ from old_string. Pass empty string '' to \
                                        delete the matched text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences instead of requiring a unique \
                                        match (default: false)",
                        "default": false
                    }
                }),
                &["mode"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        match replace(&Args(input), ctx.cwd) {
            Ok((text, path)) => {
                self.tracker.borrow_mut().written(&realpath(&path));
                Output::ok(text)
            }
            Err(e) => Output::error(e),
        }
    }
}

/// A `patch` call in replace mode: its result text and the path it changed.
fn replace(args: &Args<'_>, cwd: &Path) -> Result<(String, PathBuf), String> {
    match args.present("mode") {
        None => {}
        Some(Value::String(mode)) if mode == "replace" => {}
        Some(Value::String(mode)) => return Err(format!("Unknown mode: {mode}")),
        Some(mode) => return Err(format!("Unknown mode: {mode}")),
    }
    let path = match args.opt_str("path")? {
        Some(path) if !path.is_empty() => resolve(cwd, path),
        _ => return Err("path required".into()),
    };
    let (Some(old), Some(new)) = (args.opt_str("old_string")?, args.opt_str("new_string")?) else {
        return Err("old_string and new_string required".into());
    };
    let replace_all = args.opt_bool("replace_all")?.unwrap_or(false);
    let text = edit(&path, old, new, replace_all)?;
    Ok((text, path))
}

/// Replaces the file behind `path` through a sibling temporary file that keeps its mode, and
/// checks the bytes that landed.
fn write(path: &Path, content: &str) -> Result<String, String> {
    let absolute = fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .map_err(|e| format!("Failed to write file: {e}"))?;
    let parent = absolute.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to write file: {}: {e}", parent.display()))?;
    }
    let name = absolute
        .file_name()
        .ok_or_else(|| format!("Failed to write file: {} names no file", absolute.display()))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".tmp.{}", std::process::id()));
    let temp = absolute.with_file_name(temp_name);
    let failed = |e: std::io::Error| format!("Failed to write file: {e}");
    fs::write(&temp, content).map_err(failed)?;
    let kept_mode =
        fs::metadata(&absolute).and_then(|meta| fs::set_permissions(&temp, meta.permissions()));
    if let Err(e) = kept_mode
        .or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
        .and_then(|()| fs::rename(&temp, &absolute))
    {
        let _ = fs::remove_file(&temp);
        return Err(failed(e));
    }
    let shown = absolute.display().to_string();
    let mut fields = vec![
        ("bytes_written", content.len().to_string()),
        ("dirs_created", parent.is_some().to_string()),
    ];
    match fs::read(&absolute) {
        Ok(landed) if landed != content.as_bytes() => {
            return Err(format!(
                "Post-write verification failed for {shown}: on-disk content hash differs from \
                 the intended write. The write did not persist correctly \u{2014} re-read the file \
                 and retry."
            ));
        }
        Ok(_) => fields.push(("verified", "true".into())),
        Err(_) => {}
    }
    fields.push(("resolved_path", json_str(&shown)));
    fields.push(("files_modified", json_array([json_str(&shown)])));
    Ok(json_object(fields))
}

/// `content` is the dedup answer of `read_file` or mostly its `LINE_NUM|CONTENT` lines, not
/// the bytes the model means to write.
fn is_read_display_text(content: &str) -> bool {
    let stripped = content.trim_matches(is_py_space);
    if !stripped.is_empty()
        && (stripped == DEDUP_STATUS_MESSAGE
            || (stripped.contains(DEDUP_STATUS_MESSAGE)
                && stripped.chars().count() <= 2 * DEDUP_STATUS_MESSAGE.chars().count()))
    {
        return true;
    }
    let lines: Vec<&str> = py_splitlines(content)
        .filter(|line| !line.trim_matches(is_py_space).is_empty())
        .collect();
    if lines.len() < 2 {
        return false;
    }
    let numbered: Vec<u64> = lines
        .iter()
        .filter_map(|line| {
            let (prefix, _) = line.trim_start_matches(is_py_space).split_once('|')?;
            if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(prefix.parse().unwrap_or(u64::MAX))
        })
        .collect();
    numbered.len() >= 2
        && numbered.len() * 5 >= lines.len() * 3
        && numbered
            .windows(2)
            .all(|w| w[0].checked_add(1) == Some(w[1]))
}

/// Lines as Python's `str.splitlines` breaks them.
fn py_splitlines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let breaks = |c: char| {
            matches!(
                c,
                '\n' | '\r'
                    | '\u{b}'
                    | '\u{c}'
                    | '\u{1c}'
                    | '\u{1d}'
                    | '\u{1e}'
                    | '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
        };
        let Some(at) = rest.find(breaks) else {
            return Some(std::mem::take(&mut rest));
        };
        let line = &rest[..at];
        let skip = if rest[at..].starts_with("\r\n") {
            2
        } else {
            rest[at..].chars().next().map_or(1, char::len_utf8)
        };
        rest = &rest[at + skip..];
        Some(line)
    })
}

fn edit(path: &Path, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("field `old_string` is empty".into());
    }
    if old == new {
        return Err("`old_string` and `new_string` are the same".into());
    }
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    let count = text.matches(old).count();
    if count == 0 {
        return Err(format!("`old_string` not found in {}", path.display()));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "`old_string` occurs {count} times in {}; make it unique or set `replace_all`",
            path.display()
        ));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    fs::write(path, updated).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!(
        "replaced {count} occurrence(s) in {}",
        path.display()
    ))
}
