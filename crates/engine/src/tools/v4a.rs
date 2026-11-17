// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! V4A multi-file patches of `patch` in `mode='patch'`: the parser, the validation of every
//! operation before any write, and the Update, Add, Delete and Move operations.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use rustix::io::Errno;

use super::files::{BOM, write};
use super::read::{BINARY_EXTENSIONS, describe_binary, likely_binary, not_regular, splitext};
use super::repr::repr;
use super::{
    Fields, basename, difflib, file_name, fuzzy, is_py_space, json_array, json_str, py_splitlines,
    strerror,
};

/// Extensions a raw read answers with empty content before any binary check.
const IMAGE_EXTENSIONS: [&str; 7] = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico"];

/// Bytes of the head of a file the binary check looks at.
const SAMPLE_BYTES: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Update,
    Delete,
    Move,
}

struct Operation {
    kind: Kind,
    path: String,
    /// Destination of a Move; empty for the other kinds.
    new_path: String,
    hunks: Vec<Hunk>,
}

#[derive(Default)]
struct Hunk {
    /// The text between `@@` markers.
    hint: Option<String>,
    /// Prefix byte (` `, `-` or `+`) and text of each line.
    lines: Vec<(u8, String)>,
}

impl Hunk {
    fn has(&self, prefix: u8) -> bool {
        self.lines.iter().any(|(p, _)| *p == prefix)
    }

    /// The context lines and the lines with `prefix`, in order.
    fn side(&self, prefix: u8) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|(p, _)| *p == b' ' || *p == prefix)
            .map(|(_, line)| line.as_str())
            .collect()
    }
}

/// What a V4A patch did, as `PatchResult.to_dict` reports it; failed when `error` is set.
#[derive(Default)]
pub(super) struct PatchResult {
    diff: String,
    modified: Vec<String>,
    created: Vec<String>,
    deleted: Vec<String>,
    pub(super) error: Option<String>,
}

impl PatchResult {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    pub(super) fn fields(&self) -> Fields {
        let mut fields = vec![("success", self.error.is_none().to_string())];
        if !self.diff.is_empty() {
            fields.push(("diff", json_str(&self.diff)));
        }
        for (key, paths) in [
            ("files_modified", &self.modified),
            ("files_created", &self.created),
            ("files_deleted", &self.deleted),
        ] {
            if !paths.is_empty() {
                fields.push((key, json_array(paths.iter().map(|p| json_str(p)))));
            }
        }
        if let Some(error) = &self.error {
            fields.push(("error", json_str(error)));
        }
        Fields(fields)
    }
}

/// A header line: `*** Update|Add|Delete File: <path>` or `*** Move File: <src> -> <dst>`.
enum Header<'a> {
    File(Kind, &'a str),
    Move(&'a str, &'a str),
}

fn header(line: &str) -> Option<Header<'_>> {
    for (word, kind) in [
        ("Update", Kind::Update),
        ("Add", Kind::Add),
        ("Delete", Kind::Delete),
    ] {
        if let Some(path) = after_file(line, word).filter(|rest| !rest.is_empty()) {
            return Some(Header::File(kind, py_strip(path)));
        }
    }
    let (src, dst) = lazy_until(after_file(line, "Move")?, "->", true)?;
    Some(Header::Move(py_strip(src), py_strip(dst)))
}

fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// The text after `***`, optional whitespace, `word`, whitespace and `File:` at the start of
/// `line`.
fn after_file<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    let rest = line
        .strip_prefix("***")?
        .trim_start_matches(is_py_space)
        .strip_prefix(word)?;
    let spaced = rest.trim_start_matches(is_py_space);
    (spaced.len() < rest.len())
        .then_some(spaced)?
        .strip_prefix("File:")
}

/// `\s*(.+?)\s*<delim>` matched at the start of `text` the way a backtracking engine finds it:
/// the group and the text after the delimiter, which must not be empty when `tail` is set.
fn lazy_until<'a>(text: &'a str, delim: &str, tail: bool) -> Option<(&'a str, &'a str)> {
    let spaced = text.len() - text.trim_start_matches(is_py_space).len();
    let back = text[..spaced]
        .chars()
        .next_back()
        .map(|c| spaced - c.len_utf8());
    [Some(spaced), back]
        .into_iter()
        .flatten()
        .find_map(|start| {
            let first = start + text[start..].chars().next()?.len_utf8();
            let at = first + text[first..].find(delim)?;
            let after = &text[at + delim.len()..];
            if tail && after.is_empty() {
                return None;
            }
            let end = text[..at].trim_end_matches(is_py_space).len().max(first);
            Some((&text[start..end], after))
        })
}

