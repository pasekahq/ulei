// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `write_file` and `patch`: UTF-8 text files, paths relative to the working directory or
//! under `~`.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::read::DEDUP_STATUS_MESSAGE;
use super::repr::repr;
use super::tracker::Shared;
use super::{
    Args, Context, Output, PATCH, Tool, WRITE_FILE, difflib, fuzzy, is_py_space, json_array,
    json_object, json_str, object_schema, py_splitlines, py_strip, realpath, resolve, type_name,
    v4a,
};

/// `write_file`; a write makes the next `read_file` of the path return its content again.
pub(super) struct WriteFile {
    pub(super) tracker: Shared,
}

/// `patch`; a replacement makes the next `read_file` of the path return its content again.
pub(super) struct Patch {
    pub(super) tracker: Shared,
    /// Consecutive not-found failures per resolved path, oldest path first.
    pub(super) failures: Mutex<Vec<(PathBuf, usize)>>,
}

/// Most paths whose consecutive `patch` failures are counted; the oldest is forgotten first.
const FAILURE_PATHS_CAP: usize = 64;

/// The UTF-8 byte order mark.
pub(super) const BOM: char = '\u{feff}';

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
        let written = write(&target, content, None);
        self.tracker.lock().written(&realpath(&target));
        let written = match written {
            Ok(written) => written,
            Err(e) => return Output::error(e),
        };
        let mut fields = vec![
            ("bytes_written", written.bytes.to_string()),
            ("dirs_created", written.dirs_created.to_string()),
        ];
        if written.verified {
            fields.push(("verified", "true".into()));
        }
        fields.push(("resolved_path", json_str(&written.shown)));
        fields.push(("files_modified", json_array([json_str(&written.shown)])));
        Output::ok(json_object(fields))
    }
}

impl Tool for Patch {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: PATCH.into(),
            description: "Targeted find-and-replace edits in files. Use this instead of sed/awk \
                          in terminal. Uses fuzzy matching (9 strategies) so minor \
                          whitespace/indentation differences won't break it. Returns a unified \
                          diff.\n\nREPLACE MODE (mode='replace', default): find a unique string \
                          and replace it. REQUIRED PARAMETERS: mode, path, old_string, \
                          new_string.\nPATCH MODE (mode='patch'): apply V4A multi-file patches \
                          for bulk changes. REQUIRED PARAMETERS: mode, patch."
                .into(),
            input_schema: object_schema(
                json!({
                    "mode": {
                        "type": "string",
                        "enum": ["replace", "patch"],
                        "description": "Edit mode. 'replace' (default): requires path + \
                                        old_string + new_string. 'patch': requires patch content \
                                        only.",
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
                    },
                    "patch": {
                        "type": "string",
                        "description": "REQUIRED when mode='patch'. V4A format patch content. \
                                        Format:\n*** Begin Patch\n*** Update File: \
                                        path/to/file\n@@ context hint @@\n context \
                                        line\n-removed line\n+added line\n*** End Patch"
                    }
                }),
                &["mode"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        match request(&Args(input)) {
            Ok(Request::Replace {
                path,
                old,
                new,
                replace_all,
            }) => self.replace(path, old, new, replace_all, ctx.cwd),
            Ok(Request::V4a { path, body }) => self.v4a(path, body, ctx.cwd),
            Err(e) => Output::error(e),
        }
    }
}

impl Patch {
    fn replace(&self, path: &str, old: &str, new: &str, replace_all: bool, cwd: &Path) -> Output {
        let resolved = realpath(&resolve(cwd, path));
        let shown = resolved.display().to_string();
        match patch_replace(&resolved, &shown, old, new, replace_all) {
            Ok(patched) => {
                self.tracker.lock().written(&resolved);
                self.failures().retain(|(p, _)| *p != resolved);
                let mut fields = vec![("success", "true".to_owned())];
                if let Some(note) = patched.note {
                    fields.push(("no_change", "true".into()));
                    fields.push(("note", json_str(&note)));
                }
                if !patched.diff.is_empty() {
                    fields.push(("diff", json_str(&patched.diff)));
                }
                fields.push(("files_modified", json_array([json_str(&shown)])));
                fields.push(("resolved_path", json_str(&shown)));
                Output::ok(json_object(fields))
            }
            Err(error) => {
                let mut fields = vec![("success", "false".to_owned()), ("error", json_str(&error))];
                if error.contains("Could not find") {
                    let failures = self.failure(resolved);
                    if let Some(hint) = not_found_hint(&error, Some((failures, path))) {
                        fields.push(("_hint", json_str(&hint)));
                    }
                }
                Output {
                    content: json_object(fields),
                    is_error: true,
                }
            }
        }
    }

