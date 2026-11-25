//! The five C2 hive tools: their schemas, the check of their arguments, the argv of the hive's
//! CLI each call becomes, and the C2 output each CLI answer maps to (ADR-0071 sections 4 and 5).

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c2::{
    Ack, CardRef, CardStatus, HiveCancelInput, HiveCreateInput, HiveCreateInputKind,
    HiveCreateOutput, HiveListInput, HiveListOutput, HiveSteerInput,
};

use crate::config::Settings;
use crate::hive::Exit;

/// The tool names in the order `tools/list` serves them.
pub const NAMES: [&str; 5] = [
    "hive_create",
    "hive_list",
    "hive_show",
    "hive_steer",
    "hive_cancel",
];
/// Cards `hive_list` answers when the call gives no `limit`.
pub const DEFAULT_LIST_LIMIT: usize = 20;
/// The most characters of a line taken from the CLI's answer.
pub const MAX_LINE_CHARS: usize = 500;
/// The most characters of a title in an answer.
pub const MAX_TITLE_CHARS: usize = 200;
/// The most characters of the CLI's stderr line in an error.
pub const MAX_ERROR_CHARS: usize = 300;
/// Words an acceptance may not be on its own, as the hive's `VAGUE`.
const VAGUE: [&str; 9] = [
    "готово",
    "сделано",
    "done",
    "ok",
    "ок",
    "todo",
    "tbd",
    "-",
    "",
];
/// The acceptances that prove nothing without a review, as the hive's `empty_acceptance`.
const EMPTY_ACCEPTANCES: [&[&str]; 4] = [
    &["test", "-s", "REPORT.md"],
    &["test", "-s", "./REPORT.md"],
    &["[", "-s", "REPORT.md", "]"],
    &["[", "-s", "./REPORT.md", "]"],
];

/// The JSON of a generated C2 output.
macro_rules! to_value {
    ($value:expr) => {
        serde_json::to_value($value).unwrap_or(Value::Null)
    };
}

/// The arguments decoded into their generated C2 type.
macro_rules! typed {
    ($name:expr, $args:expr) => {
        serde_json::from_value($args)
            .map_err(|e| Refusal(format!("{}: invalid arguments: {e}", $name)))
    };
}

/// One checked call.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `hive_create`.
    Create(HiveCreateInput),
    /// `hive_list`.
    List(HiveListInput),
    /// `hive_show`.
    Show(CardRef),
    /// `hive_steer`.
    Steer(HiveSteerInput),
    /// `hive_cancel`.
    Cancel(HiveCancelInput),
}

impl Call {
    /// The tool's name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Create(_) => "hive_create",
            Self::List(_) => "hive_list",
            Self::Show(_) => "hive_show",
            Self::Steer(_) => "hive_steer",
            Self::Cancel(_) => "hive_cancel",
        }
    }

    /// The card the call names, if any.
    pub fn card(&self) -> Option<&str> {
        match self {
            Self::Show(c) => Some(&c.card_id),
            Self::Steer(c) => Some(&c.card_id),
            Self::Cancel(c) => Some(&c.card_id),
            Self::Create(_) | Self::List(_) => None,
        }
    }
}

/// Why a call's arguments are refused or its run failed: the text the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(text: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(text.into()))
}

/// The call of tool `name` with `arguments`; `None` for a tool this server does not have.
///
/// # Errors
/// [`Refusal`] naming the first rule the arguments break.
pub fn parse(name: &str, arguments: &Value) -> Option<Result<Call, Refusal>> {
    let args = match arguments {
        Value::Null => Value::Object(Map::new()),
        other => other.clone(),
    };
    let decode = |what| -> Result<_, Refusal> {
        if !args.is_object() {
            return refuse(format!("{what}: the arguments must be an object"));
        }
        Ok(args.clone())
    };
    let call = match name {
        "hive_create" => decode(name).and_then(|a| typed!(name, a).and_then(create)),
        "hive_list" => decode(name).and_then(|a| typed!(name, a).and_then(list)),
        "hive_show" => decode(name).and_then(|a| typed!(name, a).and_then(show)),
        "hive_steer" => decode(name).and_then(|a| typed!(name, a).and_then(steer)),
        "hive_cancel" => decode(name).and_then(|a| typed!(name, a).and_then(cancel)),
        _ => return None,
    };
    Some(call)
}