/// `***`, optional whitespace, `word`, whitespace, `Patch` and nothing but whitespace.
fn is_marker(line: &str, word: &str) -> bool {
    let Some(rest) = line
        .strip_prefix("***")
        .and_then(|rest| rest.trim_start_matches(is_py_space).strip_prefix(word))
    else {
        return false;
    };
    let spaced = rest.trim_start_matches(is_py_space);
    spaced.len() < rest.len()
        && spaced
            .strip_prefix("Patch")
            .is_some_and(|rest| py_strip(rest).is_empty())
}

/// The header paths of the patch `text` in the order the tool checks them: those of the Update,
/// Add and Delete headers, then the source and destination of each Move.
pub(super) fn header_paths(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut moves = Vec::new();
    for line in text.split('\n') {
        match header(line) {
            Some(Header::File(_, path)) => paths.push(path),
            Some(Header::Move(src, dst)) => moves.extend([src, dst]),
            None => {}
        }
    }
    paths.extend(moves);
    paths
}

/// The operations between the last `Begin Patch` marker before the first `End Patch` marker
/// and that marker, their paths passed through `resolve`.
fn parse(text: &str, resolve: &dyn Fn(&str) -> String) -> Result<Vec<Operation>, String> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let (mut start, mut end) = (0, lines.len());
    for (i, line) in lines.iter().enumerate() {
        if is_marker(line, "Begin") {
            start = i + 1;
        } else if is_marker(line, "End") {
            end = i;
            break;
        }
    }
    let mut operations = Vec::new();
    let mut current: Option<Operation> = None;
    let mut hunk: Option<Hunk> = None;
    for line in &lines[start..end] {
        if let Some(found) = header(line) {
            finish(&mut operations, &mut current, &mut hunk);
            let (kind, path, new_path) = match found {
                Header::File(kind, path) => (kind, resolve(path), String::new()),
                Header::Move(src, dst) => (Kind::Move, resolve(src), resolve(dst)),
            };
            let op = Operation {
                kind,
                path,
                new_path,
                hunks: Vec::new(),
            };
            match kind {
                Kind::Update | Kind::Add => current = Some(op),
                Kind::Delete | Kind::Move => operations.push(op),
            }
        } else if let Some(rest) = line.strip_prefix("@@") {
            if let Some(op) = &mut current {
                op.hunks.extend(hunk.take().filter(|h| !h.lines.is_empty()));
                hunk = Some(Hunk {
                    hint: lazy_until(rest, "@@", false).map(|(hint, _)| hint.to_owned()),
                    lines: Vec::new(),
                });
            }
        } else if current.is_some() && !line.is_empty() {
            let lines = &mut hunk.get_or_insert_with(Hunk::default).lines;
            match line.as_bytes()[0] {
                prefix @ (b' ' | b'-' | b'+') => lines.push((prefix, line[1..].to_owned())),
                b'\\' => {}
                _ => lines.push((b' ', (*line).to_owned())),
            }
        }
    }
    finish(&mut operations, &mut current, &mut hunk);
    let errors: Vec<String> = operations
        .iter()
        .filter(|op| op.kind == Kind::Update && op.hunks.is_empty())
        .map(|op| format!("UPDATE {}: no hunks found", repr(&op.path)))
        .collect();
    if !errors.is_empty() {
        return Err(format!("Parse error: {}", errors.join("; ")));
    }
    Ok(operations)
}

/// Closes the operation being read, keeping its last hunk when that has lines.
fn finish(
    operations: &mut Vec<Operation>,
    current: &mut Option<Operation>,
    hunk: &mut Option<Hunk>,
) {
    if let Some(mut op) = current.take() {
        op.hunks.extend(hunk.take().filter(|h| !h.lines.is_empty()));
        operations.push(op);
    }
}

