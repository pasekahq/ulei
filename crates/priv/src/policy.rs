//! The policy file (ADR-0073 section 2): `allowed_uids` and `[[command]]` tables in a strict TOML
//! subset, and the checks before `serve` trusts it.
//!
//! ```toml
//! allowed_uids = [1000]
//!
//! [[command]]
//! id = "restart-user-unit"
//! argv = ["/usr/bin/systemctl", "--user", "-M", "someone@", "restart", "{arg}"]
//! arg = ["ulei-inbox.service", "ulei-tg.service"]
//! timeout_s = 60
//! max_output_bytes = 65536
//! ```

use std::fmt;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

/// The slot an argument value fills, within one `argv` item.
pub const SLOT: &str = "{arg}";
/// `timeout_s` when the command gives none.
pub const DEFAULT_TIMEOUT_S: u64 = 30;
/// `max_output_bytes` when the command gives none, per stream.
pub const DEFAULT_MAX_OUTPUT: usize = 256 * 1024;
const MAX_TIMEOUT_S: u64 = 3600;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_ID: usize = 64;

/// One allowed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Its fixed name, `[a-z0-9-]{1,64}`.
    pub id: String,
    /// Its fixed argv; `argv[0]` is absolute; [`SLOT`] occurs once when `arg` is not empty.
    pub argv: Vec<String>,
    /// The only values the slot may take; empty: the command takes no argument.
    pub arg: Vec<String>,
    /// Seconds until SIGTERM.
    pub timeout_s: u64,
    /// Bytes kept of stdout and of stderr each.
    pub max_output_bytes: usize,
}

/// A checked policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Peer uids that may call.
    pub allowed_uids: Vec<u32>,
    /// The commands in file order.
    pub commands: Vec<Command>,
}

/// Why a request does not match the policy; the `refused:` reasons of the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No command has this id.
    UnknownCommand,
    /// Missing, extra or unlisted argument.
    BadArgument,
}

impl Refusal {
    /// The reason as the audit and the client name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCommand => "unknown_command",
            Self::BadArgument => "bad_argument",
        }
    }
}

impl Policy {
    /// The command `id` with `args` filled in: its argv, timeout and cap.
    ///
    /// # Errors
    /// [`Refusal`] when `id` is unknown or `args` is not exactly one listed value for a command
    /// with `arg`, or not empty for one without.
    pub fn resolve(&self, id: &str, args: &[String]) -> Result<(&Command, Vec<String>), Refusal> {
        let cmd = self
            .commands
            .iter()
            .find(|c| c.id == id)
            .ok_or(Refusal::UnknownCommand)?;
        if cmd.arg.is_empty() {
            if !args.is_empty() {
                return Err(Refusal::BadArgument);
            }
            return Ok((cmd, cmd.argv.clone()));
        }
        let [value] = args else {
            return Err(Refusal::BadArgument);
        };
        if !cmd.arg.iter().any(|a| a == value) {
            return Err(Refusal::BadArgument);
        }
        let filled = cmd
            .argv
            .iter()
            .map(|item| item.replace(SLOT, value))
            .collect();
        Ok((cmd, filled))
    }
}

/// Why the policy is refused: one line for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reads `path` after [`check_owner`] with `owner` and parses it.
///
/// # Errors
/// The first owner, read or syntax fault.
pub fn load(path: &Path, owner: u32) -> Result<Policy, Error> {
    let shown = path.display();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_meta =
        fs::symlink_metadata(path).map_err(|e| Error(format!("policy {shown}: {e}")))?;
    let dir_meta = fs::metadata(dir).map_err(|e| Error(format!("policy {shown}: {e}")))?;
    check_owner(&Stat::of(&file_meta), &Stat::of(&dir_meta), owner)
        .map_err(|why| Error(format!("policy {shown}: {why}")))?;
    let text = fs::read_to_string(path).map_err(|e| Error(format!("policy {shown}: {e}")))?;
    parse(&text).map_err(|(line, why)| Error(format!("policy {shown} line {line}: {why}")))
}

