// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `exit_code_meaning` note of a non-zero exit: signal deaths in both encodings and the
//! codes common commands use for an outcome that is not an error.

/// Notes of the lethal signals seen in practice, by number; 2 is absent since an interrupt
/// has its own marker and code.
const SIGNAL_NOTES: [(i64, &str); 11] = [
    (3, "SIGQUIT (quit from keyboard)"),
    (
        4,
        "SIGILL (illegal instruction \u{2014} corrupt binary or wrong architecture)",
    ),
    (
        6,
        "SIGABRT (abort \u{2014} assertion failure, fatal runtime error, or glibc abort)",
    ),
    (
        7,
        "SIGBUS (bus error \u{2014} misaligned or unmapped memory access)",
    ),
    (
        8,
        "SIGFPE (fatal arithmetic error, e.g. integer division by zero)",
    ),
    (
        9,
        "SIGKILL \u{2014} often the kernel OOM killer on memory exhaustion, or an explicit kill -9",
    ),
    (
        11,
        "SIGSEGV (segmentation fault \u{2014} the program crashed)",
    ),
    (
        13,
        "SIGPIPE (wrote to a closed pipe \u{2014} e.g. output piped to a reader that exited)",
    ),
    (
        15,
        "SIGTERM (terminated \u{2014} kill/timeout or shutdown requested it to stop)",
    ),
    (24, "SIGXCPU (CPU time limit exceeded)"),
    (25, "SIGXFSZ (file size limit exceeded)"),
];

/// Linux signal names by number, as the model's training shows them.
const SIGNAL_NAMES: [&str; 31] = [
    "SIGHUP",
    "SIGINT",
    "SIGQUIT",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGKILL",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGTERM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];

const NO_MATCHES: &str = "No matches found (not an error)";
const FILES_DIFFER: &str = "Files differ (expected, not an error)";
const CONDITION_FALSE: &str = "Condition evaluated to false (expected, not an error)";

/// Codes of the last command of a chain that mean something other than a failure.
const SEMANTICS: [(&str, &[(i64, &str)]); 12] = [
    ("grep", &[(1, NO_MATCHES)]),
    ("egrep", &[(1, NO_MATCHES)]),
    ("fgrep", &[(1, NO_MATCHES)]),
    ("rg", &[(1, NO_MATCHES)]),
    ("ag", &[(1, NO_MATCHES)]),
    ("ack", &[(1, NO_MATCHES)]),
    ("diff", &[(1, FILES_DIFFER)]),
    ("colordiff", &[(1, FILES_DIFFER)]),
    (
        "find",
        &[(
            1,
            "Some directories were inaccessible (partial results may still be valid)",
        )],
    ),
    ("test", &[(1, CONDITION_FALSE)]),
    ("[", &[(1, CONDITION_FALSE)]),
    (
        "curl",
        &[
            (6, "Could not resolve host"),
            (7, "Failed to connect to host"),
            (22, "HTTP response code indicated error (e.g. 404, 500)"),
            (28, "Operation timed out"),
        ],
    ),
];

const GIT: &str =
    "Non-zero exit (often normal \u{2014} e.g. 'git diff' returns 1 when files differ)";

/// The note for `exit_code` of `command`, if any; a negative code is a death by that signal.
pub(super) fn interpret(command: &str, exit_code: i64) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    if let Some(note) = signal_note(exit_code) {
        return Some(note);
    }
    let base = base_command(command)?;
    if base == "git" {
        return (exit_code == 1).then(|| GIT.to_owned());
    }
    SEMANTICS
        .iter()
        .find(|(name, _)| *name == base)
        .and_then(|(_, codes)| codes.iter().find(|(code, _)| *code == exit_code))
        .map(|(_, note)| (*note).to_owned())
}

/// A death by signal: certain for a negative code, likely for one above 128.
fn signal_note(exit_code: i64) -> Option<String> {
    let note = |signum: i64| {
        SIGNAL_NOTES
            .iter()
            .find(|(n, _)| *n == signum)
            .map(|(_, note)| *note)
    };
    if exit_code < 0 {
        let signum = -exit_code;
        if signum == 2 {
            return None;
        }
        if let Some(note) = note(signum) {
            return Some(format!("Command terminated by signal {signum}: {note}"));
        }
        let name = match signum {
            1..=31 => usize::try_from(signum - 1)
                .ok()
                .and_then(|i| SIGNAL_NAMES.get(i))
                .map_or_else(|| format!("signal {signum}"), |n| (*n).to_owned()),
            34 => "SIGRTMIN".into(),
            64 => "SIGRTMAX".into(),
            _ => format!("signal {signum}"),
        };
        return Some(format!("Command terminated by {name} (signal {signum})"));
    }
    let signum = exit_code.checked_sub(128).filter(|s| *s > 0)?;
    note(signum).map(|note| {
        format!(
            "Exit code {exit_code} usually means the command was terminated by signal {signum}: \
             {note}"
        )
    })
}

/// The program name of the last command of a `&&`, `||`, `|` or `;` chain, its leading
/// `VAR=value` words and directory skipped.
fn base_command(command: &str) -> Option<&str> {
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"||") || bytes[i..].starts_with(b"&&") {
            i += 2;
            start = i;
        } else if matches!(bytes[i], b'|' | b';') {
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    let word = command[start..]
        .split(super::is_py_space)
        .filter(|w| !w.is_empty())
        .find(|w| !w.contains('=') || w.starts_with('-'))?;
    Some(crate::tools::basename(word))
}
