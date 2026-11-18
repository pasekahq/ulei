// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `hint` of a `terminal` result: one recovery hint for a failed command, keyed on the head
//! of its output or on its exit code, or a warning that an exit 0 likely masks a failure.

use std::sync::OnceLock;

use regex_lite::Regex;

use crate::tools::redact::SPACE;
use crate::tools::{basename, char_byte, is_py_space, re};

/// Characters of output the hints look at: error headers come early, deep output is noise.
const SCAN_CHARS: usize = 4000;

/// The output hints, first match wins.
const OUTPUT_HINTS: [fn(&Patterns, &str) -> Option<String>; 7] = [
    gh_unknown_json_field,
    merge_conflict,
    command_not_found,
    module_not_found,
    already_exists,
    gh_rate_limit,
    permission_denied,
];

/// Hints by exit code alone, looked at when no output hint matches.
const EXIT_CODE_HINTS: [(i64, &str); 3] = [
    (
        126,
        "Exit 126: the file was found but is not executable \u{2014} `chmod +x` it or invoke it \
         via its interpreter (e.g. `bash script.sh`).",
    ),
    (
        137,
        "Exit 137: the process was SIGKILLed \u{2014} usually out-of-memory or an external kill. \
         Reduce memory use or check `dmesg | tail` before retrying.",
    ),
    (
        124,
        "Exit 124: the command hit its timeout. Raise timeout= (foreground max 600s).",
    ),
];

/// Heads whose output legitimately carries failure text: searches, reads, echoed strings.
const READONLY_HEADS: [&str; 17] = [
    "grep",
    "rg",
    "ag",
    "find",
    "ls",
    "cat",
    "head",
    "tail",
    "jq",
    "awk",
    "sed",
    "strings",
    "zcat",
    "journalctl",
    "dmesg",
    "echo",
    "printf",
];

const MASKING_PIPE_NOTE: &str = "exit_code 0 here is the status of the last pipeline command \
     (tail/head/cat/...), NOT of the command before the pipe \u{2014} and the output contains \
     failure indicators. Treat this run as FAILED until proven otherwise: re-run the command \
     WITHOUT the pipe (output is auto-truncated and the full text is saved to a file, so piping \
     through tail/head is never needed) to get the real exit code.";

const MASKING_OR_NOTE: &str = "exit_code 0 here is the status of the `||` fallback (echo/true), \
     NOT of the command before it \u{2014} and the output contains failure indicators. Treat this \
     run as FAILED until proven otherwise: re-run the command bare to get its real exit code.";

struct Patterns {
    gh_unknown_json_field: Regex,
    command_not_found: Regex,
    module_not_found: Regex,
    merge_conflict: Regex,
    already_exists: Regex,
    /// A top-level pipe, not `||`, into a passthrough consumer in the last segment.
    masking_pipe: Regex,
    /// A `||` into a fallback that always succeeds.
    masking_or: Regex,
    /// Failure shapes of specific tools, so that output merely mentioning an error rarely
    /// matches.
    failure_shapes: Regex,
}

/// The compiled patterns, built by the first command that needs them.
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let s = SPACE;
        Patterns {
            gh_unknown_json_field: re(r#"Unknown JSON field: "?(\w+)"#),
            command_not_found: re(
                r"(?:bash: line \d+: |bash: |sh: \d*:? ?)?([\w.+-]+): command not found",
            ),
            module_not_found: re(r"(?:ModuleNotFoundError|ImportError): No module named '?([\w.]+)"),
            merge_conflict: re(r"(?m)^CONFLICT |Automatic merge failed|needs merge"),
            already_exists: re(r"(?:fatal|error):.*?'([^']+)' already exists"),
            masking_pipe: re(&format!(
                r"(?:^|[^|])\|[{s}]*(?:tail|head|cat|tee|less|more|wc|sort|uniq)\b[^|]*$"
            )),
            masking_or: re(&format!(r"\|\|[{s}]*(?:echo\b|printf\b|true\b|:[{s}]|:$)")),
            failure_shapes: re(&format!(
                r"error\[E\d+\]|error: could not compile|error: aborting due to|Traceback \(most recent call last\)|(?m:^(?:=+ )?\d+ failed)|(?m:^FAILED (?:[^{s}]+::|[^{s}]+\.py))|compilation terminated\.|npm ERR!|BUILD FAILED|Build FAILED|FAILED: |(?m:^make(?:\[\d+\])?: \*\*\*)"
            )),
        }
    })
}