/// What [`check_owner`] looks at of a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Owner uid.
    pub uid: u32,
    /// Mode bits with the file type.
    pub mode: u32,
    /// Whether it is a regular file.
    pub regular: bool,
    /// Whether it is a directory.
    pub dir: bool,
}

impl Stat {
    fn of(meta: &fs::Metadata) -> Self {
        let kind = meta.file_type();
        Self {
            uid: meta.uid(),
            mode: meta.mode(),
            regular: kind.is_file() && !kind.is_symlink() && !kind.is_socket(),
            dir: kind.is_dir(),
        }
    }
}

/// The policy file must be a regular file and it and its directory owned by `owner` (the
/// daemon's own euid, root in the unit) and not writable by group or others.
///
/// # Errors
/// The fault, without the path.
pub fn check_owner(file: &Stat, dir: &Stat, owner: u32) -> Result<(), String> {
    if !file.regular {
        return Err("not a regular file".into());
    }
    if !dir.dir {
        return Err("its directory is not a directory".into());
    }
    for (what, st) in [("the file", file), ("its directory", dir)] {
        if st.uid != owner {
            return Err(format!("{what} is owned by uid {}, not {owner}", st.uid));
        }
        if st.mode & 0o022 != 0 {
            return Err(format!(
                "{what} is writable by group or others (mode {:04o})",
                st.mode & 0o7777
            ));
        }
    }
    Ok(())
}

/// A value of the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Str(String),
    Int(u64),
    Strs(Vec<String>),
    Ints(Vec<u64>),
}

/// Parses and checks the policy text; the error is the line number and why.
///
/// # Errors
/// The first refused line, or line 0 for a fault of the whole file.
pub fn parse(text: &str) -> Result<Policy, (usize, String)> {
    let mut uids: Option<Vec<u64>> = None;
    let mut tables: Vec<(usize, Vec<(String, Value)>)> = Vec::new();
    for (n, raw) in (1..).zip(text.lines()) {
        let line = raw.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if strip_comment(line) != "[[command]]" {
                return Err((n, "the only table is [[command]]".into()));
            }
            tables.push((n, Vec::new()));
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or((n, "expected key = value".to_owned()))?;
        let key = key.trim_matches([' ', '\t']);
        let value = parse_value(value.trim_matches([' ', '\t'])).map_err(|e| (n, e.to_owned()))?;
        match tables.last_mut() {
            None => {
                if key != "allowed_uids" {
                    return Err((n, format!("unknown key {key}")));
                }
                if uids.is_some() {
                    return Err((n, "duplicate key".into()));
                }
                match value {
                    Value::Ints(v) => uids = Some(v),
                    Value::Strs(v) if v.is_empty() => uids = Some(Vec::new()),
                    _ => return Err((n, "allowed_uids must be a list of integers".into())),
                }
            }
            Some((_, keys)) => {
                if keys.iter().any(|(k, _)| k == key) {
                    return Err((n, "duplicate key".into()));
                }
                keys.push((key.to_owned(), value));
            }
        }
    }
    let allowed_uids = uids
        .ok_or((0, "allowed_uids is missing".to_owned()))?
        .into_iter()
        .map(u32::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| (0, "a uid is out of range".to_owned()))?;
    if allowed_uids.is_empty() {
        return Err((0, "allowed_uids is empty".into()));
    }
    let mut commands: Vec<Command> = Vec::new();
    for (n, keys) in tables {
        let cmd = command(keys).map_err(|e| (n, e))?;
        if commands.iter().any(|c| c.id == cmd.id) {
            return Err((n, format!("duplicate id {}", cmd.id)));
        }
        commands.push(cmd);
    }
    if commands.is_empty() {
        return Err((0, "no [[command]]".into()));
    }
    Ok(Policy {
        allowed_uids,
        commands,
    })
}

