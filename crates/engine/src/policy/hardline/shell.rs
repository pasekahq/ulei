// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The shell reading of the hardline floor: the variants of a command the patterns run on. Real
//! command starts are marked, quoted newlines and quoted grep pattern operands are masked, command
//! words are deobfuscated and the programs of program-running options are surfaced. Nothing is
//! expanded or run. Offsets are byte offsets; every byte the scanners test is ASCII.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::tools::{basename, is_py_space, shlex};

/// Most `$(...)` and backtick substitutions open at once in one text.
const MAX_SUBSTITUTION_DEPTH: usize = 64;
/// Most words read at one command start: wrappers, their options and assignments, then the
/// command word.
const MAX_PREFIX_WORDS: usize = 12;
const COMMAND_WRAPPER_WORDS: [&str; 8] = [
    "sudo", "env", "exec", "nohup", "setsid", "time", "command", "builtin",
];
const SUDO_OPTIONS_WITH_ARG: [&str; 10] = [
    "-c",
    "--close-from",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-u",
    "--user",
];
const SHELLS: [&str; 4] = ["bash", "sh", "zsh", "ksh"];
const BASH_OPTIONS_WITH_ARG: [&str; 6] = ["-O", "+O", "-o", "+o", "--init-file", "--rcfile"];
const BASH_SHORT_OPTION_LETTERS: &str = "ilrsDcabefhkmnptuvxBCEHPTOo";
const GREP_OPTIONS_WITH_ARG: [&str; 14] = [
    "--after-context",
    "--before-context",
    "--binary-files",
    "--context",
    "--directories",
    "--devices",
    "--exclude",
    "--exclude-dir",
    "--exclude-from",
    "--include",
    "--label",
    "--max-count",
    "--regexp",
    "--file",
];
const GREP_SHORT_OPTIONS_WITH_ARG: &str = "ABCDdefm";

/// A read-only tool with an option that runs a program.
struct ReadTool {
    name: &'static str,
    exec_flags: &'static [&'static str],
    long_options_with_arg: &'static [&'static str],
    short_options_with_arg: &'static str,
}

const READ_TOOLS: [ReadTool; 4] = [
    ReadTool {
        name: "sort",
        exec_flags: &["--compress-program"],
        long_options_with_arg: &[
            "--batch-size",
            "--buffer-size",
            "--compress-program",
            "--field-separator",
            "--files0-from",
            "--key",
            "--output",
            "--parallel",
            "--random-source",
            "--sort",
            "--temporary-directory",
        ],
        short_options_with_arg: "koStT",
    },
    ReadTool {
        name: "rg",
        exec_flags: &["--pre", "--hostname-bin"],
        long_options_with_arg: &[
            "--after-context",
            "--before-context",
            "--color",
            "--colors",
            "--context",
            "--context-separator",
            "--dfa-size-limit",
            "--encoding",
            "--engine",
            "--field-context-separator",
            "--field-match-separator",
            "--file",
            "--generate",
            "--glob",
            "--hostname-bin",
            "--hyperlink-format",
            "--iglob",
            "--ignore-file",
            "--max-columns",
            "--max-count",
            "--max-depth",
            "--max-filesize",
            "--path-separator",
            "--pre",
            "--pre-glob",
            "--regex-size-limit",
            "--regexp",
            "--replace",
            "--sort",
            "--sortr",
            "--threads",
            "--type",
            "--type-add",
            "--type-clear",
            "--type-not",
        ],
        short_options_with_arg: "efEmjgdtTABCMr",
    },
    ReadTool {
        name: "ag",
        exec_flags: &["--pager"],
        long_options_with_arg: &[
            "--ackmate-dir-filter",
            "--color-line-number",
            "--color-match",
            "--color-path",
            "--depth",
            "--filename-pattern",
            "--file-search-regex",
            "--ignore",
            "--ignore-dir",
            "--max-count",
            "--pager",
            "--path-to-ignore",
            "--width",
            "--workers",
        ],
        short_options_with_arg: "gGmpW",
    },
    ReadTool {
        name: "man",
        exec_flags: &["--pager", "--html", "-P", "-H"],
        long_options_with_arg: &[
            "--config-file",
            "--encoding",
            "--extension",
            "--locale",
            "--manpath",
            "--pager",
            "--preprocessor",
            "--prompt",
            "--recode",
            "--sections",
            "--systems",
        ],
        short_options_with_arg: "CRLmMSserEPp",
    },
];