/// The first `SCAN_CHARS` characters of `output`.
fn window(output: &str) -> &str {
    &output[..char_byte(output, SCAN_CHARS)]
}

/// One recovery hint for a command that exited with `exit_code`, none for exit 0.
pub(super) fn failure(exit_code: i64, output: &str) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    let window = window(output);
    if !window.is_empty() {
        let p = patterns();
        if let Some(hint) = OUTPUT_HINTS.iter().find_map(|hint| hint(p, window)) {
            return Some(hint);
        }
    }
    EXIT_CODE_HINTS
        .iter()
        .find(|(code, _)| *code == exit_code)
        .map(|(_, hint)| (*hint).to_owned())
}

/// A warning for an exit 0 of `command` whose shape can swallow an upstream failure and whose
/// output carries a failure shape; the exit code itself stays as it is.
pub(super) fn masked_success(command: &str, output: &str) -> Option<String> {
    let window = window(output);
    if command.is_empty() || window.is_empty() || READONLY_HEADS.contains(&first_token(command)) {
        return None;
    }
    let p = patterns();
    if !p.failure_shapes.is_match(window) {
        return None;
    }
    if p.masking_pipe.is_match(command) {
        return Some(MASKING_PIPE_NOTE.to_owned());
    }
    p.masking_or
        .is_match(command)
        .then(|| MASKING_OR_NOTE.to_owned())
}

/// The program name of the first word of `command`, leading `VAR=value` words skipped.
fn first_token(command: &str) -> &str {
    command
        .split(is_py_space)
        .filter(|tok| !tok.is_empty())
        .find(|tok| !tok.contains('=') || ["=", "./", "/"].iter().any(|p| tok.starts_with(p)))
        .map_or("", basename)
}

fn gh_unknown_json_field(p: &Patterns, output: &str) -> Option<String> {
    let field = p.gh_unknown_json_field.captures(output)?.get(1)?.as_str();
    Some(format!(
        "The installed gh does not support the JSON field '{field}'. The valid field list is \
         printed in the output above \u{2014} retry using only fields from that list."
    ))
}

fn command_not_found(p: &Patterns, output: &str) -> Option<String> {
    let missing = p.command_not_found.captures(output)?.get(1)?.as_str();
    Some(match missing {
        "python" => "This system has no bare `python` \u{2014} use `python3`, or the project \
                     venv's interpreter (e.g. .venv/bin/python)."
            .to_owned(),
        "pip" => "This system has no bare `pip` \u{2014} use `pip3`, `python3 -m pip`, or the \
                  project venv's pip (e.g. .venv/bin/pip)."
            .to_owned(),
        _ => format!(
            "`{missing}` is not installed or not on PATH. Verify with `which {missing}`; install \
             it or use an absolute path instead of retrying the same command."
        ),
    })
}

fn module_not_found(p: &Patterns, output: &str) -> Option<String> {
    let module = p.module_not_found.captures(output)?.get(1)?.as_str();
    Some(format!(
        "Python cannot import '{module}'. Most often the wrong interpreter is running: activate \
         the project venv (e.g. `source .venv/bin/activate`) or invoke its python directly. Only \
         pip install if the package is genuinely absent from that venv."
    ))
}

fn merge_conflict(p: &Patterns, output: &str) -> Option<String> {
    p.merge_conflict.is_match(output).then(|| {
        "Git merge conflict. Do not retry this command. Resolve the conflicted files listed \
         above (edit, then `git add`), then continue (`git rebase --continue` / commit the \
         merge) \u{2014} or abort with `--abort`."
            .to_owned()
    })
}

fn already_exists(p: &Patterns, output: &str) -> Option<String> {
    let name = p.already_exists.captures(output)?.get(1)?.as_str();
    Some(format!(
        "'{name}' already exists \u{2014} retrying unchanged will keep failing. Reuse it, choose \
         another name, or delete it first if it is genuinely stale."
    ))
}

fn gh_rate_limit(_: &Patterns, output: &str) -> Option<String> {
    (output.contains("API rate limit") || output.contains("was submitted too quickly")).then(|| {
        "GitHub API rate limit hit \u{2014} immediate retries will keep failing. Continue with \
         other work and retry this operation later."
            .to_owned()
    })
}

fn permission_denied(_: &Patterns, output: &str) -> Option<String> {
    (output.contains("Permission denied") || output.contains("EACCES")).then(|| {
        "Permission denied. Check ownership/mode of the target path (`ls -la`); prefer a \
         user-writable location. Only escalate to sudo if the task genuinely requires it."
            .to_owned()
    })
}
