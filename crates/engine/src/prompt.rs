// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The system prompt of a session: built once, from the brain files and the host as they are
//! at the session's first turn, stored with the session and sent unchanged on every later
//! turn. Its three tiers, stable, context and volatile, each join their non-blank parts with
//! a blank line, and the prompt joins its non-empty tiers the same way.

mod context;
mod memory;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::tools::{py_splitlines, py_strip};
use crate::zone::{Zone, civil_from_days, weekday};

pub use context::CONTEXT_FILE_MAX_CHARS;
use memory::{Target, entries, render_block};

/// Default char limit of the MEMORY store.
pub const DEFAULT_MEMORY_CHAR_LIMIT: usize = 2200;
/// Default char limit of the USER store.
pub const DEFAULT_USER_CHAR_LIMIT: usize = 1375;

/// Guidance for agents with tools: finish with real results, never fabricated ones.
const TASK_COMPLETION_GUIDANCE: &str = "# Finishing the job\n\
When the user asks you to build, run, or verify something, the deliverable is \
a working artifact backed by real tool output — not a description of one. \
Do not stop after writing a stub, a plan, or a single command. Keep working \
until you have actually exercised the code or produced the requested result, \
then report what real execution returned.\n\
If a tool, install, or network call fails and blocks the real path, say so \
directly and try an alternative (different package manager, different \
approach, ask the user). NEVER substitute plausible-looking fabricated \
output (made-up data, invented file contents, synthesised API responses) \
for results you couldn't actually produce. Reporting a blocker honestly \
is always better than inventing a result.";

/// Guidance for agents with tools: batch independent calls into one reply.
const PARALLEL_TOOL_CALL_GUIDANCE: &str = "# Parallel tool calls\n\
When you need several pieces of information that don't depend on each \
other, request them together in a single response instead of one tool \
call per turn. Independent reads, searches, web fetches, and read-only \
commands should be batched into the same assistant turn — the runtime \
executes independent calls concurrently, and batching avoids resending \
the whole conversation on every extra round-trip.\n\
Only serialize calls when a later call genuinely depends on an earlier \
call's result (e.g. you must read a file before you can patch it). When \
in doubt and the calls are independent, batch them.";

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Why a prompt could not be built.
#[derive(Debug)]
pub enum Error {
    /// SOUL.md is absent or blank.
    NoSoul(PathBuf),
    /// A brain or context file exists but is not readable UTF-8 text.
    Read(PathBuf, io::Error),
    /// The host facts of the environment block could not be read.
    Host(&'static str, io::Error),
    /// `HOME` is unset or empty.
    NoHome,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSoul(path) => write!(f, "SOUL.md {} is missing or blank", path.display()),
            Self::Read(path, e) => write!(f, "prompt file {}: {e}", path.display()),
            Self::Host(what, e) => write!(f, "host {what}: {e}"),
            Self::NoHome => write!(f, "HOME is not set"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, e) | Self::Host(_, e) => Some(e),
            _ => None,
        }
    }
}

/// The files the prompt is built from and the char limits of the memory stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brain {
    /// SOUL.md, the identity.
    pub soul: PathBuf,
    /// INDEX.md of the knowledge base, named by the prompt and not read.
    pub index: PathBuf,
    /// MEMORY.md, the agent's notes.
    pub memory: PathBuf,
    /// USER.md, the profile of the user.
    pub user: PathBuf,
    /// Char limit of the MEMORY store.
    pub memory_char_limit: usize,
    /// Char limit of the USER store.
    pub user_char_limit: usize,
}

/// What the prompt tells about the host the tools run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// Kernel name and release, as `Linux (6.8.0)`.
    pub system: String,
    /// The user's home directory.
    pub home: String,
    /// Working directory of the tools.
    pub cwd: PathBuf,
}

impl Host {
    /// The running host, with `home` from `HOME` and the tools working in `cwd`.
    ///
    /// # Errors
    /// [`Error::Host`] when the kernel name or release is unreadable, [`Error::NoHome`].
    pub fn detect(home: Option<OsString>, cwd: PathBuf) -> Result<Self, Error> {
        let kernel = |what: &'static str| {
            std::fs::read_to_string(format!("/proc/sys/kernel/{what}"))
                .map(|text| text.trim().to_owned())
                .map_err(|e| Error::Host(what, e))
        };
        let home = home.filter(|home| !home.is_empty()).ok_or(Error::NoHome)?;
        Ok(Self {
            system: format!("{} ({})", kernel("ostype")?, kernel("osrelease")?),
            home: home.to_string_lossy().into_owned(),
            cwd,
        })
    }
}