/// A text nests substitutions deeper than [`MAX_SUBSTITUTION_DEPTH`].
#[derive(Debug)]
pub(super) struct TooDeep;

/// A replacement of the bytes `start..end` of a text by `insert`.
pub(super) struct Splice {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) insert: String,
}

/// The first answer of `check` over the variants of `command`, in the reference order: the
/// normalised command with quoted newlines and quoted grep patterns masked, its form with
/// backslashes as slashes when it holds a drive or UNC path, the programs of program-running
/// options with their starts marked, and the masked command with its starts marked. Last comes
/// the answer of `check_splices` for the normalised command and the splices that deobfuscate
/// one command word each, which answers as `check` does for each spliced text in turn.
/// `normalize` is the detection normalisation.
pub(super) fn first_match(
    command: &str,
    normalize: &dyn Fn(&str) -> String,
    check: &dyn Fn(&str) -> Option<&'static str>,
    check_splices: &dyn Fn(&str, &[Splice]) -> Option<&'static str>,
) -> Result<Option<&'static str>, TooDeep> {
    let normalized = normalize(&mask_quoted_newlines(command));
    let grep_safe = grep_safe_variant(&normalized)?.0.into_owned();
    if let Some(found) = check(&grep_safe) {
        return Ok(Some(found));
    }
    let mut seen = HashSet::from([grep_safe.clone()]);
    let try_new = |variant: String, seen: &mut HashSet<String>| {
        if seen.contains(&variant) {
            return None;
        }
        let found = check(&variant);
        seen.insert(variant);
        found
    };
    if has_windows_path(command) {
        let flattened = normalize(&mask_quoted_newlines(&command.replace('\\', "/")));
        if let Some(found) = try_new(flattened, &mut seen) {
            return Ok(Some(found));
        }
    }
    let mut pending = vec![normalized.clone()];
    while let Some(variant) = pending.pop() {
        for payload in program_payloads(&variant)? {
            if seen.contains(&payload) {
                continue;
            }
            let marked = mark_command_starts(&payload)?.into_owned();
            if let Some(found) = try_new(payload.clone(), &mut seen) {
                return Ok(Some(found));
            }
            if marked != payload
                && let Some(found) = try_new(marked, &mut seen)
            {
                return Ok(Some(found));
            }
            pending.push(payload);
        }
    }
    let marked = mark_command_starts(&grep_safe)?;
    if marked != grep_safe
        && let Some(found) = try_new(marked.into_owned(), &mut seen)
    {
        return Ok(Some(found));
    }
    let splices: Vec<Splice> = command_word_spans(&normalized)?
        .into_iter()
        .filter(|(start, end, deobfuscated)| {
            !deobfuscated.is_empty() && *deobfuscated != normalized[*start..*end]
        })
        .map(|(start, end, insert)| Splice { start, end, insert })
        .collect();
    Ok(check_splices(&normalized, &splices))
}

/// `command` with each structurally found quoted grep PCRE pattern operand blanked, and whether
/// the grep parse was malformed or ambiguous; a malformed parse blanks nothing.
pub(super) fn grep_safe_variant(command: &str) -> Result<(Cow<'_, str>, bool), TooDeep> {
    let (spans, malformed) = quoted_grep_pattern_spans(command)?;
    if malformed || spans.is_empty() {
        return Ok((Cow::Borrowed(command), malformed));
    }
    let mut out = String::with_capacity(command.len());
    let mut previous = 0;
    for (start, end) in spans {
        if start > previous {
            out.push_str(&command[previous..start]);
        }
        out.extend(std::iter::repeat_n(
            ' ',
            command[start..end].chars().count(),
        ));
        previous = end;
    }
    out.push_str(&command[previous..]);
    Ok((Cow::Owned(out), false))
}