/// `text` within `min..=max` characters with no NUL.
fn bounded(field: &str, text: &str, min: usize, max: usize) -> Result<(), Refusal> {
    let count = text.chars().count();
    if count < min || count > max {
        return refuse(format!("{field} must be {min} to {max} characters"));
    }
    if text.contains('\0') {
        return refuse(format!("{field} must not hold a NUL character"));
    }
    Ok(())
}

/// `text` holds something besides whitespace.
fn not_blank(field: &str, text: &str) -> Result<(), Refusal> {
    if text.trim().is_empty() {
        return refuse(format!("{field} is required"));
    }
    Ok(())
}

/// Whether `id` is a card id, `h_` and 8 lowercase hex digits.
pub fn is_card_id(id: &str) -> bool {
    id.len() == 10
        && id.starts_with("h_")
        && id[2..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn card_id(id: &str) -> Result<(), Refusal> {
    if is_card_id(id) {
        Ok(())
    } else {
        refuse("card_id must be h_ followed by 8 lowercase hex digits")
    }
}

/// Whether `text` matches C2's `^[A-Za-z0-9][A-Za-z0-9._:@/+-]{0,127}$` of `source` and
/// `idem_key`.
pub fn is_label(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@/+-".contains(b))
}

fn create(input: HiveCreateInput) -> Result<Call, Refusal> {
    bounded("title", &input.title, 1, 200)?;
    not_blank("title", &input.title)?;
    bounded("goal", &input.goal, 1, 8000)?;
    not_blank("goal", &input.goal)?;
    if let Some(details) = &input.details {
        bounded("details", details, 0, 32000)?;
    }
    if !(1..=5).contains(&input.step) {
        return refuse("step must be 1..5");
    }
    if !is_label(&input.source) {
        return refuse(
            "source must be 1 to 128 of A-Z a-z 0-9 . _ : @ / + - starting with a letter or digit",
        );
    }
    if let Some(key) = &input.idem_key
        && !is_label(key)
    {
        return refuse(
            "idem_key must be 1 to 128 of A-Z a-z 0-9 . _ : @ / + - starting with a letter or digit",
        );
    }
    match (&input.acceptance, &input.accept_review) {
        (Some(_), Some(_)) => {
            return refuse("give exactly one of acceptance and accept_review, not both");
        }
        (None, None) => return refuse("one of acceptance and accept_review is required"),
        (Some(acceptance), None) => {
            bounded("acceptance", acceptance, 1, 2000)?;
            let lowered = acceptance.trim().to_lowercase();
            if VAGUE.contains(&lowered.as_str()) {
                return refuse("acceptance must be a shell command whose exit 0 means done");
            }
            let words: Vec<&str> = acceptance
                .trim()
                .trim_end_matches(';')
                .split_whitespace()
                .collect();
            if EMPTY_ACCEPTANCES.contains(&words.as_slice()) && input.review != Some(true) {
                return refuse("test -s REPORT.md requires review");
            }
        }
        (None, Some(criteria)) => {
            bounded("accept_review", criteria, 1, 4000)?;
            not_blank("accept_review", criteria)?;
        }
    }
    Ok(Call::Create(input))
}

fn list(input: HiveListInput) -> Result<Call, Refusal> {
    if let Some(limit) = input.limit
        && !(1..=200).contains(&limit)
    {
        return refuse("limit must be 1 to 200");
    }
    Ok(Call::List(input))
}

fn show(input: CardRef) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    Ok(Call::Show(input))
}

fn steer(input: HiveSteerInput) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    bounded("note", &input.note, 1, 4000)?;
    not_blank("note", &input.note)?;
    Ok(Call::Steer(input))
}

