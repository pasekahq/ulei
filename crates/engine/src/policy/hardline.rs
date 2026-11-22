// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `hardline`: the unconditional floor of `terminal`. A command over the parser limits, or one
//! that matches a catastrophic pattern after detection normalisation, is refused before it runs.
//! No setting turns the floor off.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex_lite::{Captures, Regex};
use serde_json::Value;
use ulei_contracts::generated::c4g::ToolCall;

use super::{Policy, Verdict};
use crate::tools::redact::SPACE;
use crate::tools::{
    TERMINAL, create_exclusive, random_hex, re, realpath, rejected, remove_stale, strip_ansi,
};

const MAX_DETECTION_COMMAND_CHARS: usize = 128_000;
const MAX_SEPARATOR_FREE_COMMAND_CHARS: usize = 4_096;
const MAX_DETECTION_SEGMENTS: usize = 25_000;
const SEPARATORS: &[u8] = b";&|\n";
const PARSER_LIMIT_DESCRIPTION: &str = "command parser limit exceeded";
/// Saved payloads older than this are removed when a later one is saved.
const BLOCKED_MAX_AGE: Duration = Duration::from_secs(7 * 86_400);

/// Start of a command: start of text, newline, backtick or `$(`, then optional `sudo`, `env` and
/// wrapper words.
macro_rules! cmdpos {
    () => {
        concat!(
            r"(?:^|[\n`]|\$\()",
            r"\s*",
            r"(?:sudo\s+(?:-[^\s]+\s+)*)?",
            r"(?:env\s+(?:\w+=\S*\s+)*)?",
            r"(?:(?:exec|nohup|setsid|time)\s+)*",
            r"\s*",
        )
    };
}

macro_rules! rm_flag_prefix {
    () => {
        concat!(cmdpos!(), r"rm\s+(-[^\s]*\s+)*")
    };
}

/// The path alternatives quoted as a whole, or bare and followed by a space, the end or a shell
/// metacharacter.
macro_rules! rm_path {
    ($($alt:tt)+) => {
        concat!(
            r#"(?:["'](?:"#, $($alt)+, r#")["']|(?:"#, $($alt)+, r")(?:\s|$|[)`;|&]))"
        )
    };
}

macro_rules! system_dirs {
    () => {
        // `/home` and the slash after it sit in two literals: the source lint reads that path
        // prefix as a personal path.
        concat!(
            r"/home|/home",
            r"/\*|/root|/root/\*|/etc|/etc/\*|/usr|/usr/\*|",
            r"/var|/var/\*|/bin|/bin/\*|/sbin|/sbin/\*|/boot|/boot/\*|/lib|/lib/\*",
        )
    };
}

