// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! SOUL.md as the identity and the project context files of the working directory, each cut
//! to [`CONTEXT_FILE_MAX_CHARS`] with a marker that names the file to read in full.

use std::io;
use std::path::{Path, PathBuf};

use super::Error;
use crate::tools::{py_strip, realpath};

/// Most chars of one context file before its middle is cut.
pub const CONTEXT_FILE_MAX_CHARS: usize = 20_000;
const HEAD_RATIO: f64 = 0.7;
const TAIL_RATIO: f64 = 0.2;

const AGENTS_NAMES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "agents.md"];
const CLAUDE_NAMES: [&str; 2] = ["CLAUDE.md", "claude.md"];

/// The identity text of the SOUL.md at `path`, stripped and cut.
///
/// # Errors
/// [`Error::NoSoul`] when the file is absent or blank, [`Error::Read`] when it is not
/// readable UTF-8 text.
pub(super) fn soul(path: &Path) -> Result<String, Error> {
    let content = read(path)?.unwrap_or_default();
    let content = without_bom(py_strip(&content));
    if content.is_empty() {
        return Err(Error::NoSoul(path.to_owned()));
    }
    Ok(truncate(content, "SOUL.md", path))
}

/// The project context of `cwd`: its first non-empty source of the AGENTS.md chain from the
/// git root down, CLAUDE.md and the cursor rules, under its heading; empty without one.
///
/// # Errors
/// [`Error::Read`] for a context file that is not readable UTF-8 text.
pub(super) fn project(cwd: &Path) -> Result<String, Error> {
    let cwd = realpath(cwd);
    let mut context = agents(&cwd)?;
    if context.is_empty() {
        context = claude(&cwd)?;
    }
    if context.is_empty() {
        context = cursorrules(&cwd)?;
    }
    if context.is_empty() {
        return Ok(context);
    }
    Ok(format!(
        "# Project Context\n\nThe following project context files have been loaded and should be followed:\n\n{context}"
    ))
}

fn agents(cwd: &Path) -> Result<String, Error> {
    let mut sections: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for dir in agents_chain(cwd) {
        for name in AGENTS_NAMES {
            let candidate = dir.join(name);
            let Some(raw) = read(&candidate)? else {
                continue;
            };
            let content = py_strip(&raw);
            if content.is_empty() {
                continue;
            }
            if seen.iter().any(|s| s == content) {
                break;
            }
            seen.push(content.to_owned());
            let label = if dir == cwd {
                name.to_owned()
            } else {
                relative(&candidate, &dir, cwd)
            };
            let section = format!("## {label}\n\n{}", without_bom(content));
            sections.push(truncate(&section, &label, &candidate));
            break;
        }
    }
    Ok(match sections.len() {
        0 | 1 => sections.pop().unwrap_or_default(),
        _ => truncate(
            &sections.join("\n\n"),
            "AGENTS.md (directory chain)",
            &cwd.join("AGENTS.md"),
        ),
    })
}

fn claude(cwd: &Path) -> Result<String, Error> {
    for name in CLAUDE_NAMES {
        let candidate = cwd.join(name);
        let Some(raw) = read(&candidate)? else {
            continue;
        };
        let content = py_strip(&raw);
        if !content.is_empty() {
            let section = format!("## {name}\n\n{}", without_bom(content));
            return Ok(truncate(&section, "CLAUDE.md", &candidate));
        }
    }
    Ok(String::new())
}

fn cursorrules(cwd: &Path) -> Result<String, Error> {
    let mut out = String::new();
    if let Some(raw) = read(&cwd.join(".cursorrules"))? {
        let content = py_strip(&raw);
        if !content.is_empty() {
            for piece in ["## .cursorrules\n\n", without_bom(content), "\n\n"] {
                out.push_str(piece);
            }
        }
    }
    let rules = cwd.join(".cursor").join("rules");
    if rules.is_dir() {
        let entries = std::fs::read_dir(&rules).map_err(|e| Error::Read(rules.clone(), e))?;
        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            let name = entry
                .map_err(|e| Error::Read(rules.clone(), e))?
                .file_name()
                .to_string_lossy()
                .into_owned();
            #[allow(
                clippy::case_sensitive_file_extension_comparisons,
                reason = "the reference glob `*.mdc` is case-sensitive"
            )]
            if name.ends_with(".mdc") {
                names.push(name);
            }
        }
        crate::sort::sort(&mut names);
        for name in names {
            let Some(raw) = read(&rules.join(&name))? else {
                continue;
            };
            let content = py_strip(&raw);
            if !content.is_empty() {
                for piece in [
                    "## .cursor/rules/",
                    &name,
                    "\n\n",
                    without_bom(content),
                    "\n\n",
                ] {
                    out.push_str(piece);
                }
            }
        }
    }
    if out.is_empty() {
        return Ok(out);
    }
    Ok(truncate(&out, ".cursorrules", &cwd.join(".cursorrules")))
}

/// The directories whose AGENTS.md apply to `cwd`: from its git root down to it, or `cwd`
/// alone outside a repository.
fn agents_chain(cwd: &Path) -> Vec<PathBuf> {
    let Some(root) = cwd.ancestors().find(|dir| dir.join(".git").exists()) else {
        return vec![cwd.to_owned()];
    };
    let mut chain = vec![root.to_owned()];
    if let Ok(rel) = cwd.strip_prefix(root) {
        let mut acc = root.to_owned();
        for part in rel.components() {
            acc.push(part);
            chain.push(acc.clone());
        }
    }
    chain
}

/// `candidate` in `dir`, an ancestor of `cwd`, relative to `cwd`.
fn relative(candidate: &Path, dir: &Path, cwd: &Path) -> String {
    let depth = cwd
        .strip_prefix(dir)
        .map_or(0, |rel| rel.components().count());
    let name = candidate
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    format!("{}{name}", "../".repeat(depth))
}

/// The text of the file at `path`; `None` when it is absent or a directory.
fn read(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::IsADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::Read(path.to_owned(), e)),
    }
}

fn without_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// `content` with its middle replaced by a marker when it holds more than
/// [`CONTEXT_FILE_MAX_CHARS`] chars: the head and tail kept, `read_path` named for the rest.
fn truncate(content: &str, filename: &str, read_path: &Path) -> String {
    let len = content.chars().count();
    if len <= CONTEXT_FILE_MAX_CHARS {
        return content.to_owned();
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the float ratios the reference truncates"
    )]
    let (head_chars, tail_chars) = (
        (CONTEXT_FILE_MAX_CHARS as f64 * HEAD_RATIO) as usize,
        (CONTEXT_FILE_MAX_CHARS as f64 * TAIL_RATIO) as usize,
    );
    let head: String = content.chars().take(head_chars).collect();
    let tail: String = content.chars().skip(len - tail_chars).collect();
    format!(
        "{head}\n\n[...truncated {filename}: kept {head_chars}+{tail_chars} of {len} chars. The middle is omitted — if you need the full instructions, read the complete file with the read_file tool: {}]\n\n{tail}",
        read_path.display()
    )
}