fn command(keys: Vec<(String, Value)>) -> Result<Command, String> {
    let mut id = None;
    let mut argv = None;
    let mut arg = Vec::new();
    let mut timeout_s = DEFAULT_TIMEOUT_S;
    let mut max_output_bytes = DEFAULT_MAX_OUTPUT;
    for (key, value) in keys {
        match (key.as_str(), value) {
            ("id", Value::Str(s)) => id = Some(s),
            ("argv", Value::Strs(v)) => argv = Some(v),
            ("arg", Value::Strs(v)) => arg = v,
            ("timeout_s", Value::Int(t)) if (1..=MAX_TIMEOUT_S).contains(&t) => timeout_s = t,
            ("max_output_bytes", Value::Int(m)) => {
                max_output_bytes = usize::try_from(m)
                    .ok()
                    .filter(|m| (1..=MAX_OUTPUT).contains(m))
                    .ok_or(format!("max_output_bytes must be 1 to {MAX_OUTPUT}"))?;
            }
            ("id" | "argv" | "arg" | "timeout_s" | "max_output_bytes", _) => {
                return Err(format!(
                    "{key} has the wrong kind or range (id a string, argv and arg lists of strings, timeout_s 1 to {MAX_TIMEOUT_S})"
                ));
            }
            _ => return Err(format!("unknown key {key}")),
        }
    }
    let id = id.ok_or("id is missing")?;
    let id_ok = !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !id_ok {
        return Err(format!("id {id:?} is not [a-z0-9-]{{1,{MAX_ID}}}"));
    }
    let argv = argv.ok_or(format!("{id}: argv is missing"))?;
    let Some(program) = argv.first() else {
        return Err(format!("{id}: argv is empty"));
    };
    if !program.starts_with('/') {
        return Err(format!("{id}: argv[0] {program:?} is not an absolute path"));
    }
    if program.contains(SLOT) {
        return Err(format!("{id}: argv[0] holds {SLOT}"));
    }
    for item in argv.iter().chain(&arg) {
        if let Some(p) = placeholder(item) {
            return Err(format!("{id}: placeholder {p} left; fill it in"));
        }
    }
    let slots: usize = argv.iter().map(|a| a.matches(SLOT).count()).sum();
    if arg.is_empty() && slots != 0 {
        return Err(format!("{id}: argv holds {SLOT} but arg lists no value"));
    }
    if !arg.is_empty() && slots != 1 {
        return Err(format!(
            "{id}: with arg, argv must hold {SLOT} exactly once"
        ));
    }
    for value in &arg {
        if value.is_empty() || value.starts_with('-') || value.contains(SLOT) {
            return Err(format!(
                "{id}: arg value {value:?} is empty, starts with - or holds {SLOT}"
            ));
        }
    }
    Ok(Command {
        id,
        argv,
        arg,
        timeout_s,
        max_output_bytes,
    })
}

/// A `<UPPER_CASE>` placeholder of the shipped example left in `s`.
fn placeholder(s: &str) -> Option<&str> {
    let start = s.find('<')?;
    let len = s[start..].find('>')?;
    let name = &s[start + 1..start + len];
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'))
        .then(|| &s[start..=start + len])
}

/// `line` without a trailing comment, for a line that holds no string.
fn strip_comment(line: &str) -> &str {
    line.split('#')
        .next()
        .unwrap_or("")
        .trim_matches([' ', '\t'])
}

fn parse_value(text: &str) -> Result<Value, &'static str> {
    if text.starts_with('"') {
        let (s, rest) = string(text)?;
        trailing(rest)?;
        return Ok(Value::Str(s));
    }
    if let Some(mut rest) = text.strip_prefix('[') {
        let mut strs = Vec::new();
        let mut ints = Vec::new();
        loop {
            rest = rest.trim_start_matches([' ', '\t']);
            if let Some(after) = rest.strip_prefix(']') {
                trailing(after)?;
                break;
            }
            if rest.is_empty() {
                return Err("unterminated list (lists are one line)");
            }
            if rest.starts_with('"') {
                let (s, after) = string(rest)?;
                strs.push(s);
                rest = after;
            } else {
                let end = rest
                    .find([',', ']', ' ', '\t'])
                    .ok_or("unterminated list")?;
                ints.push(integer(&rest[..end])?);
                rest = &rest[end..];
            }
            rest = rest.trim_start_matches([' ', '\t']);
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
            } else if rest.is_empty() {
                return Err("unterminated list (lists are one line)");
            } else if !rest.starts_with(']') {
                return Err("expected , or ] in a list");
            }
        }
        return match (strs.is_empty(), ints.is_empty()) {
            (_, true) => Ok(Value::Strs(strs)),
            (true, false) => Ok(Value::Ints(ints)),
            (false, false) => Err("a list mixes strings and integers"),
        };
    }
    let end = text.find([' ', '\t', '#']).unwrap_or(text.len());
    let n = integer(&text[..end])?;
    trailing(&text[end..])?;
    Ok(Value::Int(n))
}