/// Applies the V4A patch `text` whose header paths `resolve` turns into the paths of the files:
/// every operation is validated before the first one is applied.
pub(super) fn patch_v4a(text: &str, resolve: &dyn Fn(&str) -> String) -> PatchResult {
    let operations = match parse(text, resolve) {
        Ok(operations) => operations,
        Err(e) => return PatchResult::failed(format!("Failed to parse patch: {e}")),
    };
    let errors = validate(&operations);
    if !errors.is_empty() {
        return PatchResult::failed(format!(
            "Patch validation failed (no files were modified):\n{}",
            bullets(&errors)
        ));
    }
    let mut result = PatchResult::default();
    let mut diffs = Vec::new();
    let mut errors = Vec::new();
    for op in &operations {
        let path = &op.path;
        match op.kind {
            Kind::Add => match apply_add(op) {
                Ok(diff) => {
                    result.created.push(path.clone());
                    diffs.push(diff);
                }
                Err(e) => errors.push(format!("Failed to add {path}: {e}")),
            },
            Kind::Delete => match apply_delete(path) {
                Ok(diff) => {
                    result.deleted.push(path.clone());
                    diffs.push(diff);
                }
                Err(e) => errors.push(format!("Failed to delete {path}: {e}")),
            },
            Kind::Move => match move_file(path, &op.new_path) {
                Ok(()) => {
                    result.modified.push(format!("{path} -> {}", op.new_path));
                    diffs.push(format!("# Moved: {path} -> {}", op.new_path));
                }
                Err(e) => errors.push(format!("Failed to move {path}: {e}")),
            },
            Kind::Update => match apply_update(op) {
                Ok(diff) => {
                    result.modified.push(path.clone());
                    diffs.push(diff);
                }
                Err(e) => errors.push(format!("Failed to update {path}: {e}")),
            },
        }
    }
    result.diff = diffs.join("\n");
    if !errors.is_empty() {
        result.error = Some(format!(
            "Apply phase failed (state may be inconsistent \u{2014} run `git diff` to assess):\n{}",
            bullets(&errors)
        ));
    }
    result
}

fn bullets(errors: &[String]) -> String {
    let lines: Vec<String> = errors.iter().map(|e| format!("  \u{2022} {e}")).collect();
    lines.join("\n")
}

/// File contents as the operations validated so far leave them.
#[derive(Default)]
struct Overlay {
    pending: HashMap<String, String>,
    /// Paths a Move or Delete took away.
    removed: HashSet<String>,
}

impl Overlay {
    /// `edit` as for [`read_raw`].
    fn read(&self, path: &str, edit: bool) -> Result<String, String> {
        if let Some(content) = self.pending.get(path) {
            return Ok(content.clone());
        }
        if self.removed.contains(path) {
            return Err("file not found".into());
        }
        read_raw(path, edit)
    }
}