    /// Applies the V4A patch `body` whose header paths resolve against `cwd`; `path`, when
    /// given, counts among the files it reports.
    fn v4a(&self, path: Option<&str>, body: &str, cwd: &Path) -> Output {
        let mut paths: Vec<&str> = path.into_iter().collect();
        for header in v4a::header_paths(body) {
            if header.split('/').any(|part| part == "..") {
                return Output::error(format!(
                    "V4A patch header contains '..' traversal: {}. Use the agent's cwd-relative \
                     path (no '..') or an absolute path in '*** Update File:' / '*** Add File:' / \
                     '*** Delete File:' / '*** Move File:' headers.",
                    repr(header)
                ));
            }
            paths.push(header);
        }
        let resolved_of = |raw: &str| realpath(&resolve(cwd, raw)).display().to_string();
        let result = v4a::patch_v4a(body, &resolved_of);
        let mut fields = result.fields();
        let Some(error) = result.error else {
            let resolved: Vec<String> = paths.into_iter().map(resolved_of).collect();
            fields.set(
                "files_modified",
                json_array(resolved.iter().map(|p| json_str(p))),
            );
            if let [only] = resolved.as_slice() {
                fields.set("resolved_path", json_str(only));
            }
            let mut tracker = self.tracker.lock();
            for path in &resolved {
                tracker.written(Path::new(path));
            }
            self.failures()
                .retain(|(p, _)| !resolved.iter().any(|r| Path::new(r) == p));
            return Output::ok(json_object(fields.0));
        };
        if error.contains("Could not find")
            && let Some(hint) = not_found_hint(&error, None)
        {
            fields.set("_hint", json_str(&hint));
        }
        Output {
            content: json_object(fields.0),
            is_error: true,
        }
    }

    /// Counts one more consecutive not-found failure on `resolved`: how many it has had.
    fn failure(&self, resolved: PathBuf) -> usize {
        let mut failures = self.failures();
        if let Some((_, n)) = failures.iter_mut().find(|(p, _)| *p == resolved) {
            *n += 1;
            return *n;
        }
        if failures.len() >= FAILURE_PATHS_CAP {
            failures.remove(0);
        }
        failures.push((resolved, 1));
        1
    }

    /// The failure counts; their updates never stop halfway, so a poisoned lock holds them whole.
    fn failures(&self) -> MutexGuard<'_, Vec<(PathBuf, usize)>> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The `_hint` of a not-found `error`, escalated when `repeat` holds the third or a later
/// consecutive failure on its path; none while the error shows the closest sections.
fn not_found_hint(error: &str, repeat: Option<(usize, &str)>) -> Option<String> {
    if let Some((failures, path)) = repeat.filter(|(failures, _)| *failures >= 3) {
        return Some(format!(
            "This is failure #{failures} patching {}. Stop retrying with variations of the same \
             old_string. Either: (1) re-read the file fresh to verify current content, (2) use a \
             longer / more unique old_string with surrounding context lines, or (3) use \
             write_file to replace the entire file if the targeted region is hard to anchor.",
            repr(path)
        ));
    }
    (!error.contains("Did you mean one of these sections?")).then(|| {
        "old_string not found. Use read_file to verify the current content, or search_files to \
         locate the text."
            .to_owned()
    })
}

/// The arguments of a `patch` call in its mode.
enum Request<'a> {
    Replace {
        path: &'a str,
        old: &'a str,
        new: &'a str,
        replace_all: bool,
    },
    V4a {
        path: Option<&'a str>,
        /// The `patch` argument.
        body: &'a str,
    },
}

fn request<'a>(args: &Args<'a>) -> Result<Request<'a>, String> {
    let v4a = match args.present("mode") {
        None => false,
        Some(Value::String(mode)) if mode == "replace" => false,
        Some(Value::String(mode)) if mode == "patch" => true,
        Some(Value::String(mode)) => return Err(format!("Unknown mode: {mode}")),
        Some(mode) => return Err(format!("Unknown mode: {mode}")),
    };
    let path = args.opt_str("path")?.filter(|path| !path.is_empty());
    if v4a {
        let body = args.opt_str("patch")?.filter(|body| !body.is_empty());
        let body = body.ok_or("patch content required")?;
        return Ok(Request::V4a { path, body });
    }
    let path = path.ok_or("path required")?;
    let (Some(old), Some(new)) = (args.opt_str("old_string")?, args.opt_str("new_string")?) else {
        return Err("old_string and new_string required".into());
    };
    let replace_all = args.opt_bool("replace_all")?.unwrap_or(false);
    Ok(Request::Replace {
        path,
        old,
        new,
        replace_all,
    })
}

/// A replacement `patch` made or found already made.
struct Patched {
    /// Unified diff of the change; empty when nothing changed.
    diff: String,
    /// Why nothing was written: the edit is already in the file.
    note: Option<String>,
}