fn integer(text: &str) -> Result<u64, &'static str> {
    let decimal = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if !decimal {
        return Err("unsupported value");
    }
    text.parse().map_err(|_| "integer out of range")
}

/// A basic string at the start of `text` (which starts with `"`) and the text after it.
fn string(text: &str) -> Result<(String, &str), &'static str> {
    let mut out = String::new();
    let mut chars = text[1..].char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &text[1 + i + 1..])),
            '\\' => match chars.next().map(|(_, e)| e) {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                _ => return Err("unsupported escape (only \\t \\n \\\" \\\\)"),
            },
            c if c.is_control() => return Err("control character in a string"),
            c => out.push(c),
        }
    }
    Err("unterminated string")
}

fn trailing(rest: &str) -> Result<(), &'static str> {
    let rest = rest.trim_matches([' ', '\t']);
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err("unexpected text after the value")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
# a comment
allowed_uids = [1000, 1001]

[[command]]
id = "echo"
argv = ["/bin/echo", "a\tb"]  # trailing comment

[[command]]
id = "status"
argv = ["/usr/bin/systemctl", "is-active", "{arg}"]
arg = ["a.service", "b.service"]
timeout_s = 5
max_output_bytes = 100
"#;

    fn err(text: &str) -> String {
        parse(text).expect_err("refused").1
    }

    fn with_cmd(body: &str) -> String {
        format!("allowed_uids = [1]\n[[command]]\n{body}\n")
    }

    #[test]
    fn parses_the_subset() {
        let p = parse(GOOD).expect("parses");
        assert_eq!(p.allowed_uids, [1000, 1001]);
        assert_eq!(p.commands[0].argv, ["/bin/echo", "a\tb"]);
        assert_eq!(p.commands[0].timeout_s, DEFAULT_TIMEOUT_S);
        assert_eq!(p.commands[0].max_output_bytes, DEFAULT_MAX_OUTPUT);
        assert_eq!(p.commands[1].arg, ["a.service", "b.service"]);
        assert_eq!(p.commands[1].timeout_s, 5);
        assert_eq!(p.commands[1].max_output_bytes, 100);
    }

    #[test]
    fn resolves_ids_and_arguments() {
        let p = parse(GOOD).expect("parses");
        let (_, argv) = p.resolve("echo", &[]).expect("echo");
        assert_eq!(argv, ["/bin/echo", "a\tb"]);
        let (_, argv) = p.resolve("status", &["b.service".into()]).expect("status");
        assert_eq!(argv, ["/usr/bin/systemctl", "is-active", "b.service"]);
        assert_eq!(p.resolve("nope", &[]).err(), Some(Refusal::UnknownCommand));
        // An argument to a command without one, a missing, an unlisted and an extra argument.
        assert_eq!(
            p.resolve("echo", &["x".into()]).err(),
            Some(Refusal::BadArgument)
        );
        assert_eq!(p.resolve("status", &[]).err(), Some(Refusal::BadArgument));
        assert_eq!(
            p.resolve("status", &["c.service".into()]).err(),
            Some(Refusal::BadArgument)
        );
        assert_eq!(
            p.resolve("status", &["a.service".into(), "b.service".into()])
                .err(),
            Some(Refusal::BadArgument)
        );
        assert_eq!(
            p.resolve("status", &["a.service; reboot".into()]).err(),
            Some(Refusal::BadArgument)
        );
    }

    #[test]
    fn refuses_bad_policies() {
        assert!(err("[[command]]\nid = \"a\"\nargv = [\"/a\"]\n").contains("allowed_uids"));
        assert!(
            err("allowed_uids = []\n[[command]]\nid = \"a\"\nargv = [\"/a\"]").contains("empty")
        );
        assert!(err("allowed_uids = [1]\n").contains("no [[command]]"));
        assert!(err("allowed_uids = [1]\nfoo = 1\n").contains("unknown key"));
        assert!(err("allowed_uids = [1]\n[cmd]\n").contains("[[command]]"));
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"bin/a\"]")).contains("absolute"));
        assert!(err(&with_cmd("id = \"a\"\nargv = []")).contains("empty"));
        assert!(err(&with_cmd("id = \"A\"\nargv = [\"/a\"]")).contains("[a-z0-9-]"));
        assert!(err(&with_cmd("argv = [\"/a\"]")).contains("id is missing"));
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\", \"{arg}\"]")).contains("arg lists no"));
        assert!(
            err(&with_cmd("id = \"a\"\nargv = [\"/a\"]\narg = [\"x\"]")).contains("exactly once")
        );
        assert!(
            err(&with_cmd(
                "id = \"a\"\nargv = [\"/a\", \"{arg}\"]\narg = [\"-x\"]"
            ))
            .contains("starts with -")
        );
        assert!(
            err(&with_cmd("id = \"a\"\nargv = [\"/a\", \"<ULEI_USER>@\"]")).contains("placeholder")
        );
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\"]\ntimeout_s = 0")).contains("range"));
        assert!(
            err(&with_cmd(
                "id = \"a\"\nargv = [\"/a\"]\nmax_output_bytes = 0"
            ))
            .contains("1 to")
        );
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\"]\nshell = 1")).contains("unknown key"));
        assert!(err(&with_cmd("id = \"a\"\nid = \"b\"")).contains("duplicate key"));
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\", 1]")).contains("mixes"));
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\\q\"]")).contains("escape"));
        assert!(err(&with_cmd("id = \"a\"\nargv = [\"/a\"")).contains("unterminated"));
        let twice = format!(
            "{}[[command]]\nid = \"a\"\nargv = [\"/b\"]\n",
            with_cmd("id = \"a\"\nargv = [\"/a\"]")
        );
        assert!(err(&twice).contains("duplicate id"));
        assert!(
            err("allowed_uids = [99999999999]\n[[command]]\nid = \"a\"\nargv = [\"/a\"]")
                .contains("range")
        );
    }

    fn stat(uid: u32, mode: u32, dir: bool) -> Stat {
        Stat {
            uid,
            mode,
            regular: !dir,
            dir,
        }
    }

    #[test]
    fn owner_check() {
        let dir = stat(0, 0o40_755, true);
        assert_eq!(check_owner(&stat(0, 0o100_644, false), &dir, 0), Ok(()));
        assert!(
            check_owner(&stat(1000, 0o100_644, false), &dir, 0)
                .expect_err("not root")
                .contains("owned by uid 1000")
        );
        assert!(
            check_owner(&stat(0, 0o100_664, false), &dir, 0)
                .expect_err("group writable")
                .contains("writable")
        );
        assert!(
            check_owner(&stat(0, 0o100_646, false), &dir, 0)
                .expect_err("other writable")
                .contains("writable")
        );
        assert!(
            check_owner(&stat(0, 0o100_644, false), &stat(1000, 0o40_755, true), 0)
                .expect_err("dir not root")
                .contains("its directory is owned")
        );
        assert!(
            check_owner(&stat(0, 0o100_644, false), &stat(0, 0o41_777, true), 0)
                .expect_err("dir writable")
                .contains("its directory is writable")
        );
        assert!(
            check_owner(&stat(0, 0o40_755, true), &dir, 0)
                .expect_err("a directory")
                .contains("regular")
        );
        // The daemon's own uid is the owner it asks for: a test as a normal user passes.
        assert_eq!(
            check_owner(
                &stat(1000, 0o100_600, false),
                &stat(1000, 0o40_700, true),
                1000
            ),
            Ok(())
        );
    }
}