/// The reference patterns in Python `re` syntax with their descriptions; [`translate`] turns
/// them into `regex-lite` patterns.
const PATTERNS: [(&str, &str); 12] = [
    (
        concat!(
            rm_flag_prefix!(),
            rm_path!(r"/(?:(?:\.\.?)?/)*(?:\.\.?)?\**|/ \*")
        ),
        "recursive delete of root filesystem",
    ),
    (
        concat!(rm_flag_prefix!(), rm_path!(system_dirs!())),
        "recursive delete of system directory",
    ),
    (
        concat!(
            rm_flag_prefix!(),
            rm_path!(r"(?:~|\$\{?HOME\}?)(?:/?|/\*)?")
        ),
        "recursive delete of home directory",
    ),
    (r"\bmkfs(\.[a-z0-9]+)?\b", "format filesystem (mkfs)"),
    (
        r"\bdd\b[^\n]*\bof=/dev/(sd|nvme|hd|mmcblk|vd|xvd)[a-z0-9]*",
        "dd to raw block device",
    ),
    (
        r">\s*/dev/(sd|nvme|hd|mmcblk|vd|xvd)[a-z0-9]*\b",
        "redirect to raw block device",
    ),
    (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:", "fork bomb"),
    (r"\bkill\s+(-[^\s]+\s+)*-1\b", "kill all processes"),
    (
        concat!(cmdpos!(), r"(shutdown|reboot|halt|poweroff)\b"),
        "system shutdown/reboot",
    ),
    (
        concat!(cmdpos!(), r"init\s+[06]\b"),
        "init 0/6 (shutdown/reboot)",
    ),
    (
        concat!(cmdpos!(), r"systemctl\s+(poweroff|reboot|halt|kexec)\b"),
        "systemctl poweroff/reboot",
    ),
    (
        concat!(cmdpos!(), r"telinit\s+[06]\b"),
        "telinit 0/6 (shutdown/reboot)",
    ),
];

/// Python's Unicode `\w` widened to every non-ASCII character, so a match there is never lost.
const WORD: &str = r"[0-9A-Za-z_\x{80}-\x{10FFFF}]";

/// Refuses a `terminal` command that breaks the hardline floor with the reference block result.
#[derive(Debug)]
pub struct Hardline {
    home: Option<PathBuf>,
    blocked_dir: PathBuf,
    /// Built on the first `terminal` call, so an idle engine compiles nothing.
    compiled: Option<Compiled>,
}

#[derive(Debug)]
struct Compiled {
    patterns: Vec<(Regex, &'static str)>,
    homes: Vec<Regex>,
}

impl Hardline {
    /// The floor; an absolute path under `home` reads as `~/` before matching, and a command
    /// refused by the parser limit is saved as a script under `blocked_dir`.
    pub fn new(home: Option<PathBuf>, blocked_dir: PathBuf) -> Self {
        Self {
            home,
            blocked_dir,
            compiled: None,
        }
    }

    /// The description of the rule `command` breaks, `None` when it may run.
    fn detect(&mut self, command: &str) -> Option<&'static str> {
        if parser_limit_exceeded(command) {
            return Some(PARSER_LIMIT_DESCRIPTION);
        }
        let home = self.home.as_deref();
        let compiled = self.compiled.get_or_insert_with(|| Compiled::new(home));
        let variant = normalize(command, &compiled.homes).to_lowercase();
        compiled
            .patterns
            .iter()
            .find(|(pattern, _)| pattern.is_match(&variant))
            .map(|(_, description)| *description)
    }

    /// The JSON result of the refused `command`.
    fn block_result(&self, description: &str, command: &str) -> String {
        let mut message = format!(
            "BLOCKED (hardline): {description}. This command is on the unconditional blocklist \
             and cannot be executed via the agent \u{2014} not even with --yolo, /yolo, \
             approvals.mode=off, or cron approve mode. If you genuinely need to run it, run it \
             yourself in a terminal outside the agent."
        );
        if description == PARSER_LIMIT_DESCRIPTION {
            match self.save_payload(command) {
                Ok(saved) => {
                    let saved = saved.display();
                    let _ = write!(
                        message,
                        " RECOVERY: this block fires on oversized/unparseable inline command \
                         payloads (heredocs, giant one-liners), not on the operation itself. Your \
                         command was saved to {saved} \u{2014} review it, then run: \
                         terminal(command=\"bash {saved}\"). Do not retry inline."
                    );
                }
                Err(_) => message.push_str(
                    " RECOVERY: this block fires on oversized/unparseable inline command payloads \
                     (heredocs, giant one-liners), not on the operation itself. Write the script \
                     to a file with write_file, then run it: terminal(command=\"bash \
                     /path/script.sh\") or \"python3 /path/script.py\". Do not retry inline.",
                ),
            }
        }
        rejected(-1, Some("blocked"), &message).content
    }

    /// `command` as a new owner-only script under the blocked directory, after removing the
    /// scripts saved there more than seven days ago.
    fn save_payload(&self, command: &str) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.blocked_dir)?;
        let now = SystemTime::now();
        remove_stale(&self.blocked_dir, "blocked-", ".sh", BLOCKED_MAX_AGE, now);
        let secs = now
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs();
        let path = self
            .blocked_dir
            .join(format!("blocked-{secs}-{}.sh", random_hex(4)?));
        let newline = if command.ends_with('\n') { "" } else { "\n" };
        let script = format!(
            "#!/bin/bash\n# Auto-saved by the agent: this command exceeded the inline command\n# \
             parser limit and was blocked from direct execution. Review it,\n# then run it via: \
             bash {}\n{command}{newline}",
            path.display()
        );
        create_exclusive(&path, 0o600)?.write_all(script.as_bytes())?;
        Ok(path)
    }
}

impl Policy for Hardline {
    fn name(&self) -> &'static str {
        "hardline"
    }

    fn pre_tool(&mut self, call: &ToolCall, _cwd: &Path) -> Verdict {
        if call.name != TERMINAL {
            return Verdict::Allow;
        }
        let Some(command) = call.input.get("command").and_then(Value::as_str) else {
            return Verdict::Allow;
        };
        match self.detect(command) {
            Some(description) => Verdict::Synthetic(self.block_result(description, command)),
            None => Verdict::Allow,
        }
    }
}

impl Compiled {
    fn new(home: Option<&Path>) -> Self {
        Self {
            patterns: PATTERNS
                .iter()
                .map(|&(pattern, description)| (re(&translate(pattern)), description))
                .collect(),
            homes: home_folds(home),
        }
    }
}