fn cancel(input: HiveCancelInput) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    bounded("reason", &input.reason, 1, 1000)?;
    not_blank("reason", &input.reason)?;
    Ok(Call::Cancel(input))
}

fn kind_name(kind: HiveCreateInputKind) -> &'static str {
    kind.as_str()
}

/// The arguments of the hive's CLI after its command for `call`: every value as one
/// `--flag=value` word, positionals after `--`.
pub fn argv(call: &Call, settings: &Settings) -> Vec<String> {
    let flag = |name: &str, value: &str| format!("--{name}={value}");
    let mut args: Vec<String> = Vec::new();
    match call {
        Call::Create(c) => {
            args.extend(["create".into(), "--json".into()]);
            args.push(flag("title", &c.title));
            args.push(flag("goal", &c.goal));
            if let Some(acceptance) = &c.acceptance {
                args.push(flag("acceptance", acceptance));
            }
            if let Some(criteria) = &c.accept_review {
                args.push(flag("accept-review", criteria));
            }
            args.push(flag("step", &c.step.to_string()));
            args.push(flag("source", &c.source));
            args.push(flag("created-by", &settings.actor));
            args.push(flag(
                "kind",
                kind_name(c.kind.unwrap_or(HiveCreateInputKind::OwnerTask)),
            ));
            if let Some(details) = c.details.as_deref().filter(|d| !d.is_empty()) {
                args.push(flag("body", details));
            }
            if c.review == Some(true) {
                args.push("--review".into());
            }
            if settings.hold || c.hold == Some(true) {
                args.push("--hold".into());
            }
            match c.owner_visible {
                Some(true) => args.push("--owner-visible".into()),
                Some(false) => args.push("--no-owner-visible".into()),
                None => {}
            }
            if let Some(key) = &c.idem_key {
                args.push(flag("idem", key));
            }
        }
        Call::List(l) => {
            args.extend(["list".into(), "--json".into()]);
            if let Some(state) = l.state {
                args.push(flag("state", state.as_str()));
            }
        }
        Call::Show(s) => {
            args.extend([
                "show".into(),
                "--json".into(),
                "--".into(),
                s.card_id.clone(),
            ]);
        }
        Call::Steer(s) => {
            args.extend([
                "steer".into(),
                flag("by", &settings.actor),
                "--".into(),
                s.card_id.clone(),
                s.note.clone(),
            ]);
        }
        Call::Cancel(c) => {
            args.extend([
                "cancel".into(),
                flag("reason", &c.reason),
                flag("by", &settings.actor),
                "--".into(),
                c.card_id.clone(),
            ]);
        }
    }
    args
}

/// `text` cut to at most `max` characters.
fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The first line of `text` with something in it, cut to `max` characters.
fn first_line(text: &str, max: usize) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| cut(l, max))
}

/// The tool error of a CLI run that did not succeed.
fn failed(call: &Call, exit: &Exit) -> Refusal {
    let stderr = String::from_utf8_lossy(&exit.stderr);
    let last = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or_default();
    if let Call::Show(s) = call
        && exit.status == Some(1)
        && last == "no such card"
    {
        return Refusal(format!("no such card {}", s.card_id));
    }
    // The CLI's refusal: its `hive: ` line and the lines it carries on (a `bash -n` message).
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if exit.status == Some(2)
        && let Some(at) = lines.iter().rposition(|l| l.starts_with("hive: "))
    {
        let text = lines[at..].join(" ");
        return Refusal(cut(text.trim_start_matches("hive: "), MAX_ERROR_CHARS));
    }
    let status = exit
        .status
        .map_or_else(|| "a signal".to_owned(), |s| format!("exit {s}"));
    Refusal(format!(
        "the hive CLI failed ({status}): {}",
        cut(last, MAX_ERROR_CHARS)
    ))
}

fn bad_answer(call: &Call, why: &str) -> Refusal {
    Refusal(format!(
        "{}: the hive CLI's answer is not understood: {why}",
        call.name()
    ))
}

