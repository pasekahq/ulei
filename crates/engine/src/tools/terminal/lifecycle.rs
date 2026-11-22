// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The lifecycle guard of `terminal`: a command that would restart, stop or kill the systemd unit
//! running the engine, directly or through a script or shell payload it runs, is refused before it
//! runs, since the unit's end kills the command's own caller. Every other unit passes.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex_lite::{Match, Regex};
use rustix::fs::{Mode, OFlags};

use crate::config::SERVICE_UNIT;
use crate::tools::glob::Pattern;
use crate::tools::redact::SPACE;
use crate::tools::{file_name, is_py_space, py_splitlines, re, realpath, resolve, shlex};

/// The engine executable, which a `kill`/`pkill` may name in place of the unit.
const EXECUTABLE: &str = "ulei-engine";

const SHELL_EXECUTABLES: [&str; 5] = ["sh", "bash", "dash", "ksh", "zsh"];
const SHELL_OPTIONS_WITH_VALUES: [&str; 4] = ["-O", "+O", "-o", "+o"];
const MAX_REFERENCED_SCRIPT_BYTES: usize = 1024 * 1024;
const MAX_REFERENCED_SCRIPT_DEPTH: usize = 8;
const CONTROL_CHARS: &str = ";&|()";

/// Executables whose arguments are data that they cannot execute: a lifecycle shape inside them
/// is a search, not a command.
const DATA_SINK_EXECUTABLES: [&str; 9] = [
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "ack",
    "journalctl",
    "sqlite3",
    "psql",
];
/// Argument shapes that smuggle execution back into a data sink; any of them keeps the segment.
const UNSAFE_DATA_ARG_MARKERS: [&str; 5] = ["`", "$(", "<(", ">(", "\\!"];

/// Executable images, which are never shell scripts.
const BINARY_MAGIC_PREFIXES: [&[u8]; 7] = [
    b"\x7fELF",
    b"MZ",
    b"\xca\xfe\xba\xbe",
    b"\xcf\xfa\xed\xfe",
    b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
];
const BINARY_SNIFF_BYTES: usize = 4096;

/// Characters a unit name may continue with after the part the guard looks for.
const UNIT_NAME_CHARS: &str = "A-Za-z0-9:_.@-";

/// The systemd service unit running the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnit {
    /// The unit name without its `.service` suffix.
    stem: String,
}

/// Why the unit to guard is not known.
#[derive(Debug)]
pub enum UnitError {
    /// The name is not a service unit name.
    Invalid(String),
    /// Under systemd the cgroup file could not be read.
    Cgroup(io::Error),
    /// Under systemd the cgroup file names no service unit.
    Undetermined,
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (key, env) = (SERVICE_UNIT.key, SERVICE_UNIT.env);
        match self {
            Self::Invalid(name) => write!(
                f,
                "{} is not a service unit name; set {key} ({env})",
                serde_json::Value::from(name.as_str())
            ),
            Self::Cgroup(e) => write!(
                f,
                "running under systemd (INVOCATION_ID is set) but /proc/self/cgroup could not be \
                 read: {e}; set {key} ({env})"
            ),
            Self::Undetermined => write!(
                f,
                "running under systemd (INVOCATION_ID is set) but /proc/self/cgroup names no \
                 service unit; set {key} ({env})"
            ),
        }
    }
}

impl std::error::Error for UnitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cgroup(e) => Some(e),
            _ => None,
        }
    }
}

