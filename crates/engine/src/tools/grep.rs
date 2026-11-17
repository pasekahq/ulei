//! `grep`: lines matching a regular expression in a file or under a directory.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Read as _};
use std::path::Path;

use regex_lite::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::glob::Pattern;
use super::walk::{self, End, Kind, MAX_SCAN_ENTRIES, Step, Unreadable};
use super::{Args, Context, GREP, Interrupt, Output, Tool, object_schema};

/// Most bytes of matches one `grep` call returns.
pub const MAX_GREP_OUTPUT: usize = 32 * 1024;
/// Most bytes of a matching line shown; the rest is replaced by a marker.
pub const MAX_LINE: usize = 400;
/// Most bytes of file content one `grep` call reads before it stops.
pub const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// A NUL byte in a file's first block marks it binary.
const BLOCK: usize = 8 * 1024;
/// Lines read between two looks at the interrupt handle.
const LINES_PER_CHECK: usize = 4096;

pub(super) struct Grep;

impl Tool for Grep {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: GREP.into(),
            description: format!(
                "Search files for lines matching the regular expression `pattern`, in the file or \
                 under the directory `path` (default the working directory). Prints \
                 `path:line: text`, or only matching file paths with `files_only`. `glob` filters \
                 files: a pattern without `/` matches the file name, otherwise the path below \
                 `path`. Skips binary files, `.git` and symlinks. Returns at most \
                 {MAX_GREP_OUTPUT} bytes; reads at most {MAX_SCAN_BYTES} bytes and \
                 {MAX_SCAN_ENTRIES} directory entries."
            ),
            input_schema: object_schema(
                json!({
                    "pattern": {"type": "string", "description": "Regular expression."},
                    "path": {"type": "string", "description": "File or directory to search."},
                    "glob": {"type": "string", "description": "File filter, e.g. `*.rs`."},
                    "case_insensitive": {"type": "boolean"},
                    "files_only": {"type": "boolean"}
                }),
                &["pattern"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let known = ["pattern", "path", "glob", "case_insensitive", "files_only"];
        let parsed = Args::new(input, &known).and_then(|args| {
            let regex = RegexBuilder::new(args.str("pattern")?)
                .case_insensitive(args.opt_bool("case_insensitive")?.unwrap_or(false))
                .build()
                .map_err(|e| format!("invalid regular expression: {e}"))?;
            Ok(Search {
                regex,
                path: args.opt_str("path")?,
                filter: args.opt_str("glob")?.map(Pattern::parse).transpose()?,
                files_only: args.opt_bool("files_only")?.unwrap_or(false),
            })
        });
        match parsed {
            Ok(search) => search.run(ctx),
            Err(e) => Output::error(e),
        }
    }
}

struct Search<'a> {
    regex: Regex,
    path: Option<&'a str>,
    filter: Option<Pattern>,
    files_only: bool,
}

/// Why the search stopped before covering everything.
#[derive(Debug, Clone, Copy)]
enum Halt {
    OutputCap,
    ScanBytes,
    ScanEntries,
    Interrupted,
}

/// The matches found so far and what the search has spent and skipped.
#[derive(Default)]
struct Found {
    lines: String,
    read: u64,
    binary: usize,
    unreadable: Unreadable,
}

impl Found {
    fn push(&mut self, line: &str) -> Result<(), Halt> {
        if self.lines.len() + line.len() + 1 > MAX_GREP_OUTPUT {
            return Err(Halt::OutputCap);
        }
        self.lines.push_str(line);
        self.lines.push('\n');
        Ok(())
    }
}

