// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `search_files`: lines matching a regular expression, or files whose name matches a glob,
//! in a file or under a directory, as a paged JSON result.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Read as _};
use std::path::{Component, Path};
use std::time::SystemTime;

use regex_lite::Regex;
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::glob::Pattern;
use super::redact::{Flags, Redactor};
use super::safety::read_block_error;
use super::tracker::{Call, Shared};
use super::walk::{self, End, Kind, Step, Unreadable};
use super::{
    Args, Context, Interrupt, Output, SEARCH_FILES, Tool, file_name, json_array, json_object,
    json_str, object_schema, py_int, realpath, resolve,
};

/// Results one call returns when it names no `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Most characters of a matching or context line shown.
pub const MAX_CONTENT_CHARS: usize = 500;
/// Most bytes of file content one call reads before it stops.
pub const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// Output lines read past `offset + limit` when `context` is set, for context lines and group
/// separators.
const CONTEXT_EXTRA: usize = 200;
/// From this many matches on, content matches are grouped under their path.
const DENSIFY_MIN_MATCHES: usize = 5;
/// A NUL byte in a file's first block marks it binary.
const BLOCK: usize = 8 * 1024;
/// Lines read between two looks at the interrupt handle.
const LINES_PER_CHECK: usize = 4096;
/// The `limit_reason` of a search stopped by the bytes or entries it may read.
const SCAN_LIMIT: &str = "scan_limit";

const DESCRIPTION: &str = "Search file contents or find files by name. Use this instead of \
grep/rg/find/ls in terminal.

Content search (target='content'): Regex search inside files. Output modes: full matches with \
line numbers, file paths only, or match counts.

File search (target='files'): Find files by glob pattern (e.g., '*.py', '*config*'). Also use \
this instead of ls \u{2014} results sorted by modification time.";

/// `search_files`; `redactor` masks secrets in the matched lines it returns, `tracker` counts
/// repeated searches of the session.
pub(super) struct SearchFiles {
    pub(super) redactor: Redactor,
    pub(super) tracker: Shared,
}

impl Tool for SearchFiles {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SEARCH_FILES.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern for content search, or glob pattern \
                                        (e.g., '*.py') for file search"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["content", "files"],
                        "description": "'content' searches inside file contents, 'files' \
                                        searches for files by name",
                        "default": "content"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search in (default: current \
                                        working directory)",
                        "default": "."
                    },
                    "file_glob": {
                        "type": "string",
                        "description": "Filter files by pattern in grep mode (e.g., '*.py' to \
                                        only search Python files)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 50)",
                        "default": DEFAULT_LIMIT
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Skip first N results for pagination (default: 0)",
                        "default": 0
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_only", "count"],
                        "description": "Output format for grep mode: 'content' shows matching \
                                        lines with line numbers, 'files_only' lists file paths, \
                                        'count' shows match counts per file",
                        "default": "content"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Number of context lines before and after each match \
                                        (grep mode only)",
                        "default": 0
                    }
                }),
                &["pattern"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let request = match Request::parse(&Args(input)) {
            Ok(request) => request,
            Err(e) => return Output::error(e),
        };
        let count = self.tracker.lock().repeat(Call::Search {
            pattern: request.pattern.to_owned(),
            target: request.target.to_owned(),
            path: request.path.to_owned(),
            file_glob: request.file_glob.unwrap_or("").to_owned(),
            limit: request.limit,
            offset: request.offset,
        });
        if count >= 4 {
            return Output::error_with(
                format!(
                    "BLOCKED: You have run this exact search {count} times in a row. The results \
                     have NOT changed. You already have this information. STOP re-searching and \
                     proceed with your task."
                ),
                [
                    ("pattern", json_str(request.pattern)),
                    ("already_searched", count.to_string()),
                ],
            );
        }
        if let Some(error) = read_block_error(&realpath(&resolve(ctx.cwd, request.path))) {
            return Output::error(error);
        }
        let warning = (count >= 3).then(|| {
            format!(
                "You have run this exact search {count} times consecutively. The results have \
                 not changed. Use the information you already have."
            )
        });
        request.run(ctx, self.redactor, warning.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Content,
    FilesOnly,
    Count,
}

struct Request<'a> {
    pattern: &'a str,
    /// The target as named, `grep` and `find` mapped to `content` and `files`.
    target: &'a str,
    files: bool,
    path: &'a str,
    file_glob: Option<&'a str>,
    limit: usize,
    offset: usize,
    mode: Mode,
    context: usize,
}