impl ServiceUnit {
    /// The service `name`, with or without its `.service` suffix.
    ///
    /// # Errors
    /// [`UnitError::Invalid`] unless it is ASCII letters, digits and `:_.@-\`, starting with a
    /// letter, digit or `_`.
    pub fn new(name: &str) -> Result<Self, UnitError> {
        let stem = name.strip_suffix(".service").unwrap_or(name);
        let valid = stem.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ":_.@-\\".contains(c));
        if valid {
            Ok(Self {
                stem: stem.to_owned(),
            })
        } else {
            Err(UnitError::Invalid(name.to_owned()))
        }
    }

    /// The unit to guard: `setting` when given; else, `under_systemd`, the last `.service`
    /// component of the cgroup path in the text `cgroup` reads (`/proc/self/cgroup`); else none.
    ///
    /// # Errors
    /// [`UnitError`] for an invalid name, or under systemd an unreadable cgroup file or one naming
    /// no service.
    pub fn discover(
        setting: Option<&str>,
        under_systemd: bool,
        cgroup: impl FnOnce() -> io::Result<String>,
    ) -> Result<Option<Self>, UnitError> {
        if let Some(name) = setting {
            return Self::new(name).map(Some);
        }
        if !under_systemd {
            return Ok(None);
        }
        let text = cgroup().map_err(UnitError::Cgroup)?;
        let name = unit_in_cgroup(&text).ok_or(UnitError::Undetermined)?;
        Self::new(name).map(Some)
    }

    /// The full name, `<stem>.service`.
    pub fn name(&self) -> String {
        format!("{}.service", self.stem)
    }

    /// Whether the unit reference `name`, or with `process` the process name, is this unit.
    fn is_named(&self, name: &str, process: bool) -> bool {
        let name = name.to_ascii_lowercase();
        let unit = name.strip_suffix(".service").unwrap_or(&name);
        unit == self.stem.to_ascii_lowercase() || (process && name == EXECUTABLE)
    }
}

/// The last `.service` component of the unified cgroup path, else of the `name=systemd` one.
fn unit_in_cgroup(text: &str) -> Option<&str> {
    let path = |hierarchy: fn(&str, &str) -> bool| {
        text.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let (id, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
            hierarchy(id, controllers).then_some(path)
        })
    };
    let unified = path(|id, controllers| id == "0" && controllers.is_empty());
    let named = path(|_, controllers| controllers == "name=systemd");
    [unified, named].into_iter().flatten().find_map(|path| {
        path.split('/')
            .rfind(|c| c.len() > ".service".len() && c.ends_with(".service"))
    })
}

/// The guard of one engine: its unit and the patterns built from it on the first call.
#[derive(Debug)]
pub(super) struct Guard {
    unit: ServiceUnit,
    patterns: OnceLock<Patterns>,
}

#[derive(Debug)]
struct Patterns {
    /// The systemctl and kill/pkill shapes naming the unit or the executable, after the verb or
    /// before a pipe into it.
    lifecycle: Regex,
    /// A reference to the unit: its name, the rest of the name and a glob or `$` right after.
    unit_ref: Regex,
    /// A reference to the unit or the executable, as [`Patterns::unit_ref`].
    process_ref: Regex,
    /// A pipe into a shell or interpreter, which may execute data a sink prints.
    pipe_to_interpreter: Regex,
    /// A regex, substitution, pipe or glob that could widen a kill pattern to the unit.
    kill_unsafe: Regex,
}

impl Patterns {
    fn new(unit: &ServiceUnit) -> Self {
        let s = SPACE;
        let stem = regex_lite::escape(&unit.stem);
        let exe = regex_lite::escape(EXECUTABLE);
        let systemctl = format!(r"systemctl[{s}]+(?:-[^{s}]+[{s}]+)*(?:restart|stop|start)\b");
        let reference = |names: &str| {
            re(&format!(
                r"(?i)\b(?P<name>(?:{names})[{UNIT_NAME_CHARS}]*)(?P<glob>[*?\[{{$])?"
            ))
        };
        Self {
            lifecycle: re(&format!(
                r"(?i)(?:{systemctl}[^\n]*\b{stem})|(?:p?kill\b[^\n]*\b(?:{stem}|{exe}))|(?:\b{stem}[^\n]*\|[^\n]*{systemctl})|(?:\b(?:{stem}|{exe})[^\n]*\|[^\n]*p?kill\b)"
            )),
            unit_ref: reference(&stem),
            process_ref: reference(&format!("{stem}|{exe}")),
            pipe_to_interpreter: re(&format!(
                r"\|[{s}]*&?[{s}]*(?:sudo[{s}]+)?(?:sh|bash|dash|ksh|zsh|xargs|eval|source)\b"
            )),
            kill_unsafe: re(r"[|`*?+\[\]{}()^\\]|\$\(|\.\*"),
        }
    }
}

impl Guard {
    pub(super) fn new(unit: ServiceUnit) -> Self {
        Self {
            unit,
            patterns: OnceLock::new(),
        }
    }