/// Replaces `old` by `new` in the file at `resolved`, shown to the model as `shown`, keeping
/// its byte order mark and line endings, and checks what landed.
fn patch_replace(
    resolved: &Path,
    shown: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Patched, String> {
    let raw = fs::read(resolved)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| format!("Failed to read file: {shown}"))?;
    let content = raw.strip_prefix(BOM).unwrap_or(&raw);
    let mut new_content = match fuzzy::fuzzy_find_and_replace(content, old, new, replace_all) {
        Ok(new_content) => new_content,
        Err(error) => {
            if fuzzy::is_already_applied(content, old, new) {
                return Ok(Patched {
                    diff: String::new(),
                    note: Some(format!(
                        "File already contains the target text \u{2014} the edit appears to be \
                         already applied to {shown}. No write performed; do not re-send this \
                         patch."
                    )),
                });
            }
            let hint = fuzzy::format_no_match_hint(&error, old, content);
            return Err(error + &hint);
        }
    };
    if let Some(ending) = line_ending(content) {
        new_content = normalize_line_endings(&new_content, ending);
    }
    write(resolved, &new_content, Some(&raw))
        .map_err(|e| format!("Failed to write changes: {e}"))?;
    let landed = fs::read(resolved)
        .map_err(|_| format!("Post-write verification failed: could not re-read {shown}"))?;
    let landed = String::from_utf8_lossy(&landed);
    let landed = normalize_line_endings(landed.strip_prefix(BOM).unwrap_or(&landed), "\n");
    let intended = normalize_line_endings(&new_content, "\n");
    if landed != intended {
        return Err(format!(
            "Post-write verification failed for {shown}: on-disk content differs from intended \
             write (wrote {} chars, read back {} chars after normalizing line endings). The patch \
             did not persist. Re-read the file and try again.",
            intended.chars().count(),
            landed.chars().count()
        ));
    }
    let before: Vec<&str> = py_splitlines(content, true).collect();
    let after: Vec<&str> = py_splitlines(&new_content, true).collect();
    Ok(Patched {
        diff: difflib::unified_diff(
            &before,
            &after,
            &format!("a/{shown}"),
            &format!("b/{shown}"),
        ),
        note: None,
    })
}

/// `\r\n` when the first 4096 characters hold one, else `\n` when they hold one.
fn line_ending(text: &str) -> Option<&'static str> {
    let head = &text[..text
        .char_indices()
        .nth(4096)
        .map_or(text.len(), |(at, _)| at)];
    if head.contains("\r\n") {
        Some("\r\n")
    } else if head.contains('\n') {
        Some("\n")
    } else {
        None
    }
}

/// `text` with every `\r\n`, lone `\r` and `\n` turned into `ending`.
pub(crate) fn normalize_line_endings(text: &str, ending: &str) -> String {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    if ending == "\n" {
        lf
    } else {
        lf.replace('\n', ending)
    }
}

/// What [`write`] put on disk.
pub(super) struct Written {
    /// The absolute path of the file written.
    shown: String,
    bytes: usize,
    dirs_created: bool,
    /// The bytes read back equal the bytes written; `false` when they could not be read back.
    verified: bool,
}

/// Replaces the file behind `path` with `content` through a sibling temporary file that keeps
/// its mode, in CRLF and with a byte order mark when the file it replaces has them, and checks
/// the bytes that landed. `pre_content` is the text the caller read from that file.
pub(super) fn write(
    path: &Path,
    content: &str,
    pre_content: Option<&str>,
) -> Result<Written, String> {
    let absolute = fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .map_err(|e| format!("Failed to write file: {e}"))?;
    let ending = match pre_content.filter(|pre| !pre.is_empty()) {
        Some(pre) => line_ending(pre),
        None => head(&absolute, 4096).and_then(|h| line_ending(&String::from_utf8_lossy(&h))),
    };
    let mut content = Cow::Borrowed(content);
    if ending == Some("\r\n") {
        content = Cow::Owned(normalize_line_endings(&content, "\r\n"));
    }
    let has_bom = head(&absolute, 3).is_some_and(|h| h.starts_with("\u{feff}".as_bytes()));
    if has_bom && !content.starts_with(BOM) {
        content.to_mut().insert(0, BOM);
    }
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
    fs::write(&temp, &*content).map_err(failed)?;
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
    let verified = match fs::read(&absolute) {
        Ok(landed) if landed != content.as_bytes() => {
            return Err(format!(
                "Post-write verification failed for {shown}: on-disk content hash differs from \
                 the intended write. The write did not persist correctly \u{2014} re-read the file \
                 and retry."
            ));
        }
        Ok(_) => true,
        Err(_) => false,
    };
    Ok(Written {
        shown,
        bytes: content.len(),
        dirs_created: parent.is_some(),
        verified,
    })
}

/// The first `n` bytes of the file at `path`; `None` when it cannot be read.
fn head(path: &Path, n: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(n).read_to_end(&mut bytes))
        .ok()?;
    Some(bytes)
}

/// `content` is the dedup answer of `read_file` or mostly its `LINE_NUM|CONTENT` lines, not
/// the bytes the model means to write.
fn is_read_display_text(content: &str) -> bool {
    let stripped = py_strip(content);
    if !stripped.is_empty()
        && (stripped == DEDUP_STATUS_MESSAGE
            || (stripped.contains(DEDUP_STATUS_MESSAGE)
                && stripped.chars().count() <= 2 * DEDUP_STATUS_MESSAGE.chars().count()))
    {
        return true;
    }
    let lines: Vec<&str> = py_splitlines(content, false)
        .filter(|line| !py_strip(line).is_empty())
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