/// The system prompt of a new session started at Unix time `now` in `zone`.
///
/// # Errors
/// [`Error::NoSoul`] or [`Error::Read`].
pub fn build(brain: &Brain, host: &Host, zone: &Zone, now: i64) -> Result<String, Error> {
    let stable = [
        context::soul(&brain.soul)?,
        index_pointer(&brain.index),
        TASK_COMPLETION_GUIDANCE.to_owned(),
        PARALLEL_TOOL_CALL_GUIDANCE.to_owned(),
        environment(host),
    ];
    let context = [context::project(&host.cwd)?];
    // The skills index leads this tier once the engine has skill tools: a changed index then
    // re-reads only the tail of the cached prefix.
    let volatile = [
        render_block(
            Target::Memory,
            &entries(&brain.memory)?,
            brain.memory_char_limit,
        ),
        render_block(Target::User, &entries(&brain.user)?, brain.user_char_limit),
        started(zone, now),
    ];
    let tiers = [join(&stable), join(&context), join(&volatile)];
    Ok(tiers
        .iter()
        .filter(|tier| !tier.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// The stored prompt a turn sends again: one that is present and not empty, unless the
/// session has no history yet or the prompt names another working directory than `cwd`.
pub fn restore<'a>(stored: Option<&'a str>, has_history: bool, cwd: &Path) -> Option<&'a str> {
    let stored = stored.filter(|prompt| has_history && !prompt.is_empty())?;
    let built_in = host_info_value(stored, "Current working directory");
    (built_in.is_empty() || built_in == cwd.display().to_string()).then_some(stored)
}

/// The value of `label` in the prompt's own host block: among the three lines after the
/// first `User home directory:` line, so project text with the same label is never read.
fn host_info_value<'a>(prompt: &'a str, label: &str) -> &'a str {
    let lines: Vec<&str> = py_splitlines(prompt, false).collect();
    let Some(at) = lines
        .iter()
        .position(|line| line.starts_with("User home directory:"))
    else {
        return "";
    };
    lines[at + 1..lines.len().min(at + 4)]
        .iter()
        .find_map(|line| line.strip_prefix(label)?.strip_prefix(':'))
        .map_or("", py_strip)
}

fn index_pointer(index: &Path) -> String {
    format!(
        "Knowledge base index: {}. Read it with the read_file tool to find the notes a task needs.",
        index.display()
    )
}

fn environment(host: &Host) -> String {
    format!(
        "Host: {}\nUser home directory: {}\nCurrent working directory: {}",
        host.system,
        host.home,
        host.cwd.display()
    )
}

/// `Conversation started: <weekday>, <month> <dd>, <yyyy> (<zone>, <abbreviation>, UTC±hh:mm)`
/// in the local time of `zone`; the name is left out when the zone has none and the
/// abbreviation when it equals the name.
fn started(zone: &Zone, now: i64) -> String {
    let local = zone.at(now);
    let days = (now + local.offset).div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut bits: Vec<String> = Vec::new();
    if let Some(name) = zone.name() {
        bits.push(name.to_owned());
    }
    if !local.abbrev.is_empty() && zone.name() != Some(local.abbrev.as_str()) {
        bits.push(local.abbrev);
    }
    let sign = if local.offset < 0 { '-' } else { '+' };
    let offset = local.offset.abs();
    let seconds = if offset % 60 == 0 {
        String::new()
    } else {
        format!("{:02}", offset % 60)
    };
    bits.push(format!(
        "UTC{sign}{:02}:{:02}{seconds}",
        offset / 3600,
        offset % 3600 / 60
    ));
    format!(
        "Conversation started: {}, {} {day:02}, {year} ({})",
        WEEKDAYS[usize::try_from(weekday(days)).unwrap_or(0)],
        MONTHS[usize::try_from(month - 1).unwrap_or(0)],
        bits.join(", ")
    )
}

/// The non-blank `parts`, each stripped, joined by a blank line.
fn join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| py_strip(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}