/// Whether the raw `command` holds a drive path `X:\` or a UNC prefix with a third backslash.
fn has_windows_path(command: &str) -> bool {
    command.contains(r"\\\")
        || command
            .as_bytes()
            .windows(3)
            .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\')
}

/// The byte after the character at `i` of `b`.
fn next(b: &[u8], i: usize) -> usize {
    i + match b[i] {
        0x00..=0x7F => 1,
        0x80..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// The first offset from `pos` that is not whitespace.
fn skip_whitespace(text: &str, pos: usize) -> usize {
    text[pos..]
        .find(|c: char| !is_py_space(c))
        .map_or(text.len(), |at| pos + at)
}

/// The ends of the `$(...)` substitutions of one text, each scanned for once: the scan of one
/// substitution passes each one it nests in the state that one's own scan starts in.
struct ParenEnds<'t> {
    text: &'t str,
    ends: HashMap<usize, Option<usize>>,
}

impl<'t> ParenEnds<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            ends: HashMap::new(),
        }
    }

    /// The offset after the balanced `$(...)` starting at `start`.
    fn end(&mut self, start: usize) -> Option<usize> {
        if let Some(&end) = self.ends.get(&start) {
            return end;
        }
        let b = self.text.as_bytes();
        let mut open = vec![start];
        let mut quote = None;
        let mut i = start + 2;
        while i < b.len() {
            let ch = b[i];
            if let Some(q) = quote {
                if ch == b'\\' && q == b'"' && i + 1 < b.len() {
                    i = next(b, i + 1);
                    continue;
                }
                if ch == q {
                    quote = None;
                }
                i = next(b, i);
                continue;
            }
            match ch {
                b'\'' | b'"' => {
                    quote = Some(ch);
                    i += 1;
                }
                b'\\' if i + 1 < b.len() => i = next(b, i + 1),
                b'$' if b.get(i + 1) == Some(&b'(') => {
                    open.push(i);
                    i += 2;
                }
                b')' => {
                    i += 1;
                    if let Some(opened) = open.pop() {
                        self.ends.insert(opened, Some(i));
                    }
                    if open.is_empty() {
                        return Some(i);
                    }
                }
                _ => i = next(b, i),
            }
        }
        for opened in open {
            self.ends.insert(opened, None);
        }
        None
    }
}

/// The offset after the backtick closing the one at `start`.
fn backtick_end(command: &str, start: usize) -> Option<usize> {
    let b = command.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() => i = next(b, i + 1),
            b'`' => return Some(i + 1),
            _ => i = next(b, i),
        }
    }
    None
}

/// The span of the shell word after the whitespace at `pos`, substitutions and quotes kept
/// whole; empty at the end or before a separator.
fn read_shell_word(ends: &mut ParenEnds<'_>, pos: usize) -> (usize, usize) {
    let command = ends.text;
    let b = command.as_bytes();
    let start = skip_whitespace(command, pos);
    let mut i = start;
    let mut quote = None;
    while i < b.len() {
        let ch = b[i];
        if let Some(q) = quote {
            if ch == b'\\' && q == b'"' && i + 1 < b.len() {
                i = next(b, i + 1);
                continue;
            }
            if ch == q {
                quote = None;
            }
            i = next(b, i);
            continue;
        }
        match ch {
            b'\'' | b'"' => {
                quote = Some(ch);
                i += 1;
            }
            b'\\' if i + 1 < b.len() => i = next(b, i + 1),
            b'$' if b.get(i + 1) == Some(&b'(') => {
                i = ends.end(i).unwrap_or(i + 2);
            }
            b'$' if b.get(i + 1) == Some(&b'{') => {
                i = command[i + 2..]
                    .find('}')
                    .map_or(i + 2, |at| i + 2 + at + 1);
            }
            b'`' => i = backtick_end(command, i).unwrap_or(i + 1),
            b';' | b'&' | b'|' | b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | 0x1C..=0x1F => break,
            0x00..=0x7F => i += 1,
            _ => {
                if command[i..].starts_with(is_py_space) {
                    break;
                }
                i = next(b, i);
            }
        }
    }
    (start, i)
}