impl<'a> Request<'a> {
    fn parse(args: &Args<'a>) -> Result<Self, String> {
        let target = match args.opt_str("target")? {
            None | Some("grep") => "content",
            Some("find") => "files",
            Some(target) => target,
        };
        let mode = match args.opt_str("output_mode")? {
            Some("files_only") => Mode::FilesOnly,
            Some("count") => Mode::Count,
            _ => Mode::Content,
        };
        Ok(Self {
            pattern: args.opt_str("pattern")?.unwrap_or(""),
            target,
            files: target == "files",
            path: args.opt_str("path")?.unwrap_or("."),
            file_glob: args.opt_str("file_glob")?.filter(|g| !g.is_empty()),
            limit: coerce(args.present("limit"), DEFAULT_LIMIT, 1),
            offset: coerce(args.present("offset"), 0, 0),
            mode,
            context: coerce(args.present("context"), 0, 0),
        })
    }

    /// Runs the search; `warning` is the repeat warning its result carries.
    fn run(&self, ctx: Context<'_>, redactor: Redactor, warning: Option<&str>) -> Output {
        let root = resolve(ctx.cwd, self.path);
        let shown = if self.path.starts_with('~') {
            root.display().to_string()
        } else {
            self.path.to_owned()
        };
        let Ok(meta) = fs::metadata(&root) else {
            return failure(&not_found(ctx.cwd, &shown), warning);
        };
        let found = if self.files {
            self.find(&root, &shown, meta.is_dir(), ctx.interrupt)
        } else {
            self.grep(&root, &shown, meta.is_dir(), ctx.interrupt)
        };
        match found {
            Ok(mut found) => {
                let omitted = found.omit_read_blocked(ctx.cwd);
                let file_read = Flags {
                    file_read: true,
                    ..Flags::default()
                };
                for hit in &mut found.matches {
                    if let Cow::Owned(redacted) = redactor.sensitive_text(&hit.content, file_read) {
                        hit.content = redacted;
                    }
                }
                self.render(&found, omitted, warning)
            }
            Err(e) => failure(&e, warning),
        }
    }

    /// Lines read before the search stops, as the reference's `head -n` over its search output.
    fn fetch_limit(&self) -> usize {
        let extra = if self.context > 0 { CONTEXT_EXTRA } else { 0 };
        self.offset.saturating_add(self.limit).saturating_add(extra)
    }

    /// Files whose name matches the pattern, newest first.
    fn find(
        &self,
        root: &Path,
        shown: &str,
        is_dir: bool,
        interrupt: &Interrupt,
    ) -> Result<Found, String> {
        let name = if self.pattern.starts_with("**/") || self.pattern.contains('/') {
            self.pattern.rsplit('/').next().unwrap_or("")
        } else {
            self.pattern
        };
        let glob = if name.starts_with('*') {
            name.to_owned()
        } else {
            format!("*{name}")
        };
        let glob = Pattern::parse(&glob).map_err(|e| format!("Search failed: {e}"))?;
        let mut files: Vec<(SystemTime, String)> = Vec::new();
        let mut scan_limited = false;
        if is_dir {
            let walked = walk::walk(root, interrupt, |comps, path, kind| {
                if kind == Kind::File && glob.matches(&comps[comps.len() - 1..]) {
                    files.push((modified(path), join(shown, comps)));
                }
                Step::Continue
            })?;
            match walked.end {
                End::Interrupted => return Err("interrupted".into()),
                End::ScanLimit => scan_limited = true,
                End::Complete | End::Stopped => {}
            }
        } else if glob.matches(&[file_name(root)]) {
            files.push((modified(root), shown.to_owned()));
        }
        crate::sort::sort_by(&mut files, |a, b| b.0.cmp(&a.0));
        let fetch = self.offset.saturating_add(self.limit);
        files.truncate(fetch);
        let fetched = files.len();
        let page = files
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .map(|(_, path)| path)
            .collect();
        Ok(Found {
            total: fetched,
            files: page,
            truncated: fetched >= fetch || scan_limited,
            limit_reason: scan_limited.then_some(SCAN_LIMIT),
            ..Found::default()
        })
    }