    /// The error of a `command` run in `cwd` that may end the unit, `None` when it may run.
    pub(super) fn check(&self, command: &str, cwd: &Path) -> Option<String> {
        let scan = Scan {
            unit: &self.unit,
            p: self.patterns.get_or_init(|| Patterns::new(&self.unit)),
        };
        scan.unsafe_action(command, cwd, 0, &mut HashSet::new())
            .then(|| {
                let unit = self.unit.name();
                format!(
                    "Blocked: command or referenced script cannot restart or stop the {unit} \
                     service from inside the service process. The service would kill this command \
                     before it could complete (SIGTERM propagates to child processes). Run \
                     `systemctl restart {unit}` from a separate shell outside the running service."
                )
            })
    }
}

struct Scan<'a> {
    unit: &'a ServiceUnit,
    p: &'a Patterns,
}

impl Scan<'_> {
    /// The command, a shell payload or a script it runs, to [`MAX_REFERENCED_SCRIPT_DEPTH`]
    /// levels, may end the unit; deeper nesting and an unreadable special file count as such.
    fn unsafe_action(
        &self,
        command: &str,
        cwd: &Path,
        depth: usize,
        visited: &mut HashSet<PathBuf>,
    ) -> bool {
        let lines = line_segments(command);
        if self.scan_with_data_exemption(command) || self.widens(&lines) {
            return true;
        }
        if depth >= MAX_REFERENCED_SCRIPT_DEPTH {
            return true;
        }
        let segments: Vec<&[String]> = lines
            .iter()
            .flat_map(|(_, segments)| segments.iter().map(Vec::as_slice))
            .collect();
        for payload in shell_payloads(&segments) {
            if self.unsafe_action(payload, cwd, depth + 1, visited) {
                return true;
            }
        }
        for script in referenced_scripts(&segments, cwd) {
            let resolved = realpath(&script);
            if !visited.insert(resolved.clone()) {
                continue;
            }
            let (mut text, unsafe_file) = read_referenced_script(&script);
            if unsafe_file {
                return true;
            }
            if text.is_none() {
                let (again, unsafe_text) = sanitize(reread_script(&script));
                if unsafe_text {
                    return true;
                }
                text = again;
            }
            let Some(text) = text.filter(|t| !t.is_empty()) else {
                continue;
            };
            let dir = resolved.parent().unwrap_or(&resolved);
            if self.unsafe_action(&text, dir, depth + 1, visited) {
                return true;
            }
        }
        false
    }

    /// The lifecycle scan of `text`, then again with data-sink arguments masked: only a match
    /// outside them blocks.
    fn scan_with_data_exemption(&self, text: &str) -> bool {
        if !self.contains_lifecycle_command(text) {
            return false;
        }
        let normalized = join_continuations(text);
        self.contains_lifecycle_command(&self.mask_data_sink_arguments(&normalized))
    }

    /// A lifecycle shape in `text` that may target the unit.
    fn contains_lifecycle_command(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let normalized = join_continuations(text);
        if !self.may_hold_lifecycle_shape(&normalized) {
            return false;
        }
        // Every match start, since a greedy span may have swallowed another command on the line.
        let mut at = 0;
        while let Some(found) = self.p.lifecycle.find_at(&normalized, at) {
            if !self.targets_only_other_units(&normalized, &found) {
                return true;
            }
            let start = found.start();
            at = start + normalized[start..].chars().next().map_or(1, char::len_utf8);
        }
        false
    }

    /// Whether `text` holds the literals every branch of [`Patterns::lifecycle`] needs, compared
    /// in ASCII lowercase as its `(?i)` does; the regex alone would walk all of a long script.
    fn may_hold_lifecycle_shape(&self, text: &str) -> bool {
        let text = text.to_ascii_lowercase();
        (text.contains("systemctl") || text.contains("kill"))
            && (text.contains(&self.unit.stem.to_ascii_lowercase()) || text.contains(EXECUTABLE))
    }

    /// The systemctl or kill/pkill shape `found` names only other units: every reference on the
    /// rest of its line, and for a kill those in its own segment, is another unit without a glob,
    /// and a kill line holds no regex, substitution, pipe or glob.
    fn targets_only_other_units(&self, normalized: &str, found: &Match<'_>) -> bool {
        let head = found
            .as_str()
            .trim_start_matches(is_py_space)
            .to_ascii_lowercase();
        let is_systemctl = head.starts_with("systemctl");
        let is_kill = head.starts_with("kill") || head.starts_with("pkill");
        if !(is_systemctl || is_kill) {
            return false;
        }
        let line = &normalized[found.start()..];
        let rest = &line[..line.find('\n').unwrap_or(line.len())];
        let refs = if is_kill {
            &self.p.process_ref
        } else {
            &self.p.unit_ref
        };
        let Some(mut others) = self.other_units(refs, rest, is_kill) else {
            return false;
        };
        if is_kill {
            if self.p.kill_unsafe.is_match(rest) {
                return false;
            }
            let segment = &rest[..rest.find([';', '&', '#', '\n']).unwrap_or(rest.len())];
            let Some(in_segment) = self.other_units(refs, segment, true) else {
                return false;
            };
            others = in_segment;
        }
        others > 0
    }

    /// The count of references in `text`, or `None` when one is a glob or names this unit.
    fn other_units(&self, refs: &Regex, text: &str, process: bool) -> Option<usize> {
        let mut count = 0;
        for found in refs.captures_iter(text) {
            let name = found.name("name").map_or("", |m| m.as_str());
            if found.name("glob").is_some() || self.unit.is_named(name, process) {
                return None;
            }
            count += 1;
        }
        Some(count)
    }

    /// `text` with every argument of a data-sink command replaced by `arg`; a line that pipes
    /// into an interpreter, does not tokenize, or whose sink takes an execution-capable argument
    /// stays as it is.
    fn mask_data_sink_arguments<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let mut changed = false;
        let mut lines_out = Vec::new();
        for line in lines_or_whole(text) {
            if self.p.pipe_to_interpreter.is_match(line) {
                lines_out.push(line.to_owned());
                continue;
            }
            let Some(tokens) = shlex::split(line, CONTROL_CHARS, true) else {
                lines_out.push(line.to_owned());
                continue;
            };
            let mut segments: Vec<Vec<String>> = Vec::new();
            let mut current = Vec::new();
            for token in tokens {
                if is_control(&token) {
                    segments.push(std::mem::take(&mut current));
                    segments.push(vec![token]);
                } else {
                    current.push(token);
                }
            }
            segments.push(current);
            let mut rebuilt: Vec<String> = Vec::new();
            for segment in segments {
                match data_sink_arguments(&segment) {
                    Some(start) => {
                        changed = true;
                        rebuilt.extend_from_slice(&segment[..start]);
                        rebuilt
                            .extend(std::iter::repeat_n("arg".to_owned(), segment.len() - start));
                    }
                    None => rebuilt.extend(segment),
                }
            }
            lines_out.push(rebuilt.join(" "));
        }
        if changed {
            Cow::Owned(lines_out.join("\n"))
        } else {
            Cow::Borrowed(text)
        }
    }

    /// A lifecycle command whose target the text does not spell: a `systemctl restart|stop|start`
    /// unit that is a glob matching the unit, an expansion, or fed by `xargs`; a `pkill` pattern,
    /// or on a line that kills a `pgrep`/`pidof` pattern, that matches the unit or the executable.
    fn widens(&self, lines: &[(String, Vec<Vec<String>>)]) -> bool {
        for (line, segments) in lines {
            let into_interpreter = self.p.pipe_to_interpreter.is_match(line);
            let commands: Vec<(Lifecycle, &[String], bool)> = segments
                .iter()
                .filter(|s| into_interpreter || data_sink_arguments(s).is_none())
                .filter_map(|s| lifecycle_command(s))
                .collect();
            let kills = commands
                .iter()
                .any(|(tool, _, _)| matches!(tool, Lifecycle::Kill | Lifecycle::Pkill));
            for (tool, args, fed) in commands {
                let operands = || args.iter().filter(|a| !a.starts_with('-'));
                let widened = match tool {
                    Lifecycle::Systemctl => {
                        let mut operands = operands();
                        let verb = operands.next().map(|v| v.to_ascii_lowercase());
                        matches!(verb.as_deref(), Some("restart" | "stop" | "start"))
                            && (fed || operands.any(|u| self.unit_pattern_widens(u)))
                    }
                    Lifecycle::Pkill => operands().any(|a| self.process_pattern_widens(a)),
                    Lifecycle::Pgrep if kills => operands().any(|a| self.process_pattern_widens(a)),
                    Lifecycle::Kill | Lifecycle::Pgrep => false,
                };
                if widened {
                    return true;
                }
            }
        }
        false
    }

    fn unit_pattern_widens(&self, unit: &str) -> bool {
        if unit.contains(['$', '`', '{']) {
            return true;
        }
        if !unit.contains(['*', '?', '[']) {
            return false;
        }
        Pattern::parse(&unit.to_ascii_lowercase()).map_or(true, |pattern| {
            [self.unit.name(), self.unit.stem.clone()]
                .into_iter()
                .any(|name| pattern.matches(&[name.to_ascii_lowercase()]))
        })
    }

    fn process_pattern_widens(&self, pattern: &str) -> bool {
        if pattern.contains(['$', '`']) {
            return true;
        }
        Regex::new(&format!("(?i){pattern}")).map_or(true, |regex| {
            [EXECUTABLE, &self.unit.stem, &self.unit.name()]
                .into_iter()
                .any(|name| regex.is_match(name))
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Lifecycle {
    Systemctl,
    Kill,
    Pkill,
    /// `pgrep` or `pidof`, which name the processes a kill on the same line may take.
    Pgrep,
}

/// The first lifecycle executable of `segment` from its command word on, its arguments, and
/// whether `xargs` runs it.
fn lifecycle_command(segment: &[String]) -> Option<(Lifecycle, &[String], bool)> {
    let start = command_token_index(segment)?;
    segment[start..].iter().enumerate().find_map(|(i, token)| {
        let tool = match file_name(Path::new(token)).as_str() {
            "systemctl" => Lifecycle::Systemctl,
            "kill" => Lifecycle::Kill,
            "pkill" => Lifecycle::Pkill,
            "pgrep" | "pidof" => Lifecycle::Pgrep,
            _ => return None,
        };
        let fed = segment[start..start + i]
            .iter()
            .any(|t| file_name(Path::new(t)) == "xargs");
        Some((tool, &segment[start + i + 1..], fed))
    })
}

/// Index of the first argument of a data-sink command whose arguments are all inert.
fn data_sink_arguments(segment: &[String]) -> Option<usize> {
    let index = command_token_index(segment)?;
    if !DATA_SINK_EXECUTABLES.contains(&file_name(Path::new(&segment[index])).as_str()) {
        return None;
    }
    let arguments = &segment[index + 1..];
    let unsafe_argument = arguments.iter().any(|argument| {
        argument.starts_with('.')
            || UNSAFE_DATA_ARG_MARKERS
                .iter()
                .any(|marker| argument.contains(marker))
    });
    (!unsafe_argument).then_some(index + 1)
}

/// `text` with each shell line continuation, a backslash, an optional CR, a newline and the
/// blanks after it, joined into one space as the shell joins the lines before parsing.
fn join_continuations(text: &str) -> Cow<'_, str> {
    if !text.contains('\\') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let after_cr = after.strip_prefix('\r').unwrap_or(after);
        if let Some(next) = after_cr.strip_prefix('\n') {
            out.push(' ');
            rest = next.trim_start_matches([' ', '\t']);
        } else {
            out.push('\\');
            rest = after;
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The lines of `text` as Python's `splitlines`, or `text` itself when it has none.
fn lines_or_whole(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = py_splitlines(text, false).collect();
    if lines.is_empty() { vec![text] } else { lines }
}

/// Each tokenizable line of `command`, line continuations removed, with its command segments.
fn line_segments(command: &str) -> Vec<(String, Vec<Vec<String>>)> {
    let normalized = command.replace("\\\n", "");
    lines_or_whole(&normalized)
        .into_iter()
        .filter_map(|line| {
            let tokens = shlex::split(line, CONTROL_CHARS, true)?;
            let mut segments = Vec::new();
            let mut segment = Vec::new();
            for token in tokens {
                if is_control(&token) {
                    if !segment.is_empty() {
                        segments.push(std::mem::take(&mut segment));
                    }
                } else {
                    segment.push(token);
                }
            }
            if !segment.is_empty() {
                segments.push(segment);
            }
            Some((line.to_owned(), segments))
        })
        .collect()
}

fn is_control(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| CONTROL_CHARS.contains(c))
}

/// Index of the executable word, after leading `NAME=value` assignments.
fn command_token_index(segment: &[String]) -> Option<usize> {
    segment.iter().position(|token| {
        let name = token.split_once('=').map(|(name, _)| name);
        !name.is_some_and(|name| {
            name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

/// The code passed to a POSIX shell with `-c` or `--command`.
fn shell_payloads<'a>(segments: &[&'a [String]]) -> impl Iterator<Item = &'a str> {
    segments.iter().filter_map(|segment| {
        let index = command_token_index(segment)?;
        if !SHELL_EXECUTABLES.contains(&file_name(Path::new(&segment[index])).as_str()) {
            return None;
        }
        let arguments = &segment[index + 1..];
        arguments
            .windows(2)
            .find(|pair| pair[0] == "-c" || pair[0] == "--command")
            .map(|pair| pair[1].as_str())
    })
}

/// Scripts run directly, through `source`, or as the operand of a POSIX shell, against `cwd`.
fn referenced_scripts(segments: &[&[String]], cwd: &Path) -> Vec<PathBuf> {
    let script = |candidate: &str| (!candidate.contains('\0')).then(|| resolve(cwd, candidate));
    let mut scripts = Vec::new();
    for segment in segments {
        let Some(index) = command_token_index(segment) else {
            continue;
        };
        let executable = &segment[index];
        let name = file_name(Path::new(executable));
        if name == "." || name == "source" {
            scripts.extend(segment.get(index + 1).and_then(|s| script(s)));
            continue;
        }
        if SHELL_EXECUTABLES.contains(&name.as_str()) {
            let arguments = &segment[index + 1..];
            let mut at = 0;
            while let Some(argument) = arguments.get(at) {
                if argument == "--" {
                    at += 1;
                    break;
                }
                if argument == "-c" || argument == "--command" {
                    break;
                }
                if SHELL_OPTIONS_WITH_VALUES.contains(&argument.as_str()) {
                    at += 2;
                    continue;
                }
                if argument.starts_with('-') {
                    at += 1;
                    continue;
                }
                break;
            }
            if let Some(operand) = arguments.get(at)
                && operand != "-c"
                && operand != "--command"
            {
                scripts.extend(script(operand));
            }
            continue;
        }
        // A bare `/` is an operator in sources such as Python's, not an executable.
        if !executable.trim_matches('/').is_empty()
            && (executable.contains('/')
                || [".sh", ".bash", ".zsh"]
                    .iter()
                    .any(|suffix| executable.ends_with(suffix)))
        {
            scripts.extend(script(executable));
        }
    }
    scripts
}

/// The text of a script and whether it must count as unsafe: a regular file read to one byte past
/// the limit, a directory or a binary read as nothing, another special file or an oversized file
/// unsafe. It opens without blocking, so a FIFO cannot hang the guard.
fn read_referenced_script(path: &Path) -> (Option<String>, bool) {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let Ok(file) = rustix::fs::open(path, flags, Mode::empty()).map(File::from) else {
        return (None, false);
    };
    let Ok(metadata) = file.metadata() else {
        return (None, false);
    };
    if !metadata.is_file() {
        return (None, !metadata.is_dir());
    }
    let mut data = vec![0; BINARY_SNIFF_BYTES];
    let Ok(sniffed) = (&file).read(&mut data) else {
        return (None, false);
    };
    data.truncate(sniffed);
    if BINARY_MAGIC_PREFIXES.iter().any(|m| data.starts_with(m)) || data.contains(&0) {
        return (None, false);
    }
    let limit = (MAX_REFERENCED_SCRIPT_BYTES + 1).saturating_sub(data.len()) as u64;
    if (&file).take(limit).read_to_end(&mut data).is_err() {
        return (None, false);
    }
    if data.contains(&0) {
        return (None, false);
    }
    if data.len() > MAX_REFERENCED_SCRIPT_BYTES {
        return (None, true);
    }
    (Some(String::from_utf8_lossy(&data).into_owned()), false)
}

/// The run's second read of a script the first read gave no text for: a regular file up to one
/// byte past the limit, nothing for a binary with a NUL byte.
fn reread_script(path: &Path) -> Option<String> {
    if !fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut data = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_REFERENCED_SCRIPT_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

/// The read contract applied to the second read: nothing for empty or NUL-bearing text, unsafe
/// above the limit in UTF-8 bytes.
fn sanitize(text: Option<String>) -> (Option<String>, bool) {
    match text {
        None => (None, false),
        Some(text) if text.is_empty() || text.contains('\0') => (None, false),
        Some(text) if text.len() > MAX_REFERENCED_SCRIPT_BYTES => (None, true),
        Some(text) => (Some(text), false),
    }
}