/// `word` without its quotes and backslash escapes.
fn strip_shell_word_syntax(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut quote = None;
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == '\\' && q == '"' && chars.peek().is_some() {
                out.extend(chars.next());
            } else if ch == q {
                quote = None;
            } else {
                out.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' if chars.peek().is_some() => out.extend(chars.next()),
            _ => out.push(ch),
        }
    }
    out
}

/// Whether `value` is a non-empty run of ASCII letters, digits and `_./:@%+=,-`.
fn is_simple_shell_literal(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c))
}

/// The output of a tiny literal `echo` or `printf` substitution body, without running it.
fn literal_command_substitution_output(script: &str) -> Option<String> {
    let tokens = shlex::split(script, "", false)?;
    let (command, mut args) = tokens.split_first()?;
    let simple = |arg: &String| is_simple_shell_literal(arg).then(|| arg.clone());
    match command.to_lowercase().as_str() {
        "echo" => {
            let echo_flags = |arg: &String| {
                arg.strip_prefix('-')
                    .is_some_and(|f| !f.is_empty() && f.chars().all(|c| "nEe".contains(c)))
            };
            while let Some((first, rest)) = args.split_first()
                && echo_flags(first)
            {
                args = rest;
            }
            match args {
                [only] => simple(only),
                _ => None,
            }
        }
        "printf" => match args {
            [only] => simple(only),
            [format, value] if format == "%s" => simple(value),
            _ => None,
        },
        _ => None,
    }
}

/// `word` with each `$(...)` and backtick substitution of a literal `echo` or `printf` made its
/// output.
fn replace_simple_command_substitutions(word: &str) -> String {
    let b = word.as_bytes();
    let mut ends = ParenEnds::new(word);
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < b.len() {
        let substitution = match b[i] {
            b'$' if b.get(i + 1) == Some(&b'(') => ends.end(i).map(|end| (i + 2, end)),
            b'`' => backtick_end(word, i).map(|end| (i + 1, end)),
            _ => None,
        };
        if let Some((body, end)) = substitution
            && let Some(output) = literal_command_substitution_output(&word[body..end - 1])
        {
            out.push_str(&output);
            i = end;
            continue;
        }
        let from = next(b, i);
        let after = word[from..]
            .find(['$', '`'])
            .map_or(word.len(), |at| from + at);
        out.push_str(&word[i..after]);
        i = after;
    }
    out
}