/// The errors of every operation checked against the contents the earlier ones leave, hunks of
/// an Update in order; empty when the whole patch can be applied.
fn validate(operations: &[Operation]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut changes = 0;
    let mut overlay = Overlay::default();
    for op in operations {
        let path = &op.path;
        match op.kind {
            Kind::Update => {
                let mut simulated = match overlay.read(path, true) {
                    Ok(content) => content,
                    Err(e) => {
                        errors.push(format!("{path}: {e}"));
                        continue;
                    }
                };
                for (index, hunk) in op.hunks.iter().enumerate() {
                    if !hunk.has(b'-') && !hunk.has(b'+') {
                        continue;
                    }
                    changes += 1;
                    let (search, replace) = (hunk.side(b'-'), hunk.side(b'+'));
                    if search.is_empty() {
                        if let Some(hint) = &hunk.hint {
                            match count_occurrences(&simulated, hint) {
                                0 => errors.push(format!(
                                    "{path}: addition-only hunk context hint '{hint}' not found"
                                )),
                                1 => {}
                                n => errors.push(format!(
                                    "{path}: addition-only hunk context hint '{hint}' is \
                                     ambiguous ({n} occurrences)"
                                )),
                            }
                        }
                        continue;
                    }
                    if search == replace {
                        continue;
                    }
                    let (search, replace) = (search.join("\n"), replace.join("\n"));
                    match fuzzy::fuzzy_find_and_replace(&simulated, &search, &replace, false) {
                        Ok(next) => simulated = next,
                        Err(_) if fuzzy::is_already_applied(&simulated, &search, &replace) => {}
                        Err(e) => {
                            let label = hunk
                                .hint
                                .as_ref()
                                .map_or_else(|| "(no hint)".to_owned(), |h| format!("'{h}'"));
                            let closest = fuzzy::format_no_match_hint(&e, &search, &simulated);
                            errors.push(format!(
                                "{path}: hunk {} {label} not found \u{2014} {e}{closest}",
                                index + 1
                            ));
                        }
                    }
                }
                overlay.pending.insert(path.clone(), simulated);
            }
            Kind::Delete => {
                changes += 1;
                if overlay.read(path, false).is_err() {
                    errors.push(format!("{path}: file not found for deletion"));
                } else {
                    overlay.removed.insert(path.clone());
                    overlay.pending.remove(path);
                }
            }
            Kind::Move => {
                changes += 1;
                let source = overlay.read(path, false);
                if source.is_err() {
                    errors.push(format!("{path}: source file not found for move"));
                }
                let destination_free = overlay.read(&op.new_path, false).is_err();
                if !destination_free {
                    errors.push(format!(
                        "{}: destination already exists \u{2014} move would overwrite",
                        op.new_path
                    ));
                }
                if let (Ok(content), true) = (source, destination_free) {
                    overlay.pending.insert(op.new_path.clone(), content);
                    overlay.pending.remove(path);
                    overlay.removed.insert(path.clone());
                }
            }
            Kind::Add => changes += 1,
        }
    }
    if errors.is_empty() && changes == 0 {
        errors.push("Patch contains no changes (only context lines were provided)".into());
    }
    errors
}

fn count_occurrences(text: &str, pattern: &str) -> usize {
    occurrences(text, pattern).count()
}

/// Byte offsets of `pattern` in `text`, each searched from one character after the last.
fn occurrences<'a>(text: &'a str, pattern: &'a str) -> impl Iterator<Item = usize> + 'a {
    let mut from = 0;
    std::iter::from_fn(move || {
        let at = from + text.get(from..)?.find(pattern)?;
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
        Some(at)
    })
}

/// The file the `+` lines of every hunk make, written; the diff that creates it.
fn apply_add(op: &Operation) -> Result<String, String> {
    let lines: Vec<&str> = op
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|(prefix, _)| *prefix == b'+')
        .map(|(_, line)| line.as_str())
        .collect();
    write(Path::new(&op.path), &lines.join("\n"), None)?;
    let added: Vec<String> = lines.iter().map(|line| format!("+{line}")).collect();
    Ok(format!(
        "--- /dev/null\n+++ b/{}\n{}",
        op.path,
        added.join("\n")
    ))
}

fn apply_delete(path: &str) -> Result<String, String> {
    let content =
        read_raw(path, false).map_err(|_| format!("Cannot delete {path}: file not found"))?;
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            let reason = match e.raw_os_error() {
                Some(errno) => format!("[Errno {errno}] {}: {}", strerror(&e), repr(path)),
                None => e.to_string(),
            };
            return Err(format!("Failed to delete {path}: {reason}"));
        }
        _ => {}
    }
    let lines: Vec<&str> = py_splitlines(&content, true).collect();
    let diff = difflib::unified_diff(&lines, &[], &format!("a/{path}"), "/dev/null");
    Ok(if diff.is_empty() {
        format!("# Deleted: {path}")
    } else {
        diff
    })
}

/// Moves `src` to `dst`, or into `dst` when it is a directory, as `mv` does.
fn move_file(src: &str, dst: &str) -> Result<(), String> {
    let failed = |e: String| format!("Failed to move {src} -> {dst}: {e}");
    let mut target = PathBuf::from(dst);
    if target.is_dir() {
        target.push(file_name(Path::new(src)));
    }
    if let (Ok(from), Ok(to)) = (fs::canonicalize(src), fs::canonicalize(&target))
        && from == to
    {
        return Err(failed(format!(
            "mv: '{src}' and '{}' are the same file\n",
            target.display()
        )));
    }
    match fs::rename(src, &target) {
        Err(e) if Errno::from_io_error(&e) == Some(Errno::XDEV) => {
            fs::copy(src, &target).and_then(|_| fs::remove_file(src))
        }
        moved => moved,
    }
    .map_err(|e| {
        failed(format!(
            "mv: cannot move '{src}' to '{}': {}\n",
            target.display(),
            strerror(&e)
        ))
    })
}