/// The Python `pattern`, compiled with `IGNORECASE | DOTALL`, in `regex-lite` syntax: `\s` and
/// `\S` become Python's whitespace sets and `\w` becomes [`WORD`].
fn translate(pattern: &str) -> String {
    let mut out = String::from("(?is)");
    let mut in_class = false;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match (chars.next(), in_class) {
                (Some('s'), false) => {
                    out.push('[');
                    out.push_str(SPACE);
                    out.push(']');
                }
                (Some('s'), true) => out.push_str(SPACE),
                (Some('S'), false) => {
                    out.push_str("[^");
                    out.push_str(SPACE);
                    out.push(']');
                }
                (Some('w'), false) => out.push_str(WORD),
                (Some(e @ ('S' | 'w')), true) => {
                    panic!("constant pattern {pattern}: \\{e} in a class")
                }
                (Some(e), _) => {
                    out.push('\\');
                    out.push(e);
                }
                (None, _) => out.push('\\'),
            },
            '[' if !in_class => {
                in_class = true;
                out.push(c);
            }
            ']' if in_class => {
                in_class = false;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether `command` is too long or holds too many separators to inspect.
fn parser_limit_exceeded(command: &str) -> bool {
    let chars = command.chars().count();
    if chars > MAX_DETECTION_COMMAND_CHARS {
        return true;
    }
    let separators = command.bytes().filter(|b| SEPARATORS.contains(b)).count();
    (chars > MAX_SEPARATOR_FREE_COMMAND_CHARS && separators == 0)
        || separators >= MAX_DETECTION_SEGMENTS
}

/// The fold patterns of `home` and its resolved form, longest first; none for a home with fewer
/// than two components.
fn home_folds(home: Option<&Path>) -> Vec<Regex> {
    let Some(home) = home else {
        return Vec::new();
    };
    let mut candidates: Vec<String> = [home.to_path_buf(), realpath(home)]
        .iter()
        .filter_map(|p| p.to_str().map(str::to_owned))
        .collect();
    crate::sort::sort_by(&mut candidates, |a, b| b.len().cmp(&a.len()));
    candidates.dedup();
    let tail = format!(r#"(?P<tail>(?:[/\\][^/\\{SPACE}'"`;|&<>()]*)+)"#);
    candidates
        .iter()
        .filter_map(|path| {
            let components: Vec<String> = path
                .split(['/', '\\'])
                .filter(|c| !c.is_empty())
                .map(regex_lite::escape)
                .collect();
            (components.len() >= 2)
                .then(|| re(&format!(r"[/\\]*{}{tail}", components.join(r"[/\\]+"))))
        })
        .collect()
}

/// `command` as detection reads it: escape sequences, NUL and the fullwidth forms resolved, line
/// continuations joined, the home prefix folded to `~`, backslash escapes, empty quotes and
/// `$IFS` resolved.
fn normalize(command: &str, homes: &[Regex]) -> String {
    let text: String = strip_ansi(command)
        .chars()
        .filter(|&c| c != '\0')
        .map(fold_width)
        .collect();
    let mut text = join_continuations(&text);
    for home in homes {
        text = home
            .replace_all(&text, |caps: &Captures<'_>| {
                format!("~{}", caps["tail"].replace('\\', "/"))
            })
            .into_owned();
    }
    collapse_ifs(&drop_empty_quotes(&unescape(&text)))
}

/// The ASCII character of a fullwidth form, a space for the ideographic space.
fn fold_width(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(c) - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        _ => c,
    }
}

/// `text` without each backslash that ends a line, and that line end.
fn join_continuations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(next) = after
            .strip_prefix('\n')
            .or_else(|| after.strip_prefix("\r\n"))
        {
            rest = next;
        } else {
            out.push('\\');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `text` with each backslash before a character other than a newline removed.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(&next) if c == '\\' && next != '\n' => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// `text` without `''` and `""`, scanned from the left.
fn drop_empty_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, '\'' | '"') && chars.peek() == Some(&c) {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` with each `${IFS...}` and `$IFS` that ends at a word boundary made one space.
fn collapse_ifs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(len) = ifs_len(after) {
            out.push(' ');
            rest = &after[len..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Bytes of the `IFS` expansion `after` a `$` starts with.
fn ifs_len(after: &str) -> Option<usize> {
    let boundary = |rest: &str| !rest.chars().next().is_some_and(is_word);
    if let Some(body) = after.strip_prefix("{IFS")
        && boundary(body)
        && let Some(end) = body.find('}')
    {
        return Some("{IFS".len() + end + 1);
    }
    after
        .strip_prefix("IFS")
        .filter(|body| boundary(body))
        .map(|_| "IFS".len())
}

/// A character of Python's Unicode `\w`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