/// `text` with each `${...}` whose body `expand` reads made the text it returns; `expand` gets
/// the text after `${` and answers the bytes it took through the closing brace.
fn replace_parameter_expansions(
    text: &str,
    expand: impl Fn(&str) -> Option<(usize, &str)>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let body = &rest[at + 2..];
        if let Some((taken, replacement)) = expand(body) {
            out.push_str(replacement);
            rest = &body[taken..];
        } else {
            out.push('$');
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// The length of the run at the start of `text` of characters `keep` accepts.
fn run(text: &str, keep: impl Fn(char) -> bool) -> usize {
    text.find(|c: char| !keep(c)).unwrap_or(text.len())
}

/// The body of `${name/pattern/replacement}` read as its replacement: a name without `}`, `/`
/// or whitespace, a pattern without `}` or `/`, a replacement without `}`.
fn parameter_replacement(body: &str) -> Option<(usize, &str)> {
    let name = run(body, |c| c != '}' && c != '/' && !is_py_space(c));
    let rest = body[name..].strip_prefix('/').filter(|_| name > 0)?;
    let pattern = run(rest, |c| c != '}' && c != '/');
    let replacement = rest[pattern..].strip_prefix('/')?;
    let end = replacement.find('}')?;
    let taken = body.len() - replacement.len() + end + 1;
    Some((taken, &replacement[..end]))
}

/// The body of `${name:-default}` read as its default: a name without `}`, `:` or whitespace,
/// a default without `}`.
fn parameter_default(body: &str) -> Option<(usize, &str)> {
    let name = run(body, |c| c != '}' && c != ':' && !is_py_space(c));
    let default = body[name..].strip_prefix(":-").filter(|_| name > 0)?;
    let end = default.find('}')?;
    let taken = body.len() - default.len() + end + 1;
    Some((taken, &default[..end]))
}

/// `word` with its literal substitutions and its parameter replacements and defaults resolved.
fn replace_simple_shell_expansions(word: &str) -> String {
    let word = replace_simple_command_substitutions(word);
    let word = replace_parameter_expansions(&word, parameter_replacement);
    replace_parameter_expansions(&word, parameter_default)
}

/// `word` as the shell may spell a command word: literal substitutions and parameter forms
/// resolved and quoting removed, twice at most.
fn deobfuscate(word: &str) -> String {
    let mut current = word.to_owned();
    for _ in 0..2 {
        let next = strip_shell_word_syntax(&replace_simple_shell_expansions(&current));
        if next == current {
            break;
        }
        current = next;
    }
    current
}

/// Whether `word` is a `NAME=value` assignment with a value on one line.
fn is_env_assignment(word: &str) -> bool {
    let Some((name, value)) = word.split_once('=') else {
        return false;
    };
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.contains('\n')
}

/// A scan of [`command_starts`]: its position, its end and its open quote.
struct Frame {
    at: usize,
    end: usize,
    quote: Option<u8>,
}

/// The offsets where `command` starts a command, whitespace skipped, each once, in scan order:
/// the start, after `;`, `&`, `&&`, `|`, `||`, a newline, `(` or `{` outside quotes, and inside
/// every `$(...)` and backtick substitution outside single quotes.
fn command_starts(ends: &mut ParenEnds<'_>) -> Result<Vec<usize>, TooDeep> {
    let command = ends.text;
    let b = command.as_bytes();
    let mut starts = vec![0];
    // An explicit stack: substitutions may nest deeper than the thread's stack allows.
    let mut frames = vec![Frame {
        at: 0,
        end: b.len(),
        quote: None,
    }];
    loop {
        let depth = frames.len();
        let Some(frame) = frames.last_mut() else {
            break;
        };
        let (i, end) = (frame.at, frame.end);
        if i >= end {
            frames.pop();
            continue;
        }
        let ch = b[i];
        let substitution = match (frame.quote, ch) {
            (Some(b'\''), _) => {
                if ch == b'\'' {
                    frame.quote = None;
                }
                frame.at = next(b, i);
                None
            }
            (Some(_) | None, b'\\') if i + 1 < end => {
                frame.at = next(b, i + 1);
                None
            }
            (Some(_), b'"') => {
                frame.quote = None;
                frame.at = i + 1;
                None
            }
            (_, b'$') if b.get(i + 1) == Some(&b'(') => Some((i + 2, ends.end(i))),
            (_, b'`') => Some((i + 1, backtick_end(command, i))),
            (None, b'\'' | b'"') => {
                frame.quote = Some(ch);
                frame.at = i + 1;
                None
            }
            (None, b'(' | b'{' | b';' | b'\n') => {
                starts.push(i + 1);
                frame.at = i + 1;
                None
            }
            (None, b'&' | b'|') => {
                let repeated = i + 1 < end && b[i + 1] == ch;
                let after = if repeated { i + 2 } else { i + 1 };
                starts.push(after);
                frame.at = after;
                None
            }
            _ => {
                frame.at = next(b, i);
                None
            }
        };
        if let Some((inner, nested_end)) = substitution {
            if depth > MAX_SUBSTITUTION_DEPTH {
                return Err(TooDeep);
            }
            starts.push(inner);
            frame.at = nested_end.unwrap_or(end);
            frames.push(Frame {
                at: inner,
                end: nested_end.map_or(end, |e| e - 1),
                quote: None,
            });
        }
    }
    let mut seen = HashSet::new();
    Ok(starts
        .into_iter()
        .map(|start| skip_whitespace(command, start))
        .filter(|&start| start < b.len() && seen.insert(start))
        .collect())
}

/// `command` with a newline before each command start after the first character.
fn mark_command_starts(command: &str) -> Result<Cow<'_, str>, TooDeep> {
    let mut offsets: Vec<usize> = command_starts(&mut ParenEnds::new(command))?
        .into_iter()
        .filter(|&o| o > 0)
        .collect();
    if offsets.is_empty() {
        return Ok(Cow::Borrowed(command));
    }
    offsets.sort_unstable();
    let mut out = String::with_capacity(command.len() + offsets.len());
    let mut previous = 0;
    for offset in offsets {
        out.push_str(&command[previous..offset]);
        out.push('\n');
        previous = offset;
    }
    out.push_str(&command[previous..]);
    Ok(Cow::Owned(out))
}

/// `command` with each newline inside single or double quotes made a space; an unclosed quote
/// runs to the end.
fn mask_quoted_newlines(command: &str) -> Cow<'_, str> {
    if !command.contains('\n') {
        return Cow::Borrowed(command);
    }
    let b = command.as_bytes();
    let mut out = String::with_capacity(command.len());
    let mut quote = None;
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        let escape = ch == b'\\' && quote != Some(b'\'') && i + 1 < b.len();
        let after = if escape { next(b, i + 1) } else { next(b, i) };
        match quote {
            Some(b'"') if escape => {}
            Some(q) => {
                if ch == q {
                    quote = None;
                }
                if ch == b'\n' {
                    out.push(' ');
                    i = after;
                    continue;
                }
            }
            None if ch == b'\'' || ch == b'"' => quote = Some(ch),
            None => {}
        }
        out.push_str(&command[i..after]);
        i = after;
    }
    Cow::Owned(out)
}

