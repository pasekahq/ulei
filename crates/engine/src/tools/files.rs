//! `read`, `write` and `edit`: UTF-8 text files, paths relative to the working directory.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{Args, Context, EDIT, Output, READ, Tool, WRITE, object_schema};

/// Most bytes of file text one `read` call returns.
pub const MAX_READ: usize = 128 * 1024;

pub(super) struct Read;
pub(super) struct Write;
pub(super) struct Edit;

impl Tool for Read {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: READ.into(),
            description: format!(
                "Read a UTF-8 text file, optionally from a 1-based line `offset` for `limit` lines. \
                 Returns at most {MAX_READ} bytes."
            ),
            input_schema: object_schema(
                json!({
                    "file_path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1}
                }),
                &["file_path"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let result = Args::new(input, &["file_path", "offset", "limit"]).and_then(|args| {
            let path = resolve(ctx.cwd, args.str("file_path")?)?;
            let offset = positive(args.opt_u64("offset")?, "offset")?.unwrap_or(1);
            let limit = positive(args.opt_u64("limit")?, "limit")?;
            read(&path, offset, limit)
        });
        result.into()
    }
}

impl Tool for Write {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: WRITE.into(),
            description: "Create or replace a file with `content`, creating parent directories."
                .into(),
            input_schema: object_schema(
                json!({
                    "file_path": {"type": "string"},
                    "content": {"type": "string"}
                }),
                &["file_path", "content"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let result = Args::new(input, &["file_path", "content"]).and_then(|args| {
            let path = resolve(ctx.cwd, args.str("file_path")?)?;
            let content = args.str("content")?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            fs::write(&path, content)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path.display()
            ))
        });
        result.into()
    }
}

impl Tool for Edit {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: EDIT.into(),
            description: "Replace the exact text `old_string` by `new_string` in a UTF-8 file. \
                          `old_string` must occur exactly once unless `replace_all` is true."
                .into(),
            input_schema: object_schema(
                json!({
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                }),
                &["file_path", "old_string", "new_string"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let result = Args::new(
            input,
            &["file_path", "old_string", "new_string", "replace_all"],
        )
        .and_then(|args| {
            let path = resolve(ctx.cwd, args.str("file_path")?)?;
            let old = args.str("old_string")?;
            let new = args.str("new_string")?;
            let replace_all = args.opt_bool("replace_all")?.unwrap_or(false);
            edit(&path, old, new, replace_all)
        });
        result.into()
    }
}

fn resolve(cwd: &Path, file_path: &str) -> Result<PathBuf, String> {
    if file_path.is_empty() {
        return Err("field `file_path` is empty".into());
    }
    Ok(cwd.join(file_path))
}

fn positive(value: Option<u64>, name: &str) -> Result<Option<usize>, String> {
    value
        .map(|v| match usize::try_from(v) {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("field `{name}` must be a positive integer")),
        })
        .transpose()
}

fn read(path: &Path, offset: usize, limit: Option<usize>) -> Result<String, String> {
    let failed = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let not_text = |line: usize| format!("{} is not UTF-8 text (line {line})", path.display());
    let mut reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut number = 0;
    while number + 1 < offset {
        if reader.skip_until(b'\n').map_err(failed)? == 0 {
            return Ok(String::new());
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
            write!(
                text,
                "\n[truncated at {MAX_READ} bytes; continue with offset {number}]"
            )
            .map_err(|e| e.to_string())?;
            return Ok(text);
        }
        text.push_str(std::str::from_utf8(&line).map_err(|_| not_text(number))?);
    }
    Ok(text)
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