/// The hunks applied in order to the file, written in its line endings; the unified diff.
fn apply_update(op: &Operation) -> Result<String, String> {
    let current = read_raw(&op.path, true).map_err(|e| format!("Cannot read file: {e}"))?;
    let mut content = current.clone();
    for hunk in &op.hunks {
        let (search, replace) = (hunk.side(b'-'), hunk.side(b'+'));
        if search.is_empty() {
            let insert = replace.join("\n");
            let found: Vec<usize> = match &hunk.hint {
                Some(hint) => occurrences(&content, hint).collect(),
                None => Vec::new(),
            };
            match (hunk.hint.as_ref(), found.as_slice()) {
                (Some(hint), [_, _, ..]) => {
                    return Err(format!(
                        "Addition-only hunk: context hint '{hint}' is ambiguous ({} \
                         occurrences) \u{2014} provide a more unique hint",
                        found.len()
                    ));
                }
                (Some(_), &[at]) => {
                    if let Some(eol) = content[at..].find('\n') {
                        content.insert_str(at + eol + 1, &format!("{insert}\n"));
                    } else {
                        content.push('\n');
                        content.push_str(&insert);
                    }
                }
                _ => content = format!("{}\n{insert}\n", content.trim_end_matches('\n')),
            }
            continue;
        }
        if search == replace {
            continue;
        }
        let (search, replace) = (search.join("\n"), replace.join("\n"));
        let mut replaced = fuzzy::fuzzy_find_and_replace(&content, &search, &replace, false);
        if replaced.is_err()
            && let Some(at) = hunk
                .hint
                .as_ref()
                .and_then(|hint| content.find(hint.as_str()))
        {
            let start = content[..at]
                .char_indices()
                .rev()
                .nth(499)
                .map_or(0, |(i, _)| i);
            let end = content[at..]
                .char_indices()
                .nth(2000)
                .map_or(content.len(), |(i, _)| at + i);
            replaced =
                fuzzy::fuzzy_find_and_replace(&content[start..end], &search, &replace, false)
                    .map(|window| format!("{}{window}{}", &content[..start], &content[end..]));
        }
        match replaced {
            Ok(next) => content = next,
            Err(_) if fuzzy::is_already_applied(&content, &search, &replace) => {}
            Err(e) => {
                let closest = fuzzy::format_no_match_hint(&e, &search, &content);
                return Err(format!("Could not apply hunk: {e}{closest}"));
            }
        }
    }
    write(Path::new(&op.path), &content, Some(&current))?;
    let before: Vec<&str> = py_splitlines(&current, true).collect();
    let after: Vec<&str> = py_splitlines(&content, true).collect();
    Ok(difflib::unified_diff(
        &before,
        &after,
        &format!("a/{}", op.path),
        &format!("b/{}", op.path),
    ))
}

/// The text of the file at `path` without its byte order mark, refused when it is missing, not
/// regular or binary; empty for an image. Bytes that are not UTF-8 refuse the read when the text
/// is to be edited and written back, and read as U+FFFD otherwise.
fn read_raw(path: &str, edit: bool) -> Result<String, String> {
    let not_found = || format!("File not found: {path}");
    let meta = fs::metadata(path).map_err(|_| not_found())?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    let ext = splitext(basename(path)).1.to_lowercase();
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(String::new());
    }
    let mut file = File::open(path).map_err(|_| not_found())?;
    let mut bytes = Vec::new();
    let failed = |_| format!("Failed to read file: {path}");
    (&mut file)
        .take(SAMPLE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    if BINARY_EXTENSIONS.contains(&ext.as_str()) || likely_binary(&bytes) {
        return Err(describe_binary(&bytes, meta.len()));
    }
    file.read_to_end(&mut bytes).map_err(failed)?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) if edit => return Err(format!("Failed to read file: {path}")),
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(match text.strip_prefix(BOM) {
        Some(rest) => rest.to_owned(),
        None => text,
    })
}