/// The command words of `command` with their spans and deobfuscated forms: at each command
/// start, every wrapper word and assignment and then the word they lead to; the options of
/// `sudo` and `env` and the arguments of `sudo`'s argument options are passed over.
fn command_word_spans(command: &str) -> Result<Vec<(usize, usize, String)>, TooDeep> {
    let mut ends = ParenEnds::new(command);
    let mut spans = Vec::new();
    for start in command_starts(&mut ends)? {
        let mut pos = start;
        let mut skip_wrapper_options = false;
        let mut skip_next_wrapper_arg = false;
        for _ in 0..MAX_PREFIX_WORDS {
            let (word_start, word_end) = read_shell_word(&mut ends, pos);
            if word_start == word_end {
                break;
            }
            pos = word_end;
            let deobfuscated = deobfuscate(&command[word_start..word_end]);
            let lower = deobfuscated.to_lowercase();
            if skip_next_wrapper_arg {
                skip_next_wrapper_arg = false;
                continue;
            }
            if skip_wrapper_options && lower.starts_with('-') {
                skip_next_wrapper_arg =
                    !lower.contains('=') && SUDO_OPTIONS_WITH_ARG.contains(&lower.as_str());
                continue;
            }
            let wrapper = COMMAND_WRAPPER_WORDS.contains(&lower.as_str());
            let assignment = is_env_assignment(&deobfuscated);
            spans.push((word_start, word_end, deobfuscated));
            if wrapper {
                skip_wrapper_options = lower == "sudo" || lower == "env";
            } else if assignment {
                skip_wrapper_options = false;
            } else {
                break;
            }
        }
    }
    Ok(spans)
}

/// The top-level command segments of `command` with their offsets, split at `;`, `&`, `&&`,
/// `|`, `||` and newlines outside quotes and escapes.
fn top_level_segments(command: &str) -> Vec<(usize, &str)> {
    let b = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if escaped {
            escaped = false;
        } else if ch == b'\\' && quote != Some(b'\'') {
            escaped = true;
        } else if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == b'\'' || ch == b'"' {
            quote = Some(ch);
        } else if matches!(ch, b';' | b'&' | b'|' | b'\n') {
            if start < i {
                segments.push((start, &command[start..i]));
            }
            if matches!(ch, b'&' | b'|') && b.get(i + 1) == Some(&ch) {
                i += 1;
            }
            start = i + 1;
        }
        i = next(b, i);
    }
    if start < b.len() {
        segments.push((start, &command[start..]));
    }
    segments
}

/// A shell word with its source span and whether it is wholly single-quoted data.
struct Token {
    value: String,
    start: usize,
    end: usize,
    inert: bool,
}