    /// Lines matching the pattern, as the output mode asks.
    fn grep(
        &self,
        root: &Path,
        shown: &str,
        is_dir: bool,
        interrupt: &Interrupt,
    ) -> Result<Found, String> {
        let regex = Regex::new(self.pattern).map_err(|e| format!("Search failed: {e}"))?;
        let filter = self
            .file_glob
            .map(Pattern::parse)
            .transpose()
            .map_err(|e| format!("Search failed: {e}"))?;
        let mut scan = Scan {
            regex,
            mode: self.mode,
            context: self.context,
            budget: self.fetch_limit(),
            offset: self.offset,
            limit: self.limit,
            interrupt,
            out: Found::default(),
            lines: 0,
            read: 0,
            separator_due: false,
            unreadable: Unreadable::default(),
        };
        let wanted = |comps: &[String]| match &filter {
            None => true,
            Some(f) if f.is_name() => f.matches(&comps[comps.len() - 1..]),
            Some(f) => f.matches(comps),
        };
        let mut halt = None;
        if is_dir {
            let walked = walk::walk(root, interrupt, |comps, path, kind| {
                match kind {
                    Kind::Dir => {
                        return match &filter {
                            Some(f) if !f.is_name() && !f.may_match_below(comps) => Step::Prune,
                            _ => Step::Continue,
                        };
                    }
                    Kind::File if wanted(comps) => {}
                    Kind::File | Kind::Symlink | Kind::Other => return Step::Continue,
                }
                let display = join(shown, comps);
                match scan.file(path, &display) {
                    Ok(None) => Step::Continue,
                    Ok(Some(stop)) => {
                        halt = Some(stop);
                        Step::Stop
                    }
                    Err(e) => {
                        scan.unreadable.note(&display, &e);
                        Step::Continue
                    }
                }
            })?;
            scan.unreadable.absorb(walked.unreadable);
            halt = halt.or(match walked.end {
                End::ScanLimit => Some(Halt::ScanLimit),
                End::Interrupted => Some(Halt::Interrupted),
                End::Complete | End::Stopped => None,
            });
        } else if admits_file(filter.as_ref(), root) {
            halt = scan
                .file(root, shown)
                .map_err(|e| format!("Search failed: {shown}: {e}"))?;
        }
        let mut found = scan.out;
        match halt {
            Some(Halt::Interrupted) => return Err("interrupted".into()),
            Some(Halt::ScanLimit) => found.limit_reason = Some(SCAN_LIMIT),
            Some(Halt::Fetched) | None => {}
        }
        let empty = found.matches.is_empty() && found.files.is_empty() && found.counts.is_empty();
        if let (true, Some(first)) = (empty, scan.unreadable.first()) {
            return Err(format!("Search failed: {first}"));
        }
        found.truncated = found.limit_reason.is_some()
            || (self.mode == Mode::Content && found.total > self.offset.saturating_add(self.limit));
        Ok(found)
    }