/// The C2 output of `call` from the CLI's `exit`.
///
/// # Errors
/// [`Refusal`] for a failed run or an answer that does not fit the output.
pub fn answer(call: &Call, exit: &Exit) -> Result<Value, Refusal> {
    if exit.status != Some(0) {
        return Err(failed(call, exit));
    }
    let stdout = String::from_utf8_lossy(&exit.stdout);
    let out = match call {
        Call::Create(_) => {
            let line = stdout
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or_default();
            let card: Value =
                serde_json::from_str(line).map_err(|_| bad_answer(call, "no JSON line"))?;
            let id = card["id"].as_str().filter(|id| is_card_id(id));
            let (Some(id), Some(created), Some(state)) =
                (id, card["created"].as_bool(), card["state"].as_str())
            else {
                return Err(bad_answer(call, "no id, created or state"));
            };
            to_value!(&HiveCreateOutput {
                card_id: id.to_owned(),
                created,
                state: cut(state, 32),
            })
        }
        Call::List(l) => {
            let rows: Value =
                serde_json::from_str(&stdout).map_err(|_| bad_answer(call, "no JSON list"))?;
            let rows = rows
                .as_array()
                .ok_or_else(|| bad_answer(call, "no JSON list"))?;
            let limit = l
                .limit
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(DEFAULT_LIST_LIMIT);
            let cards = rows
                .iter()
                .take(limit)
                .map(|row| {
                    status(row).ok_or_else(|| bad_answer(call, "a row without id, state or round"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            to_value!(&HiveListOutput { cards })
        }
        Call::Show(_) => {
            let card: Value =
                serde_json::from_str(&stdout).map_err(|_| bad_answer(call, "no JSON card"))?;
            let mut out = status(&card).ok_or_else(|| bad_answer(call, "no id, state or round"))?;
            out.result_line = card["result"]
                .as_str()
                .and_then(|r| first_line(r, MAX_LINE_CHARS));
            out.report_line = card["runs"].as_array().and_then(|runs| {
                runs.iter()
                    .rev()
                    .filter_map(|r| r["report"].as_str())
                    .find_map(|r| first_line(r, MAX_LINE_CHARS))
            });
            to_value!(&out)
        }
        Call::Steer(_) | Call::Cancel(_) => to_value!(&Ack {
            ok: true,
            message: first_line(&stdout, MAX_LINE_CHARS),
        }),
    };
    Ok(out)
}

/// The `CardStatus` of a card row of `list --json` or `show --json`, without the report lines.
fn status(row: &Value) -> Option<CardStatus> {
    let id = row["id"].as_str().filter(|id| is_card_id(id))?;
    Some(CardStatus {
        card_id: id.to_owned(),
        state: cut(row["state"].as_str()?, 32),
        round: row["round"].as_u64()?,
        title: row["title"].as_str().map(|t| cut(t, MAX_TITLE_CHARS)),
        result_line: None,
        report_line: None,
    })
}

const CARD_ID: &str = "^h_[0-9a-f]{8}$";
const LABEL: &str = "^[A-Za-z0-9][A-Za-z0-9._:@/+-]{0,127}$";

fn card_id_schema() -> Value {
    json!({"type": "string", "description": "Card id, e.g. `h_48b892d9`.", "pattern": CARD_ID})
}

/// The input schema of each tool: the C2 definitions, `HiveCreateInput` without its top-level
/// `oneOf`, which the server enforces.
pub fn input_schemas() -> [(&'static str, Value); 5] {
    [
        ("hive_create", create_schema()),
        (
            "hive_list",
            json!({
                "type": "object",
                "description": "List cards.",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Filter by state; without it cancelled cards are left out.",
                        "enum": ["created", "queued", "running", "check", "review", "done", "blocked", "cancelled"]
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of cards; default 20.",
                        "minimum": 1,
                        "maximum": 200
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        ),
        (
            "hive_show",
            json!({
                "type": "object",
                "description": "Reference to a Hive card.",
                "properties": {"card_id": card_id_schema()},
                "required": ["card_id"],
                "additionalProperties": false
            }),
        ),
        (
            "hive_steer",
            json!({
                "type": "object",
                "description": "Send a steering note to the head working on a card.",
                "properties": {
                    "card_id": card_id_schema(),
                    "note": {
                        "type": "string",
                        "description": "Note text the head gets in its session or its next round.",
                        "minLength": 1,
                        "maxLength": 4000
                    }
                },
                "required": ["card_id", "note"],
                "additionalProperties": false
            }),
        ),
        (
            "hive_cancel",
            json!({
                "type": "object",
                "description": "Cancel a card; its active heads are stopped.",
                "properties": {
                    "card_id": card_id_schema(),
                    "reason": {
                        "type": "string",
                        "description": "Why, kept on the card.",
                        "minLength": 1,
                        "maxLength": 1000
                    }
                },
                "required": ["card_id", "reason"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn create_schema() -> Value {
    json!({
        "type": "object",
        "description": "Create a Hive card: the fields of `hive.py create` a delegation may set. Exactly one of `acceptance` and `accept_review`. The caller is not an input: the server sets `created_by` from its config. No field names a script, an executor, a model, a cadence, a delivery or a path.",
        "properties": {
            "title": {"type": "string", "description": "Short title.", "minLength": 1, "maxLength": 200},
            "goal": {"type": "string", "description": "What the head must achieve.", "minLength": 1, "maxLength": 8000},
            "details": {"type": "string", "description": "Further context for the head, the card's body (`--body`).", "maxLength": 32000},
            "acceptance": {"type": "string", "description": "Mechanical acceptance: a bash command run in the card workspace, exit 0 = pass. A bare `test -s REPORT.md` needs `review`.", "minLength": 1, "maxLength": 2000},
            "accept_review": {"type": "string", "description": "Review criteria a judge checks the report against, instead of `acceptance` (`--accept-review`); implies `review`.", "minLength": 1, "maxLength": 4000},
            "step": {"type": "integer", "description": "Difficulty step 1..5.", "minimum": 1, "maximum": 5},
            "source": {"type": "string", "description": "Where the request came from (`--source`), such as `msg:<id>`, `issue:<n>` or `manual`.", "pattern": LABEL},
            "kind": {"type": "string", "description": "Card class (`--kind`); default `owner_task`.", "enum": ["owner_task", "system", "followup"]},
            "review": {"type": "boolean", "description": "Independent review by the other vendor."},
            "hold": {"type": "boolean", "description": "Park the card until the owner runs `hive.py release`."},
            "owner_visible": {"type": "boolean", "description": "Notify the owner about the result; default by class."},
            "idem_key": {"type": "string", "description": "Idempotency key of the request: a second create with it returns the first card.", "pattern": LABEL}
        },
        "required": ["title", "goal", "step", "source"],
        "additionalProperties": false
    })
}

/// What each tool does, for the model.
fn description(name: &str) -> &'static str {
    match name {
        "hive_create" => {
            "Delegate a task to the hive: create a card that a hive head carries out. Give exactly one of `acceptance` (a bash check, exit 0 = done) and `accept_review` (criteria a judge checks the report against). Answers the card id and its state."
        }
        "hive_list" => "List hive cards, newest first, optionally of one state.",
        "hive_show" => {
            "Show a hive card: state, round, title, the first line of its result and of its last report."
        }
        "hive_steer" => {
            "Send a clarification to the head working on a hive card; a card without a live head gets it in its next round."
        }
        "hive_cancel" => "Cancel a hive card with a reason; its active heads are stopped.",
        _ => "",
    }
}

/// The `tools` of `tools/list`.
pub fn list_tools() -> Value {
    let tools: Vec<Value> = input_schemas()
        .into_iter()
        .map(|(name, schema)| {
            json!({
                "name": name,
                "description": description(name),
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    fn settings(hold: bool) -> Settings {
        Settings {
            listen: "127.0.0.1:1".parse().unwrap(),
            token: "t".repeat(32),
            command: vec!["/usr/bin/python3".into(), "/x/hive.py".into()],
            hive_config: PathBuf::from("/x/config.toml"),
            actor: "secretary".into(),
            timeout: Duration::from_secs(5),
            hold,
        }
    }

    fn base() -> Value {
        json!({"title": "T", "goal": "G", "acceptance": "test -f out.txt", "step": 2, "source": "msg:1"})
    }

    fn with(extra: &Value) -> Value {
        let mut v = base();
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    fn refused(name: &str, args: &Value) -> String {
        match parse(name, args).unwrap() {
            Err(Refusal(text)) => text,
            Ok(call) => panic!("accepted: {call:?}"),
        }
    }

    fn args_of(name: &str, args: &Value, hold: bool) -> Vec<String> {
        argv(&parse(name, args).unwrap().unwrap(), &settings(hold))
    }

    #[test]
    fn create_argv_puts_every_value_in_one_flag_word() {
        assert_eq!(
            args_of("hive_create", &base(), false),
            [
                "create",
                "--json",
                "--title=T",
                "--goal=G",
                "--acceptance=test -f out.txt",
                "--step=2",
                "--source=msg:1",
                "--created-by=secretary",
                "--kind=owner_task"
            ]
        );
        let full = with(&json!({
            "acceptance": null, "accept_review": "criteria", "details": "body", "kind": "system",
            "review": true, "hold": true, "owner_visible": false, "idem_key": "req-1"
        }));
        let mut full = full;
        full.as_object_mut().unwrap().remove("acceptance");
        assert_eq!(
            args_of("hive_create", &full, false),
            [
                "create",
                "--json",
                "--title=T",
                "--goal=G",
                "--accept-review=criteria",
                "--step=2",
                "--source=msg:1",
                "--created-by=secretary",
                "--kind=system",
                "--body=body",
                "--review",
                "--hold",
                "--no-owner-visible",
                "--idem=req-1"
            ]
        );
    }

    #[test]
    fn config_hold_holds_every_card() {
        assert!(args_of("hive_create", &base(), true).contains(&"--hold".to_owned()));
        assert!(!args_of("hive_create", &base(), false).contains(&"--hold".to_owned()));
    }

    #[test]
    fn option_like_values_stay_values() {
        let args = args_of(
            "hive_create",
            &with(&json!({"title": "--script=/tmp/x", "goal": "--executor dots"})),
            false,
        );
        assert!(args.contains(&"--title=--script=/tmp/x".to_owned()));
        assert!(args.contains(&"--goal=--executor dots".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--script") || a.starts_with("--executor"))
        );
        assert_eq!(
            args_of(
                "hive_steer",
                &json!({"card_id": "h_0123abcd", "note": "--by=owner"}),
                false
            ),
            ["steer", "--by=secretary", "--", "h_0123abcd", "--by=owner"]
        );
        assert_eq!(
            args_of(
                "hive_cancel",
                &json!({"card_id": "h_0123abcd", "reason": "-x; rm -rf /"}),
                false
            ),
            [
                "cancel",
                "--reason=-x; rm -rf /",
                "--by=secretary",
                "--",
                "h_0123abcd"
            ]
        );
        assert_eq!(
            args_of("hive_show", &json!({"card_id": "h_0123abcd"}), false),
            ["show", "--json", "--", "h_0123abcd"]
        );
        assert_eq!(
            args_of("hive_list", &json!({"state": "queued"}), false),
            ["list", "--json", "--state=queued"]
        );
    }

    #[test]
    fn every_unexposed_flag_is_refused() {
        for field in [
            "created_by",
            "script",
            "script_timeout",
            "executor",
            "assignee",
            "on_unavailable",
            "model",
            "effort",
            "panel",
            "body_file",
            "deny_tools",
            "every",
            "cron",
            "tz",
            "misfire",
            "start_window",
            "max_runs",
            "until",
            "overlap",
            "deliver",
            "tag",
            "type",
            "priority",
            "gate_timeout",
            "not_before",
            "recheck_every",
            "blocked_by",
            "epic",
            "parent",
            "optional",
            "followup_of",
            "wait",
            "config",
            "body",
        ] {
            let text = refused("hive_create", &with(&json!({ field: "x" })));
            assert!(text.contains("unknown field"), "{field}: {text}");
        }
        assert!(
            refused(
                "hive_steer",
                &json!({"card_id": "h_0123abcd", "note": "n", "by": "owner"})
            )
            .contains("unknown field")
        );
        assert!(
            refused(
                "hive_cancel",
                &json!({"card_id": "h_0123abcd", "reason": "r", "by": "owner"})
            )
            .contains("unknown field")
        );
        assert!(
            refused(
                "hive_cancel",
                &json!({"card_id": "h_0123abcd", "reason": "r", "steward": true})
            )
            .contains("unknown field")
        );
    }

    #[test]
    fn create_mirrors_the_hive_rules() {
        let mut none = base();
        none.as_object_mut().unwrap().remove("acceptance");
        assert_eq!(
            refused("hive_create", &none),
            "one of acceptance and accept_review is required"
        );
        assert!(refused("hive_create", &with(&json!({"accept_review": "c"}))).contains("not both"));
        assert!(
            refused("hive_create", &with(&json!({"acceptance": " Готово "})))
                .contains("shell command")
        );
        assert!(
            refused("hive_create", &with(&json!({"acceptance": "done"}))).contains("shell command")
        );
        assert_eq!(
            refused(
                "hive_create",
                &with(&json!({"acceptance": "test -s REPORT.md;"}))
            ),
            "test -s REPORT.md requires review"
        );
        assert!(
            parse(
                "hive_create",
                &with(&json!({"acceptance": "test -s REPORT.md", "review": true}))
            )
            .unwrap()
            .is_ok()
        );
        assert_eq!(
            refused("hive_create", &with(&json!({"title": "  "}))),
            "title is required"
        );
        assert!(
            refused("hive_create", &with(&json!({"title": "x".repeat(201)})))
                .contains("title must be")
        );
        assert!(refused("hive_create", &with(&json!({"goal": "a\u{0}b"}))).contains("NUL"));
        assert!(refused("hive_create", &with(&json!({"step": 6}))).contains("step"));
        assert!(refused("hive_create", &with(&json!({"step": 0}))).contains("step"));
        assert!(refused("hive_create", &with(&json!({"source": "-x"}))).contains("source"));
        assert!(refused("hive_create", &with(&json!({"source": "a b"}))).contains("source"));
        assert!(refused("hive_create", &with(&json!({"idem_key": "--x"}))).contains("idem_key"));
        assert!(
            refused("hive_create", &with(&json!({"kind": "series_run"})))
                .contains("invalid arguments")
        );
        assert!(refused("hive_create", &json!("text")).contains("object"));
        assert!(parse("hive_nope", &json!({})).is_none());
    }

    #[test]
    fn card_ids_are_checked() {
        for bad in ["--by=owner", "h_0123ABCD", "h_0123abc", "x_0123abcd", ""] {
            assert!(
                refused("hive_show", &json!({"card_id": bad})).contains("card_id"),
                "{bad}"
            );
            assert!(
                refused("hive_steer", &json!({"card_id": bad, "note": "n"})).contains("card_id")
            );
            assert!(
                refused("hive_cancel", &json!({"card_id": bad, "reason": "r"})).contains("card_id")
            );
        }
        assert!(
            refused("hive_steer", &json!({"card_id": "h_0123abcd", "note": " "})).contains("note")
        );
        assert!(
            refused(
                "hive_cancel",
                &json!({"card_id": "h_0123abcd", "reason": ""})
            )
            .contains("reason")
        );
        assert!(refused("hive_list", &json!({"limit": 201})).contains("limit"));
        assert!(refused("hive_list", &json!({"state": "finished"})).contains("invalid"));
    }

    fn exit(status: i32, stdout: &str, stderr: &str) -> Exit {
        Exit {
            status: Some(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn answers_map_to_c2_outputs() {
        let create = parse("hive_create", &base()).unwrap().unwrap();
        assert_eq!(
            answer(&create, &exit(0, "{\"id\": \"h_0123abcd\", \"created\": false, \"state\": \"queued\", \"card_class\": \"owner_task\"}\n", "")).unwrap(),
            json!({"card_id": "h_0123abcd", "created": false, "state": "queued"})
        );
        assert_eq!(
            answer(
                &create,
                &exit(
                    2,
                    "",
                    "hive: card rejected: acceptance is not valid bash: x\n"
                )
            ),
            Err(Refusal(
                "card rejected: acceptance is not valid bash: x".into()
            ))
        );
        assert_eq!(
            answer(
                &create,
                &exit(
                    1,
                    "",
                    "Traceback\n  File x\nsqlite3.OperationalError: locked\n"
                )
            ),
            Err(Refusal(
                "the hive CLI failed (exit 1): sqlite3.OperationalError: locked".into()
            ))
        );
        let show = parse("hive_show", &json!({"card_id": "h_0123abcd"}))
            .unwrap()
            .unwrap();
        assert_eq!(
            answer(&show, &exit(1, "", "no such card\n")),
            Err(Refusal("no such card h_0123abcd".into()))
        );
        let card = json!({"id": "h_0123abcd", "state": "done", "round": 2, "title": "T",
            "result": "\nAll good\nmore", "runs": [{"report": "# first"}, {"report": null}, {"report": "\n# last\nbody"}, {"report": ""}]});
        assert_eq!(
            answer(&show, &exit(0, &card.to_string(), "")).unwrap(),
            json!({"card_id": "h_0123abcd", "state": "done", "round": 2, "title": "T", "result_line": "All good", "report_line": "# last"})
        );
        let list = parse("hive_list", &json!({"limit": 1})).unwrap().unwrap();
        let rows = json!([{"id": "h_0123abcd", "state": "queued", "round": 0, "title": "A"}, {"id": "h_4567abcd", "state": "done", "round": 1, "title": "B"}]);
        assert_eq!(
            answer(&list, &exit(0, &rows.to_string(), "")).unwrap(),
            json!({"cards": [{"card_id": "h_0123abcd", "state": "queued", "round": 0, "title": "A"}]})
        );
        let steer = parse("hive_steer", &json!({"card_id": "h_0123abcd", "note": "n"}))
            .unwrap()
            .unwrap();
        assert_eq!(
            answer(
                &steer,
                &exit(0, "h_0123abcd steer 2 recorded, not delivered yet\n", "")
            )
            .unwrap(),
            json!({"ok": true, "message": "h_0123abcd steer 2 recorded, not delivered yet"})
        );
        assert!(
            answer(
                &steer,
                &exit(
                    2,
                    "",
                    "hive: steer rejected: card h_0123abcd is epic; no worker receives a steer\n"
                )
            )
            .unwrap_err()
            .0
            .starts_with("steer rejected: ")
        );
    }

    /// `contracts/c2-mcp-tools.json`, read at test time.
    fn c2() -> Value {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/c2-mcp-tools.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn served_schemas_are_the_c2_definitions() {
        let doc = c2();
        let defs = &doc["$defs"];
        let tools = doc["x-tools"].as_array().unwrap();
        for (name, schema) in input_schemas() {
            let tool = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("{name} not in C2"));
            let def = tool["input"]
                .as_str()
                .unwrap()
                .trim_start_matches("#/$defs/");
            let mut want = defs[def].clone();
            if name == "hive_create" {
                assert_eq!(
                    want.as_object_mut().unwrap().remove("oneOf"),
                    Some(json!([{"required": ["acceptance"]}, {"required": ["accept_review"]}]))
                );
            }
            assert_eq!(schema, want, "{name}");
        }
        let c2_hive: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .filter(|n| n.starts_with("hive_"))
            .collect();
        let mut ours = NAMES.to_vec();
        ours.sort_unstable();
        let mut theirs = c2_hive.clone();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
    }
}