impl Search<'_> {
    fn run(&self, ctx: Context<'_>) -> Output {
        let root = self
            .path
            .map_or_else(|| ctx.cwd.to_path_buf(), |p| ctx.cwd.join(p));
        let meta = match fs::metadata(&root) {
            Ok(meta) => meta,
            Err(e) => return Output::error(format!("cannot read {}: {e}", root.display())),
        };
        let mut found = Found::default();
        let halt = if meta.is_dir() {
            match self.directory(&root, ctx.interrupt, &mut found) {
                Ok(halt) => halt,
                Err(e) => return Output::error(e),
            }
        } else {
            let display = self
                .path
                .map_or_else(|| root.display().to_string(), str::to_owned);
            let name = root
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            if self.wanted(&[name]) {
                match self.file(&root, &display, ctx.interrupt, &mut found) {
                    Ok(scanned) => scanned.err(),
                    Err(e) => return Output::error(format!("cannot read {display}: {e}")),
                }
            } else {
                None
            }
        };
        render(found, halt)
    }

    /// Searches every wanted file under `root`; an `Err` only when `root` cannot be read.
    fn directory(
        &self,
        root: &Path,
        interrupt: &Interrupt,
        found: &mut Found,
    ) -> Result<Option<Halt>, String> {
        let mut halt = None;
        let walked = walk::walk(root, interrupt, |comps, path, kind| {
            match kind {
                Kind::Dir => {
                    return match &self.filter {
                        Some(f) if !f.is_name() && !f.may_match_below(comps) => Step::Prune,
                        _ => Step::Continue,
                    };
                }
                Kind::File if self.wanted(comps) => {}
                Kind::File | Kind::Symlink | Kind::Other => return Step::Continue,
            }
            let rel = comps.join("/");
            let display = match self.path {
                Some(prefix) => format!("{}/{rel}", prefix.trim_end_matches('/')),
                None => rel,
            };
            match self.file(path, &display, interrupt, found) {
                Ok(Ok(())) => Step::Continue,
                Ok(Err(stop)) => {
                    halt = Some(stop);
                    Step::Stop
                }
                Err(e) => {
                    found.unreadable.note(&display, &e);
                    Step::Continue
                }
            }
        })?;
        found.unreadable.absorb(walked.unreadable);
        Ok(halt.or(match walked.end {
            End::ScanLimit => Some(Halt::ScanEntries),
            End::Interrupted => Some(Halt::Interrupted),
            End::Complete | End::Stopped => None,
        }))
    }

    /// Whether the filter admits the file with these path components.
    fn wanted(&self, comps: &[String]) -> bool {
        match &self.filter {
            None => true,
            Some(filter) if filter.is_name() => filter.matches(&comps[comps.len() - 1..]),
            Some(filter) => filter.matches(comps),
        }
    }

    /// Scans one file; the outer `Err` is a read failure, the inner one ends the whole search.
    fn file(
        &self,
        path: &Path,
        display: &str,
        interrupt: &Interrupt,
        found: &mut Found,
    ) -> io::Result<Result<(), Halt>> {
        let mut reader = BufReader::with_capacity(BLOCK, File::open(path)?);
        if reader.fill_buf()?.contains(&0) {
            found.binary += 1;
            return Ok(Ok(()));
        }
        let mut bytes = Vec::new();
        let mut number = 0usize;
        loop {
            let left = MAX_SCAN_BYTES - found.read;
            bytes.clear();
            let n = (&mut reader).take(left + 1).read_until(b'\n', &mut bytes)? as u64;
            if n == 0 {
                return Ok(Ok(()));
            }
            if n > left {
                return Ok(Err(Halt::ScanBytes));
            }
            found.read += n;
            number += 1;
            if number.is_multiple_of(LINES_PER_CHECK) && interrupt.is_interrupted() {
                return Ok(Err(Halt::Interrupted));
            }
            let line = String::from_utf8_lossy(&bytes);
            let line = line.trim_end_matches(['\n', '\r']);
            if !self.regex.is_match(line) {
                continue;
            }
            if self.files_only {
                return Ok(found.push(display));
            }
            if let Err(halt) = found.push(&format!("{display}:{number}: {}", shorten(line))) {
                return Ok(Err(halt));
            }
        }
    }
}

/// The line cut to [`MAX_LINE`] bytes on a character boundary, with a marker.
fn shorten(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LINE {
        return Cow::Borrowed(line);
    }
    let cut = (0..=MAX_LINE)
        .rev()
        .find(|&i| line.is_char_boundary(i))
        .unwrap_or(0);
    Cow::Owned(format!(
        "{} [line truncated: {} more bytes]",
        &line[..cut],
        line.len() - cut
    ))
}

fn render(found: Found, halt: Option<Halt>) -> Output {
    let mut text = found.lines;
    if text.is_empty() {
        text.push_str("no matches\n");
    }
    if found.binary > 0 {
        let _ = writeln!(text, "[{} binary file(s) skipped]", found.binary);
    }
    if let Some(line) = found.unreadable.marker() {
        let _ = writeln!(text, "{line}");
    }
    let last = match halt {
        None => None,
        Some(Halt::OutputCap) => Some(format!(
            "[output truncated at {MAX_GREP_OUTPUT} bytes; narrow `pattern`, `path` or `glob`]"
        )),
        Some(Halt::ScanBytes) => Some(format!(
            "[scan stopped after {MAX_SCAN_BYTES} bytes read; narrow `path` or `glob`]"
        )),
        Some(Halt::ScanEntries) => Some(walk::scan_limit_marker()),
        Some(Halt::Interrupted) => {
            text.push_str("[interrupted]");
            return Output::error(text);
        }
    };
    if let Some(line) = last {
        text.push_str(&line);
    }
    text.truncate(text.trim_end_matches('\n').len());
    Output::ok(text)
}