    fn render(&self, found: &Found, omitted: usize, warning: Option<&str>) -> Output {
        let mut fields = vec![("total_count", found.total.to_string())];
        if found.matches.len() >= DENSIFY_MIN_MATCHES {
            fields.push((
                "matches_format",
                json_str(
                    "path-grouped: each file path on its own line, followed by indented \
                     '<line>: <content>' rows for matches in that file",
                ),
            ));
            fields.push(("matches_text", json_str(&densify(&found.matches))));
        } else if !found.matches.is_empty() {
            let matches = found.matches.iter().map(|m| {
                json_object([
                    ("path", json_str(&m.path)),
                    ("line", m.line.to_string()),
                    ("content", json_str(&m.content)),
                ])
            });
            fields.push(("matches", json_array(matches)));
        }
        if !found.files.is_empty() {
            fields.push(("files", json_array(found.files.iter().map(|f| json_str(f)))));
        }
        if !found.counts.is_empty() {
            let counts = found
                .counts
                .iter()
                .map(|(path, n)| (path.as_str(), n.to_string()));
            fields.push(("counts", json_object(counts)));
        }
        if found.truncated {
            fields.push(("truncated", "true".into()));
        }
        if let Some(reason) = found.limit_reason {
            fields.push(("limit_reason", json_str(reason)));
        }
        if omitted > 0 {
            let note = format!(
                "{omitted} result(s) omitted because they target credential, token, cache, or \
                 secret-bearing environment files."
            );
            fields.push(("_omitted", json_str(&note)));
        }
        if let Some(warning) = warning {
            fields.push(("_warning", json_str(warning)));
        }
        let mut text = json_object(fields);
        if found.truncated {
            let next = self.offset.saturating_add(self.limit);
            let _ = write!(
                text,
                "\n\n[Hint: Results truncated. Use offset={next} to see more, or narrow with a \
                 more specific pattern or file_glob.]"
            );
        }
        Output::ok(text)
    }
}

/// A matching line or, with `context`, a line around one.
struct Hit {
    path: String,
    line: usize,
    content: String,
}

/// The page of results and what the search counted.
#[derive(Default)]
struct Found {
    total: usize,
    matches: Vec<Hit>,
    files: Vec<String>,
    counts: Vec<(String, usize)>,
    truncated: bool,
    limit_reason: Option<&'static str>,
}

impl Found {
    /// Drops the matches, files and counts whose path `read_file` refuses; how many it dropped.
    fn omit_read_blocked(&mut self, cwd: &Path) -> usize {
        let blocked = |path: &str| read_block_error(&realpath(&resolve(cwd, path))).is_some();
        let before = self.matches.len() + self.files.len() + self.counts.len();
        self.matches.retain(|m| !blocked(&m.path));
        self.files.retain(|f| !blocked(f));
        self.counts.retain(|(path, _)| !blocked(path));
        before - (self.matches.len() + self.files.len() + self.counts.len())
    }
}

/// Why the search stopped before covering everything.
#[derive(Debug, Clone, Copy)]
enum Halt {
    /// The fetch limit of output lines is reached.
    Fetched,
    ScanLimit,
    Interrupted,
}

/// A content search in progress over the output line stream of the reference's search.
struct Scan<'a> {
    regex: Regex,
    mode: Mode,
    context: usize,
    budget: usize,
    offset: usize,
    limit: usize,
    interrupt: &'a Interrupt,
    out: Found,
    /// Output lines produced so far, group separators included.
    lines: usize,
    read: u64,
    /// A context group was shown, so the next one starts with a `--` separator.
    separator_due: bool,
    unreadable: Unreadable,
}