/// The shell words of `segment` from `start`, none expanded; `None` on an unclosed quote or a
/// trailing backslash.
fn tokens_with_spans(segment: &str, start: usize) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut i = start;
    loop {
        i = skip_whitespace(segment, i);
        if i >= segment.len() {
            break;
        }
        let token_start = i;
        let mut value = String::new();
        let mut quote = None;
        let mut chars = segment[i..].chars().peekable();
        while let Some(ch) = chars.next() {
            if quote.is_none() && is_py_space(ch) {
                break;
            }
            i += ch.len_utf8();
            match quote {
                Some(q) if ch == q => quote = None,
                Some('"') if ch == '\\' && chars.peek().is_some() => {
                    let escaped = chars.next()?;
                    i += escaped.len_utf8();
                    value.push(escaped);
                }
                None if ch == '\'' || ch == '"' => quote = Some(ch),
                None if ch == '\\' => {
                    let escaped = chars.next()?;
                    i += escaped.len_utf8();
                    value.push(escaped);
                }
                _ => value.push(ch),
            }
        }
        if quote.is_some() {
            return None;
        }
        let raw = &segment[token_start..i];
        let inert = raw.ends_with('\'') && (raw.starts_with('\'') || raw.contains("='"));
        tokens.push(Token {
            value,
            start: token_start,
            end: i,
            inert,
        });
    }
    Some(tokens)
}

/// The spans of the quoted PCRE pattern operands of every grep in `command`, and whether a grep
/// parse was malformed or ambiguous, in which case no span is given.
fn quoted_grep_pattern_spans(command: &str) -> Result<(Vec<(usize, usize)>, bool), TooDeep> {
    let mut spans = Vec::new();
    for (segment_at, segment) in top_level_segments(command) {
        for (start, _, word) in command_word_spans(segment)? {
            if !matches!(basename(&word).to_lowercase().as_str(), "grep" | "egrep") {
                continue;
            }
            let Some(tokens) = tokens_with_spans(segment, start) else {
                return Ok((Vec::new(), true));
            };
            let args = tokens.get(1..).unwrap_or_default();
            let (pcre, pattern_indexes) = match grep_patterns(args) {
                None => return Ok((Vec::new(), true)),
                Some(GrepPatterns::NoOperand { pcre }) => return Ok((Vec::new(), pcre)),
                Some(GrepPatterns::Found {
                    pcre,
                    pattern_indexes,
                }) => (pcre, pattern_indexes),
            };
            if pcre {
                spans.extend(
                    pattern_indexes
                        .into_iter()
                        .map(|index| &args[index])
                        .filter(|token| token.inert)
                        .map(|token| (segment_at + token.start, segment_at + token.end)),
                );
            }
        }
    }
    Ok((spans, false))
}

/// The pattern operands of one grep's arguments.
enum GrepPatterns {
    /// The indexes of its pattern operands and whether it reads them as PCRE.
    Found {
        pcre: bool,
        pattern_indexes: Vec<usize>,
    },
    /// No `-e`, `-f` or operand names a pattern.
    NoOperand { pcre: bool },
}

/// The pattern operands of the grep arguments `args`; `None` when an option lacks its argument.
fn grep_patterns(args: &[Token]) -> Option<GrepPatterns> {
    let mut pcre = false;
    let mut explicit_patterns = false;
    let mut pattern_indexes = Vec::new();
    let mut operand_index = None;
    let mut options = true;
    let mut i = 0;
    while i < args.len() {
        let token = args[i].value.as_str();
        if options && token == "--" {
            options = false;
            i += 1;
            continue;
        }
        if options && token.starts_with("--") {
            let (option, equals) = token
                .split_once('=')
                .map_or((token, false), |(o, _)| (o, true));
            pcre |= option == "--perl-regexp";
            explicit_patterns |= option == "--regexp" || option == "--file";
            if GREP_OPTIONS_WITH_ARG.contains(&option) && !equals {
                if i + 1 >= args.len() {
                    return None;
                }
                if option == "--regexp" {
                    pattern_indexes.push(i + 1);
                }
                i += 2;
                continue;
            }
            if option == "--regexp" && equals {
                pattern_indexes.push(i);
            }
            i += 1;
            continue;
        }
        if options && token.starts_with('-') && token != "-" {
            let letters: Vec<char> = token[1..].chars().collect();
            for (j, &letter) in letters.iter().enumerate() {
                pcre |= letter == 'P';
                explicit_patterns |= letter == 'e' || letter == 'f';
                if GREP_SHORT_OPTIONS_WITH_ARG.contains(letter) {
                    if j + 1 < letters.len() {
                        if letter == 'e' {
                            pattern_indexes.push(i);
                        }
                    } else {
                        if i + 1 >= args.len() {
                            return None;
                        }
                        if letter == 'e' {
                            pattern_indexes.push(i + 1);
                        }
                        i += 1;
                    }
                    break;
                }
            }
            i += 1;
            continue;
        }
        operand_index.get_or_insert(i);
        i += 1;
    }
    if !explicit_patterns {
        let Some(operand) = operand_index else {
            return Some(GrepPatterns::NoOperand { pcre });
        };
        pattern_indexes.push(operand);
    }
    Some(GrepPatterns::Found {
        pcre,
        pattern_indexes,
    })
}

