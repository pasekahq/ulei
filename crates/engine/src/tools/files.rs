// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `read_file`, `write_file` and `patch`: UTF-8 text files, paths relative to the working
//! directory or under `~`.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::redact::{Flags, Redactor};
use super::{
    Args, Context, Output, PATCH, READ_FILE, Tool, WRITE_FILE, json_array, json_object, json_str,
    object_schema, resolve, type_name,
};

/// Most bytes of file text one `read_file` call returns.
pub const MAX_READ: usize = 128 * 1024;

/// `read_file`; `redactor` masks secrets in the text it returns.
pub(super) struct ReadFile {
    pub(super) redactor: Redactor,
}
pub(super) struct WriteFile;
pub(super) struct Patch;

impl Tool for ReadFile {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: READ_FILE.into(),
            description: "Read a text file with pagination. Use this instead of cat/head/tail in \
                          terminal. Use offset and limit for large files. NOTE: Cannot read \
                          images or other binary files."
                .into(),
            input_schema: object_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (absolute, relative, or ~/path)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed, default: 1)",
                        "default": 1,
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                }),
                &["path"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let args = Args(input);
        let result = path_arg(&args, ctx.cwd).and_then(|path| {
            let offset = positive(args.opt_u64("offset")?, "offset")?.unwrap_or(1);
            let limit = positive(args.opt_u64("limit")?, "limit")?;
            read(&path, offset, limit)
        });
        match result {
            Ok((text, cut)) => {
                let file_read = Flags {
                    file_read: true,
                    ..Flags::default()
                };
                let mut text = self.redactor.sensitive_text(&text, file_read).into_owned();
                if let Some(line) = cut {
                    let _ = write!(
                        text,
                        "\n[truncated at {MAX_READ} bytes; continue with offset {line}]"
                    );
                }
                Output::ok(text)
            }
            Err(e) => Output::error(e),
        }
    }
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
        match write(&resolve(ctx.cwd, path), content) {
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
            Ok(text) => Output::ok(text),
            Err(e) => Output::error(e),
        }
    }
}

/// A `patch` call in replace mode.
fn replace(args: &Args<'_>, cwd: &Path) -> Result<String, String> {
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
    edit(&path, old, new, replace_all)
}

/// The resolved `path` field of a call.
fn path_arg(args: &Args<'_>, cwd: &Path) -> Result<PathBuf, String> {
    let path = args.str("path")?;
    if path.is_empty() {
        return Err("field `path` is empty".into());
    }
    Ok(resolve(cwd, path))
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

fn positive(value: Option<u64>, name: &str) -> Result<Option<usize>, String> {
    value
        .map(|v| match usize::try_from(v) {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("field `{name}` must be a positive integer")),
        })
        .transpose()
}

/// Lines of the file from `offset`, at most `limit` of them and [`MAX_READ`] bytes, and the
/// number of the line cut at that bound.
fn read(
    path: &Path,
    offset: usize,
    limit: Option<usize>,
) -> Result<(String, Option<usize>), String> {
    let failed = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let not_text = |line: usize| format!("{} is not UTF-8 text (line {line})", path.display());
    let mut reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut number = 0;
    while number + 1 < offset {
        if reader.skip_until(b'\n').map_err(failed)? == 0 {
            return Ok((String::new(), None));
        }
        number += 1;
    }
    let mut text = String::new();
    let mut line = Vec::new();
    while limit.is_none_or(|limit| number + 1 - offset < limit) {
        let budget = MAX_READ - text.len();
        line.clear();
        let n = (&mut reader)
            .take(budget as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(failed)?;
        if n == 0 {
            break;
        }
        number += 1;
        if line.len() > budget {
            let kept = &line[..budget];
            let valid = match std::str::from_utf8(kept) {
                Ok(valid) => valid,
                // A character split by the cap is not an encoding error.
                Err(e) if e.error_len().is_none() => {
                    std::str::from_utf8(&kept[..e.valid_up_to()]).map_err(|_| not_text(number))?
                }
                Err(_) => return Err(not_text(number)),
            };
            text.push_str(valid);
            return Ok((text, Some(number)));
        }
        text.push_str(std::str::from_utf8(&line).map_err(|_| not_text(number))?);
    }
    Ok((text, None))
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