impl Scan<'_> {
    /// Scans one file; the outer `Err` is a read failure, `Some` ends the whole search.
    fn file(&mut self, path: &Path, display: &str) -> io::Result<Option<Halt>> {
        let mut reader = BufReader::with_capacity(BLOCK, File::open(path)?);
        let block = reader.fill_buf()?;
        if block.contains(&0) {
            // The lines of a text file count its first block; a binary file has no lines.
            let n = block.len() as u64;
            if n > MAX_SCAN_BYTES - self.read {
                return Ok(Some(Halt::ScanLimit));
            }
            self.read += n;
            return Ok(None);
        }
        let mut before: VecDeque<(usize, String)> = VecDeque::new();
        let mut last_shown: Option<usize> = None;
        let mut after = 0;
        let mut matched = 0;
        let mut bytes = Vec::new();
        let mut number = 0usize;
        loop {
            let left = MAX_SCAN_BYTES - self.read;
            bytes.clear();
            let n = (&mut reader).take(left + 1).read_until(b'\n', &mut bytes)? as u64;
            if n == 0 {
                break;
            }
            if n > left {
                return Ok(Some(Halt::ScanLimit));
            }
            self.read += n;
            number += 1;
            if number.is_multiple_of(LINES_PER_CHECK) && self.interrupt.is_interrupted() {
                return Ok(Some(Halt::Interrupted));
            }
            let line = String::from_utf8_lossy(&bytes);
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let hit = self.regex.is_match(line);
            if !hit {
                if after > 0 {
                    after -= 1;
                    if let Some(halt) = self.show(display, number, line, &mut last_shown) {
                        return Ok(Some(halt));
                    }
                } else if self.context > 0 && self.mode == Mode::Content {
                    if before.len() == self.context {
                        before.pop_front();
                    }
                    before.push_back((number, line.to_owned()));
                }
                continue;
            }
            matched += 1;
            match self.mode {
                Mode::FilesOnly => return Ok(self.push_file(display)),
                Mode::Count => continue,
                Mode::Content => {}
            }
            for (n, text) in before.drain(..) {
                if let Some(halt) = self.show(display, n, &text, &mut last_shown) {
                    return Ok(Some(halt));
                }
            }
            if let Some(halt) = self.show(display, number, line, &mut last_shown) {
                return Ok(Some(halt));
            }
            after = self.context;
        }
        if self.mode == Mode::Count && matched > 0 {
            self.out.total += matched;
            self.out.counts.push((display.to_owned(), matched));
            return Ok(self.next_line());
        }
        Ok(None)
    }

    /// Shows one content line, after a `--` separator when it does not continue the group.
    fn show(
        &mut self,
        display: &str,
        number: usize,
        line: &str,
        last_shown: &mut Option<usize>,
    ) -> Option<Halt> {
        let continues = last_shown.is_some_and(|last| number == last + 1);
        if self.context > 0
            && self.separator_due
            && !continues
            && let Some(halt) = self.next_line()
        {
            return Some(halt);
        }
        *last_shown = Some(number);
        self.separator_due = true;
        let index = self.out.total;
        self.out.total += 1;
        if index >= self.offset && index - self.offset < self.limit {
            let content = match line.char_indices().nth(MAX_CONTENT_CHARS) {
                Some((cut, _)) => &line[..cut],
                None => line,
            };
            self.out.matches.push(Hit {
                path: display.to_owned(),
                line: number,
                content: content.to_owned(),
            });
        }
        self.next_line()
    }

    fn push_file(&mut self, display: &str) -> Option<Halt> {
        let index = self.out.total;
        self.out.total += 1;
        if index >= self.offset && index - self.offset < self.limit {
            self.out.files.push(display.to_owned());
        }
        self.next_line()
    }

    /// Counts one output line; `Some` once the fetch limit is reached.
    fn next_line(&mut self) -> Option<Halt> {
        self.lines += 1;
        (self.lines >= self.budget).then_some(Halt::Fetched)
    }
}

/// Content matches grouped under their path: the path on its own line, then `  <line>: <text>`.
fn densify(matches: &[Hit]) -> String {
    let mut lines = Vec::new();
    let mut current: Option<&str> = None;
    for m in matches {
        if current != Some(m.path.as_str()) {
            lines.push(m.path.clone());
            current = Some(&m.path);
        }
        lines.push(format!("  {}: {}", m.line, m.content.trim_end()));
    }
    lines.join("\n")
}

/// The failed result `{"total_count": 0, "error": ...}` with the repeat `warning` if any.
fn failure(error: &str, warning: Option<&str>) -> Output {
    let mut fields = vec![("total_count", "0".into()), ("error", json_str(error))];
    if let Some(warning) = warning {
        fields.push(("_warning", json_str(warning)));
    }
    Output {
        content: json_object(fields),
        is_error: true,
    }
}