/// The programs `command` hands to `bash -c` and its kin or to a read-only tool's
/// program-running option, in order.
fn program_payloads(command: &str) -> Result<Vec<String>, TooDeep> {
    let mut payloads = Vec::new();
    for (_, segment) in top_level_segments(command) {
        for (start, _, word) in command_word_spans(segment)? {
            let name = basename(&word).to_lowercase();
            let shell = SHELLS.contains(&name.as_str());
            let tool = READ_TOOLS.iter().find(|tool| tool.name == name);
            if !shell && tool.is_none() {
                continue;
            }
            let Some(tokens) = shlex::split(&segment[start..], "<>", false) else {
                continue;
            };
            let Some((_, args)) = tokens.split_first() else {
                continue;
            };
            if shell {
                payloads.extend(bash_exec_payload(args));
            }
            if let Some(tool) = tool {
                payloads.extend(read_tool_exec_payload(tool, args));
            }
        }
    }
    payloads.retain(|payload| !payload.is_empty());
    Ok(payloads)
}

/// The command string a Bash-like shell runs with `-c` among its arguments `args`.
fn bash_exec_payload(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let token = args[index].as_str();
        if token == "--" || !token.starts_with(['-', '+']) {
            break;
        }
        if BASH_OPTIONS_WITH_ARG.contains(&token) {
            index += 2;
            continue;
        }
        if token.starts_with("--") {
            index += 1;
            continue;
        }
        let letters = &token[1..];
        if !letters
            .chars()
            .all(|c| BASH_SHORT_OPTION_LETTERS.contains(c))
        {
            index += 1;
            continue;
        }
        let option_arg = usize::from(letters.contains(['O', 'o']));
        if !letters.contains('c') {
            index += 1 + option_arg;
            continue;
        }
        return args.get(index + 1 + option_arg).cloned();
    }
    None
}

/// The program `tool` runs through its program-running option among its arguments `args`.
fn read_tool_exec_payload(tool: &ReadTool, args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let token = args[index].as_str();
        if token == "--" {
            break;
        }
        let (option, mut payload) = token
            .split_once('=')
            .map_or((token, None), |(option, value)| (option, Some(value)));
        let mut matched = tool.exec_flags.contains(&option);
        if tool.name == "man"
            && (token.starts_with("-P") || token.starts_with("-H"))
            && token.chars().count() > 2
        {
            matched = true;
            payload = Some(&token[2..]);
        }
        if matched {
            if payload.is_none() {
                payload = args.get(index + 1).map(String::as_str);
            }
            if let Some(program) = payload.filter(|p| !p.is_empty()) {
                return Some(program.to_owned());
            }
            index += if payload.is_some() && !token.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        if tool.long_options_with_arg.contains(&option) && payload.is_none() {
            index += 2;
            continue;
        }
        if token.starts_with('-') && !token.starts_with("--") && token.chars().count() > 1 {
            let last = token.chars().count() - 1;
            index += match token
                .chars()
                .skip(1)
                .position(|c| tool.short_options_with_arg.contains(c))
            {
                Some(at) if at + 1 == last => 2,
                _ => 1,
            };
            continue;
        }
        index += 1;
    }
    None
}