/// `Path not found: <path>`, with up to five entries of its parent whose names resemble it.
fn not_found(cwd: &Path, shown: &str) -> String {
    let mut text = format!("Path not found: {shown}");
    let (parent, query) = match shown.rsplit_once('/') {
        Some(("", query)) => ("/", query),
        Some((parent, query)) => (parent, query),
        None => (".", shown),
    };
    if query.is_empty() {
        return text;
    }
    let Ok(entries) = fs::read_dir(resolve(cwd, parent)) else {
        return text;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    crate::sort::sort(&mut names);
    names.truncate(20);
    let query = query.to_lowercase();
    let prefix: String = query.chars().take(3).collect();
    let similar: Vec<String> = names
        .iter()
        .filter(|name| {
            let name = name.to_lowercase();
            name.contains(&query) || query.contains(&name) || name.starts_with(&prefix)
        })
        .take(5)
        .map(|name| {
            if parent.ends_with('/') {
                format!("{parent}{name}")
            } else {
                format!("{parent}/{name}")
            }
        })
        .collect();
    if !similar.is_empty() {
        let _ = write!(text, ". Similar paths: {}", similar.join(", "));
    }
    text
}

/// `value` as Python's `int()` reads it, `default` when it cannot, never below `min`.
fn coerce(value: Option<&Value>, default: usize, min: usize) -> usize {
    py_int(value)
        .map_or(default, |n| usize::try_from(n.max(0)).unwrap_or(usize::MAX))
        .max(min)
}

/// Whether `filter` admits the single file `path`: a name glob by its file name, a path glob by
/// some trailing run of its components, the path a walk from one of its ancestors would see.
fn admits_file(filter: Option<&Pattern>, path: &Path) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let mut comps: Vec<String> = path
        .components()
        .rev()
        .map_while(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    comps.reverse();
    if filter.is_name() {
        return filter.matches(&comps[comps.len().saturating_sub(1)..]);
    }
    (0..comps.len()).any(|start| filter.matches(&comps[start..]))
}

/// The display path of a walked entry under the root shown as `shown`.
fn join(shown: &str, comps: &[String]) -> String {
    format!("{}/{}", shown.trim_end_matches('/'), comps.join("/"))
}

fn modified(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use regex_lite::Regex;

    use super::{
        BLOCK, Found, Halt, Hit, Interrupt, MAX_SCAN_BYTES, Mode, Path, Request, Scan, Unreadable,
    };

    #[test]
    fn the_first_block_of_a_binary_file_counts_toward_the_byte_limit() {
        let path = std::env::temp_dir().join(format!("ulei-search-{}.bin", std::process::id()));
        fs::write(&path, vec![0u8; 3 * BLOCK]).unwrap();
        let interrupt = Interrupt::new();
        let mut scan = Scan {
            regex: Regex::new("x").unwrap(),
            mode: Mode::Content,
            context: 0,
            budget: 50,
            offset: 0,
            limit: 50,
            interrupt: &interrupt,
            out: Found::default(),
            lines: 0,
            read: 0,
            separator_due: false,
            unreadable: Unreadable::default(),
        };
        assert!(scan.file(&path, "a.bin").unwrap().is_none());
        assert_eq!(scan.read, BLOCK as u64);
        scan.read = MAX_SCAN_BYTES - 1;
        let halt = scan.file(&path, "a.bin").unwrap();
        fs::remove_file(&path).unwrap();
        assert!(matches!(halt, Some(Halt::ScanLimit)), "{halt:?}");
        assert_eq!(scan.read, MAX_SCAN_BYTES - 1);
    }

    #[test]
    fn read_blocked_paths_are_omitted_with_a_note() {
        let request = Request {
            pattern: "KEY",
            target: "content",
            files: false,
            path: ".",
            file_glob: None,
            limit: 50,
            offset: 0,
            mode: Mode::Content,
            context: 0,
        };
        let hit = |path: &str, line: usize| Hit {
            path: path.into(),
            line,
            content: "KEY".into(),
        };
        let mut found = Found {
            total: 3,
            matches: vec![hit("app/.env", 1), hit("app/main.rs", 2)],
            counts: vec![("app/.ENVRC".into(), 1)],
            ..Found::default()
        };
        let omitted = found.omit_read_blocked(Path::new("/"));
        assert_eq!(omitted, 2);
        assert_eq!(
            request.render(&found, omitted, None).content,
            "{\"total_count\": 3, \"matches\": [{\"path\": \"app/main.rs\", \"line\": 2, \
             \"content\": \"KEY\"}], \"_omitted\": \"2 result(s) omitted because they target \
             credential, token, cache, or secret-bearing environment files.\"}"
        );
    }
}
