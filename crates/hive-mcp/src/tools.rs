//! The C2 hive tools: their schemas, the check of their arguments, the argv of the hive's CLI
//! each call becomes, and the C2 output each CLI answer maps to (ADR-0071 sections 4 and 5; the
//! read-only snapshot tools `hive_inspect`, `hive_health` and `hive_file` of ADR-0086; the
//! reminder verbs of `hive_reminder`, ADR-0101).

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c2::{
    Ack, CardRef, CardStatus, Empty, HiveCancelInput, HiveCreateInput, HiveCreateInputKind,
    HiveCreateOutput, HiveFileInput, HiveFileOutput, HiveHealthOutput, HiveInspectOutput,
    HiveListInput, HiveListInputCardClass, HiveListInputPlacement, HiveListInputState,
    HiveListOutput, HiveReminderInput, HiveReminderInputAction, HiveResolveInput, HiveSteerInput,
};

use crate::config::Settings;
use crate::hive::Exit;

/// The tool names in the order `tools/list` serves them.
pub const NAMES: [&str; 10] = [
    "hive_create",
    "hive_list",
    "hive_show",
    "hive_steer",
    "hive_cancel",
    "hive_resolve",
    "hive_reminder",
    "hive_inspect",
    "hive_health",
    "hive_file",
];
/// The most bytes `hive_file` reads in one call, as the hive's `inspect.FILE_CHARS`.
pub const MAX_FILE_BYTES: u64 = 262_144;
/// The most characters of `hive_inspect`'s `waiting_on`.
pub const MAX_WAITING_CHARS: usize = 500;
/// Cards `hive_list` answers when the call gives no `limit`.
pub const DEFAULT_LIST_LIMIT: usize = 20;
/// The most characters of a line taken from the CLI's answer.
pub const MAX_LINE_CHARS: usize = 500;
/// The most characters of a title in an answer.
pub const MAX_TITLE_CHARS: usize = 200;
/// The most characters of the CLI's stderr line in an error.
pub const MAX_ERROR_CHARS: usize = 300;
/// The most bytes of UTF-8 of a reminder text, as the hive's `REMINDER_MAX_BYTES`.
pub const MAX_REMINDER_BYTES: usize = 32_768;
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
    Create(Box<HiveCreateInput>),
    /// `hive_list`.
    List(HiveListInput),
    /// `hive_show`.
    Show(CardRef),
    /// `hive_steer`.
    Steer(HiveSteerInput),
    /// `hive_cancel`.
    Cancel(HiveCancelInput),
    /// `hive_resolve`.
    Resolve(HiveResolveInput),
    /// `hive_reminder`.
    Reminder(HiveReminderInput),
    /// `hive_inspect`.
    Inspect(CardRef),
    /// `hive_health`.
    Health,
    /// `hive_file`.
    File(HiveFileInput),
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
            Self::Resolve(_) => "hive_resolve",
            Self::Reminder(_) => "hive_reminder",
            Self::Inspect(_) => "hive_inspect",
            Self::Health => "hive_health",
            Self::File(_) => "hive_file",
        }
    }

    /// The card the call names, if any.
    pub fn card(&self) -> Option<&str> {
        match self {
            Self::Show(c) | Self::Inspect(c) => Some(&c.card_id),
            Self::Steer(c) => Some(&c.card_id),
            Self::Cancel(c) => Some(&c.card_id),
            Self::Resolve(c) => Some(&c.card_id),
            Self::Reminder(c) => Some(&c.card_id),
            Self::File(c) => Some(&c.card_id),
            Self::Create(_) | Self::List(_) | Self::Health => None,
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
        "hive_resolve" => decode(name).and_then(|a| typed!(name, a).and_then(resolve)),
        "hive_reminder" => decode(name).and_then(|a| typed!(name, a).and_then(reminder)),
        "hive_inspect" => decode(name).and_then(|a| typed!(name, a).and_then(inspect)),
        "hive_health" => decode(name).and_then(|a| typed!(name, a).map(|_: Empty| Call::Health)),
        "hive_file" => decode(name).and_then(|a| typed!(name, a).and_then(file)),
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
    timing(&input)?;
    if input.urgent == Some(true)
        && (input
            .kind
            .is_some_and(|k| k != HiveCreateInputKind::OwnerTask)
            || input.every.is_some()
            || input.cron.is_some()
            || input.reminder.is_some())
    {
        return refuse(
            "urgent applies only to an owner_task card run by a head: no series or reminder",
        );
    }
    if let Some(text) = &input.reminder {
        reminder_text("reminder", text)?;
        if input.accept_review.is_some()
            || input.review == Some(true)
            || input.recheck_every.is_some()
        {
            return refuse(
                "a reminder runs no head: give acceptance `true`, and no accept_review, review or recheck",
            );
        }
        let timed = input.not_before.is_some() || input.every.is_some() || input.cron.is_some();
        if !timed && input.hold != Some(true) {
            return refuse("a reminder needs not_before, every, cron or hold");
        }
    }
    Ok(Call::Create(Box::new(input)))
}

/// A reminder text as the hive takes it, verbatim: not blank, at most [`MAX_REMINDER_BYTES`]
/// of UTF-8, and no NUL, which no argv word can hold.
fn reminder_text(field: &str, text: &str) -> Result<(), Refusal> {
    not_blank(field, text)?;
    if text.len() > MAX_REMINDER_BYTES {
        return refuse(format!(
            "{field} must be at most {MAX_REMINDER_BYTES} bytes of UTF-8"
        ));
    }
    if text.contains('\0') {
        return refuse(format!("{field} must not hold a NUL character"));
    }
    Ok(())
}

/// The most seconds of `every` and `recheck_every`: a year of 366 days.
pub const MAX_DURATION_SECS: u64 = 366 * 86_400;
/// The most cards of a series (`max_runs`).
pub const MAX_RUNS: u64 = 10_000;
/// The most rechecks of a card (`recheck_max`).
pub const MAX_RECHECKS: u64 = 1_000;

/// The seconds of a duration as the hive's `parse_duration` reads it, `^[1-9][0-9]*[smhd]$`,
/// here at most [`MAX_DURATION_SECS`].
pub fn duration_secs(text: &str) -> Option<u64> {
    let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
    let size = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    if digits.is_empty()
        || digits.len() > 7
        || digits.starts_with('0')
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let secs = digits.parse::<u64>().ok()?.checked_mul(size)?;
    (secs <= MAX_DURATION_SECS).then_some(secs)
}

/// The Unix time of an RFC 3339 time with an offset, `YYYY-MM-DDTHH:MM:SS[.f{1,6}](Z|+HH:MM|-HH:MM)`:
/// a subset of what the hive's `parse_timestamp` (`datetime.fromisoformat` with an offset)
/// accepts, with every field in range. A time without an offset is `None`, as the hive refuses it.
pub fn rfc3339_secs(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    if !(at(4, b'-') && at(7, b'-') && at(10, b'T') && at(13, b':') && at(16, b':')) {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if year < 1 || !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut i = 19;
    if at(i, b'.') {
        let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if !(1..=6).contains(&digits) {
            return None;
        }
        i += 1 + digits;
    }
    let offset = match b.get(i..)? {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (h, m) = (num(i + 1, 2)?, num(i + 4, 2)?);
            if h > 23 || m > 59 {
                return None;
            }
            let secs = h * 3_600 + m * 60;
            if *sign == b'-' { -secs } else { secs }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Whether `text` is five cron fields of `A-Z a-z 0-9 * / , ? # -` separated by single spaces,
/// at most 100 characters. The hive (`cronsim`) decides whether the fields mean a schedule.
pub fn is_cron(text: &str) -> bool {
    text.len() <= 100
        && text.split(' ').count() == 5
        && text.split(' ').all(|field| {
            !field.is_empty()
                && field
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"*/,?#-".contains(&b))
        })
}

/// Whether `text` has the shape of an IANA zone name, such as `UTC` or `Europe/Moscow`:
/// `^[A-Za-z][A-Za-z0-9_+-]{0,31}(/[A-Za-z0-9_+-]{1,32}){0,2}$`. The hive decides whether the
/// zone exists.
pub fn is_zone(text: &str) -> bool {
    let parts: Vec<&str> = text.split('/').collect();
    let word = |p: &str| {
        (1..=32).contains(&p.len())
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
    };
    parts.len() <= 3
        && parts[0]
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && parts.iter().all(|p| word(p))
}

fn time_field(field: &str, value: Option<&String>) -> Result<Option<i64>, Refusal> {
    value
        .map(|v| {
            rfc3339_secs(v).ok_or_else(|| {
                Refusal(format!(
                    "{field} must be an RFC 3339 time with an offset, such as 2026-11-27T09:00:00+03:00"
                ))
            })
        })
        .transpose()
}

fn duration_field(field: &str, value: Option<&String>) -> Result<(), Refusal> {
    if let Some(v) = value
        && duration_secs(v).is_none()
    {
        return refuse(format!(
            "{field} must be a positive duration Ns, Nm, Nh or Nd of at most 366 days, such as 15m"
        ));
    }
    Ok(())
}

fn count_field(field: &str, value: Option<u64>, max: u64) -> Result<(), Refusal> {
    if let Some(n) = value
        && !(1..=max).contains(&n)
    {
        return refuse(format!("{field} must be 1 to {max}"));
    }
    Ok(())
}

/// The timing fields of a create, checked as the hive's CLI and `api.create` check them
/// (ADR-0082): their formats, then the pairs that exclude or need each other.
fn timing(input: &HiveCreateInput) -> Result<(), Refusal> {
    let not_before = time_field("not_before", input.not_before.as_ref())?;
    let until = time_field("until", input.until.as_ref())?;
    let recheck_until = time_field("recheck_until", input.recheck_until.as_ref())?;
    duration_field("every", input.every.as_ref())?;
    duration_field("recheck_every", input.recheck_every.as_ref())?;
    if let Some(cron) = &input.cron
        && !is_cron(cron)
    {
        return refuse(
            "cron must be five fields (minute hour day month weekday) of A-Z a-z 0-9 * / , ? # - separated by single spaces",
        );
    }
    if let Some(tz) = &input.tz
        && !is_zone(tz)
    {
        return refuse("tz must be an IANA time zone name, such as Europe/Moscow");
    }
    count_field("max_runs", input.max_runs, MAX_RUNS)?;
    count_field("recheck_max", input.recheck_max, MAX_RECHECKS)?;

    let series = input.every.is_some() || input.cron.is_some();
    let recheck = input.recheck_every.is_some();
    if input.every.is_some() && input.cron.is_some() {
        return refuse("give at most one of every and cron");
    }
    if input.tz.is_some() && input.cron.is_none() {
        return refuse("tz is the zone of cron: give it only with cron");
    }
    if !series && (input.until.is_some() || input.max_runs.is_some()) {
        return refuse("until and max_runs bound a series: give them only with every or cron");
    }
    if input.every.is_some() && input.until.is_none() && input.max_runs.is_none() {
        return refuse("every needs until or max_runs");
    }
    if !recheck && (input.recheck_until.is_some() || input.recheck_max.is_some()) {
        return refuse("recheck_until and recheck_max need recheck_every");
    }
    if recheck && input.recheck_until.is_none() && input.recheck_max.is_none() {
        return refuse("recheck_every needs recheck_until or recheck_max");
    }
    if series && recheck {
        return refuse("a series (every or cron) takes no recheck_every");
    }
    if series && input.kind.is_some() {
        return refuse("leave kind out with every or cron: a series is always series_run");
    }
    if input.hold == Some(true) && (not_before.is_some() || series) {
        return refuse(
            "hold parks a card until the owner releases it: give it without not_before, every or cron",
        );
    }
    if let (Some(start), Some(end)) = (not_before, until)
        && start >= end
    {
        return refuse("until must be after not_before");
    }
    if let (Some(start), Some(end)) = (not_before, recheck_until)
        && start >= end
    {
        return refuse("recheck_until must be after not_before");
    }
    Ok(())
}

/// The checks of a parsed call that depend on the server's config: with `hive_mcp.hold` every
/// card waits for the owner's `hive.py release`, which a timed card cannot (ADR-0082).
///
/// # Errors
/// [`Refusal`] of a timed create while `hive_mcp.hold` is on.
pub fn admit(call: &Call, settings: &Settings) -> Result<(), Refusal> {
    if let Call::Create(c) = call
        && settings.hold
        && (c.not_before.is_some() || c.every.is_some() || c.cron.is_some())
    {
        return refuse(
            "this hive holds every card until the owner releases it (hive_mcp.hold), so a timed card cannot be made: leave out not_before, every and cron, or ask the owner",
        );
    }
    Ok(())
}

fn list(input: HiveListInput) -> Result<Call, Refusal> {
    if let Some(limit) = input.limit
        && !(1..=200).contains(&limit)
    {
        return refuse("limit must be 1 to 200");
    }
    for id in [&input.parent, &input.followup_of].into_iter().flatten() {
        card_id(id)?;
    }
    if let Some(actor) = &input.created_by
        && !is_label(actor)
    {
        return refuse("created_by must be a label: letters, digits and ._:@/+-, at most 128");
    }
    if let Some(query) = &input.query {
        bounded("query", query, 1, 200)?;
        not_blank("query", query)?;
    }
    Ok(Call::List(input))
}

fn inspect(input: CardRef) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    Ok(Call::Inspect(input))
}

fn file(input: HiveFileInput) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    bounded("path", &input.path, 1, 512)?;
    if input.path.starts_with('/') {
        return refuse("path must be relative to the card's workspace");
    }
    if input.path.split('/').any(|p| p == "..") {
        return refuse("path must stay inside the card's workspace: no `..`");
    }
    if let Some(limit) = input.limit
        && !(1..=MAX_FILE_BYTES).contains(&limit)
    {
        return refuse("limit must be 1 to 262144");
    }
    Ok(Call::File(input))
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

fn resolve(input: HiveResolveInput) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    bounded("text", &input.text, 1, 4000)?;
    not_blank("text", &input.text)?;
    Ok(Call::Resolve(input))
}

fn reminder(input: HiveReminderInput) -> Result<Call, Refusal> {
    card_id(&input.card_id)?;
    match (input.action, &input.text, &input.reason) {
        (HiveReminderInputAction::Claim, None, None) => {}
        (HiveReminderInputAction::Claim, _, _) => {
            return refuse("claim takes no text and no reason");
        }
        (HiveReminderInputAction::Send, text, None) => {
            if let Some(text) = text {
                reminder_text("text", text)?;
            }
        }
        (HiveReminderInputAction::Send, _, Some(_)) => {
            return refuse("send takes no reason: give the edited text as text, or nothing");
        }
        (HiveReminderInputAction::Withdraw, None, Some(reason)) => {
            bounded("reason", reason, 1, 1000)?;
            not_blank("reason", reason)?;
        }
        (HiveReminderInputAction::Withdraw, Some(_), _) => {
            return refuse("withdraw takes no text: nothing is sent");
        }
        (HiveReminderInputAction::Withdraw, None, None) => {
            return refuse("withdraw needs a reason");
        }
    }
    Ok(Call::Reminder(input))
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
            let series = c.every.is_some() || c.cron.is_some();
            args.push(flag(
                "kind",
                if series {
                    "series_run"
                } else {
                    kind_name(c.kind.unwrap_or(HiveCreateInputKind::OwnerTask))
                },
            ));
            if let Some(details) = c.details.as_deref().filter(|d| !d.is_empty()) {
                args.push(flag("body", details));
            }
            if c.review == Some(true) {
                args.push("--review".into());
            }
            if c.urgent == Some(true) {
                args.push("--urgent".into());
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
            if let Some(text) = &c.reminder {
                args.push(flag("reminder", text));
            }
            let timing = [
                ("not-before", c.not_before.clone()),
                ("recheck-every", c.recheck_every.clone()),
                ("recheck-until", c.recheck_until.clone()),
                ("recheck-max", c.recheck_max.map(|n| n.to_string())),
                ("every", c.every.clone()),
                ("cron", c.cron.clone()),
                ("tz", c.tz.clone()),
                ("until", c.until.clone()),
                ("max-runs", c.max_runs.map(|n| n.to_string())),
            ];
            for (name, value) in timing {
                if let Some(value) = value {
                    args.push(flag(name, &value));
                }
            }
        }
        Call::List(l) => list_argv(l, &mut args),
        Call::Inspect(c) => {
            args.extend([
                "inspect".into(),
                "--json".into(),
                "--".into(),
                c.card_id.clone(),
            ]);
        }
        Call::Health => args.extend(["health".into(), "--json".into()]),
        Call::File(f) => file_argv(f, &mut args),
        Call::Show(s) => {
            args.extend([
                "show".into(),
                "--json".into(),
                "--".into(),
                s.card_id.clone(),
            ]);
        }
        Call::Steer(_) | Call::Cancel(_) | Call::Resolve(_) | Call::Reminder(_) => {
            write_argv(call, settings, &mut args);
        }
    }
    args
}

/// The arguments of the commands that change a card (`steer`, `cancel`, `resolve`, `reminder`):
/// the server's actor as `--by`, the input's values as one word each.
fn write_argv(call: &Call, settings: &Settings, args: &mut Vec<String>) {
    let by = format!("--by={}", settings.actor);
    match call {
        Call::Steer(s) => {
            args.extend([
                "steer".into(),
                by,
                "--".into(),
                s.card_id.clone(),
                s.note.clone(),
            ]);
        }
        Call::Cancel(c) => {
            args.extend([
                "cancel".into(),
                format!("--reason={}", c.reason),
                by,
                "--".into(),
                c.card_id.clone(),
            ]);
        }
        Call::Resolve(r) => {
            args.extend([
                "resolve".into(),
                by,
                "--".into(),
                r.card_id.clone(),
                r.action.as_str().into(),
                r.text.clone(),
            ]);
        }
        Call::Reminder(r) => {
            args.extend(["reminder".into(), r.action.as_str().into(), by]);
            if let Some(text) = &r.text {
                args.push(format!("--text={text}"));
            }
            if let Some(reason) = &r.reason {
                args.push(format!("--reason={reason}"));
            }
            args.extend(["--".into(), r.card_id.clone()]);
        }
        _ => {}
    }
}

/// The arguments of `list` after its command: each filter given, and the limit.
fn list_argv(l: &HiveListInput, args: &mut Vec<String>) {
    let flag = |name: &str, value: &str| format!("--{name}={value}");
    args.extend(["list".into(), "--json".into()]);
    let filters = [
        ("state", l.state.map(HiveListInputState::as_str)),
        ("class", l.card_class.map(HiveListInputCardClass::as_str)),
        ("parent", l.parent.as_deref()),
        ("followup-of", l.followup_of.as_deref()),
        ("creator", l.created_by.as_deref()),
        ("placement", l.placement.map(HiveListInputPlacement::as_str)),
        ("query", l.query.as_deref()),
    ];
    for (name, value) in filters {
        if let Some(value) = value {
            args.push(flag(name, value));
        }
    }
    let limit = l
        .limit
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_LIST_LIMIT);
    args.push(flag("limit", &limit.to_string()));
}

/// The arguments of `file`: offset and limit as flags, the card and the path after `--`.
fn file_argv(f: &HiveFileInput, args: &mut Vec<String>) {
    args.push("file".into());
    if let Some(offset) = f.offset {
        args.push(format!("--offset={offset}"));
    }
    if let Some(limit) = f.limit {
        args.push(format!("--limit={limit}"));
    }
    args.extend(["--".into(), f.card_id.clone(), f.path.clone()]);
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
    if let Some(id) = call.card()
        && matches!(call, Call::Show(_) | Call::Inspect(_) | Call::File(_))
        && exit.status == Some(1)
        && last == "no such card"
    {
        return Refusal(format!("no such card {id}"));
    }
    // The CLI's refusal: its `hive: ` line and the lines it carries on (a `bash -n` message).
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // A reminder verb's refusal exits 2, its failed send 1 (`reminder send failed: ...`).
    let typed =
        exit.status == Some(2) || (matches!(call, Call::Reminder(_)) && exit.status == Some(1));
    if typed && let Some(at) = lines.iter().rposition(|l| l.starts_with("hive: ")) {
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
                card_class: card["card_class"].as_str().map(|c| cut(c, 32)),
                owner_visible: card["owner_visible"].as_bool(),
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
        Call::Inspect(_) => {
            let snap: Value =
                serde_json::from_str(&stdout).map_err(|_| bad_answer(call, "no JSON snapshot"))?;
            to_value!(
                &inspected(snap).ok_or_else(|| bad_answer(call, "no card_id, card or state"))?
            )
        }
        Call::Health => {
            let snap: Value =
                serde_json::from_str(&stdout).map_err(|_| bad_answer(call, "no JSON snapshot"))?;
            to_value!(
                &healthy(snap).ok_or_else(|| bad_answer(call, "no dispatcher, slots or queue"))?
            )
        }
        Call::File(_) => {
            let out: HiveFileOutput =
                serde_json::from_str(&stdout).map_err(|_| bad_answer(call, "no JSON file"))?;
            to_value!(&out)
        }
        Call::Steer(_) | Call::Cancel(_) | Call::Resolve(_) | Call::Reminder(_) => {
            to_value!(&Ack {
                ok: true,
                message: first_line(&stdout, MAX_LINE_CHARS),
            })
        }
    };
    Ok(out)
}

/// The `HiveInspectOutput` of an `inspect --json` snapshot: its summary fields and the snapshot.
fn inspected(snap: Value) -> Option<HiveInspectOutput> {
    let id = snap["card_id"]
        .as_str()
        .filter(|id| is_card_id(id))?
        .to_owned();
    let card = &snap["card"];
    let state = cut(card["state"].as_str()?, 32);
    let round = card["round"].as_u64()?;
    let title = card["title"].as_str().map(|t| cut(t, MAX_TITLE_CHARS));
    let result_line = card["result"]
        .as_str()
        .and_then(|r| first_line(r, MAX_LINE_CHARS));
    let report_line = snap["rounds"].as_array().and_then(|rounds| {
        rounds
            .iter()
            .rev()
            .filter_map(|r| r["runs"].as_array())
            .flat_map(|runs| runs.iter().rev())
            .filter_map(|r| r["report"].as_str())
            .find_map(|r| first_line(r, MAX_LINE_CHARS))
    });
    let waiting_on = snap["heads"]["heads"]
        .as_array()
        .and_then(|heads| heads.iter().find_map(|h| waiting_line(&h["waiting_on"])));
    let Value::Object(snapshot) = snap else {
        return None;
    };
    Some(HiveInspectOutput {
        card_id: id,
        state,
        round,
        title,
        result_line,
        report_line,
        waiting_on,
        snapshot,
    })
}

/// One line of a head's `waiting_on`: the command, how long it has run, the pull requests and
/// workflow runs it names.
fn waiting_line(w: &Value) -> Option<String> {
    let command = w["command"].as_str()?;
    let command: Vec<&str> = command.split_whitespace().collect();
    let mut line = command.join(" ");
    if let Some(seconds) = w["for_s"].as_f64() {
        line = format!("{} s: {line}", seconds.round());
    }
    let numbers = |key: &str, mark: &str| -> String {
        let mut out = String::new();
        for n in w[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
        {
            out.push(' ');
            out.push_str(mark);
            out.push_str(&n.to_string());
        }
        out
    };
    let tags = format!(
        "{}{}",
        numbers("pull_requests", "PR #"),
        numbers("workflow_runs", "run ")
    );
    let room = MAX_WAITING_CHARS.saturating_sub(tags.chars().count());
    Some(format!("{}{tags}", cut(&line, room)))
}

/// The `HiveHealthOutput` of a `health --json` snapshot: its summary fields and the snapshot.
fn healthy(snap: Value) -> Option<HiveHealthOutput> {
    let dispatcher = &snap["dispatcher"];
    let ticking = dispatcher["ticking"].as_bool()?;
    let tick_age_s = dispatcher["tick_age_s"].as_f64().map(|a| a.max(0.0));
    let slots_used = snap["slots"]["used"].as_u64()?;
    let concurrency = snap["slots"]["concurrency"].as_u64()?;
    let queue_length = snap["queue"]["length"].as_u64()?;
    let Value::Object(snapshot) = snap else {
        return None;
    };
    Some(HiveHealthOutput {
        ticking,
        tick_age_s,
        slots_used,
        concurrency,
        queue_length,
        snapshot,
    })
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
pub fn input_schemas() -> [(&'static str, Value); 10] {
    [
        ("hive_create", create_schema()),
        ("hive_list", list_schema()),
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
        (
            "hive_resolve",
            json!({
                "type": "object",
                "description": "Decide a disputed card (state blocked, wait disputed) as its author: accept finishes it done with the head's result, rework queues a new round.",
                "properties": {
                    "card_id": card_id_schema(),
                    "action": {
                        "type": "string",
                        "description": "`accept`: the dispute is right, the card is done. `rework`: a new round follows `text`.",
                        "enum": ["accept", "rework"]
                    },
                    "text": {
                        "type": "string",
                        "description": "Why (accept) or the instructions of the next round (rework), kept on the card.",
                        "minLength": 1,
                        "maxLength": 4000
                    }
                },
                "required": ["card_id", "action", "text"],
                "additionalProperties": false
            }),
        ),
        ("hive_reminder", reminder_schema()),
        ("hive_inspect", card_ref_schema()),
        ("hive_health", empty_schema()),
        ("hive_file", file_schema()),
    ]
}

fn list_schema() -> Value {
    json!({
        "type": "object",
        "description": "List cards, newest first; every filter given must hold.",
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
            },
            "card_class": {
                "type": "string",
                "description": "Only cards of this class (`--class`).",
                "enum": ["owner_task", "system", "followup", "series_run"]
            },
            "parent": {"type": "string", "description": "Only the children of this epic (`--parent`).", "pattern": CARD_ID},
            "followup_of": {"type": "string", "description": "Only the followups of this card (`--followup-of`).", "pattern": CARD_ID},
            "created_by": {"type": "string", "description": "Only cards this actor created (`--creator`).", "pattern": LABEL},
            "placement": {
                "type": "string",
                "description": "`board`: cards on the board or in its archive; `hidden`: the rules of schedules and series and the series runs without a followup link (`--placement`).",
                "enum": ["board", "hidden"]
            },
            "query": {"type": "string", "description": "Case-insensitive text in the title or goal (`--query`).", "minLength": 1, "maxLength": 200}
        },
        "required": [],
        "additionalProperties": false
    })
}

fn card_ref_schema() -> Value {
    json!({
        "type": "object",
        "description": "Reference to a Hive card.",
        "properties": {"card_id": card_id_schema()},
        "required": ["card_id"],
        "additionalProperties": false
    })
}

fn empty_schema() -> Value {
    json!({
        "type": "object",
        "description": "No arguments.",
        "properties": {},
        "required": [],
        "additionalProperties": false
    })
}

fn reminder_schema() -> Value {
    json!({
        "type": "object",
        "description": "Act on a due reminder or watchdog alert (a card waiting `blocked`, wait `reminder`): claim it first, then send it as is or edited, or withdraw it with a reason.",
        "properties": {
            "card_id": card_id_schema(),
            "action": {
                "type": "string",
                "description": "`claim`: take a `due` one, which stops the hive's own send while you decide. `send`: send a claimed one to the owner through Telegram, the original text or `text`. `withdraw`: drop a claimed one with `reason`; nothing is sent.",
                "enum": ["claim", "send", "withdraw"]
            },
            "text": {
                "type": "string",
                "description": "Only with `send`: the text the owner gets instead of the original, verbatim. Not blank, at most 32768 bytes of UTF-8.",
                "minLength": 1,
                "maxLength": 32_768
            },
            "reason": {
                "type": "string",
                "description": "Only with `withdraw`, and required there: why the owner does not get it, kept on the card.",
                "minLength": 1,
                "maxLength": 1000
            }
        },
        "required": ["card_id", "action"],
        "additionalProperties": false
    })
}

fn file_schema() -> Value {
    json!({
        "type": "object",
        "description": "Read one file or list one directory of a card's workspace, read-only: a relative path inside it, or `@out/<name>` for a results file the card's own goal, details or acceptance names. No `..`, no absolute path, no symlink, no name that looks like a secret (`.env`, `token`, `key`, ...).",
        "properties": {
            "card_id": card_id_schema(),
            "path": {
                "type": "string",
                "description": "Relative path in the workspace, such as `REPORT.md`, `ev/log.txt` or `ev`; or `@out/<name>`.",
                "minLength": 1,
                "maxLength": 512,
                "pattern": "^[^/]"
            },
            "offset": {"type": "integer", "description": "Byte offset to start reading at; default 0.", "minimum": 0},
            "limit": {"type": "integer", "description": "At most this many bytes; default and maximum 262144.", "minimum": 1, "maximum": 262_144}
        },
        "required": ["card_id", "path"],
        "additionalProperties": false
    })
}

fn create_schema() -> Value {
    json!({
        "type": "object",
        "description": "Create a Hive card: the fields of `hive.py create` a delegation may set. Exactly one of `acceptance` and `accept_review`. The caller is not an input: the server sets `created_by` from its config. No field names a script, an executor, a model, a delivery or a path; the result always comes back to the secretary through the owner inbox. Timing is the hive's own scheduler (ADR-0082): `not_before` holds a one-off card until a time; with `reminder` the card is a reminder, whose exact text falls due then and runs no head; `every` or `cron` (with `tz`) make a series that creates one card per period, bounded by `until` or `max_runs` (`every` needs one of them); `recheck_every` with `recheck_until` or `recheck_max` re-runs the acceptance of a done card and reports when it fails (a watcher of a condition). Times are RFC 3339 with an offset, durations `Ns`, `Nm`, `Nh` or `Nd`.",
        "properties": {
            "title": {"type": "string", "description": "Short title.", "minLength": 1, "maxLength": 200},
            "goal": {"type": "string", "description": "What the head must achieve.", "minLength": 1, "maxLength": 8000},
            "details": {"type": "string", "description": "Further context for the head, the card's body (`--body`).", "maxLength": 32000},
            "acceptance": {"type": "string", "description": "Mechanical acceptance: a bash command run in the card workspace, exit 0 = pass. A bare `test -s REPORT.md` needs `review`; for criteria a judge checks use `accept_review` instead.", "minLength": 1, "maxLength": 2000},
            "accept_review": {"type": "string", "description": "Criteria the judge checks the report against, instead of `acceptance` (`--accept-review`; the gate is a non-empty `REPORT.md`). Judge only: it adds no reviewer; set `review` for one.", "minLength": 1, "maxLength": 4000},
            "step": {"type": "integer", "description": "Difficulty step 1..5.", "minimum": 1, "maximum": 5},
            "source": {"type": "string", "description": "Where the request came from (`--source`), such as `msg:<id>`, `issue:<n>` or `manual`.", "pattern": LABEL},
            "kind": {"type": "string", "description": "Card class (`--kind`); default `owner_task`. Leave it out with `every` or `cron`: a series is `series_run`.", "enum": ["owner_task", "system", "followup"]},
            "review": {"type": "boolean", "description": "Independent review by the other vendor (`--review`). Off by default; only `review: true` turns it on, never `accept_review`, `kind` or `step`. Set it for code cards (pull requests, code changes) first, for complex research that needs a second check, and for anything else the author decides to review; small errands (look, switch, check a status) go without."},
            "urgent": {"type": "boolean", "description": "The owner waits for this result now (`--urgent`): the card runs on the hive's urgent lane, past its IO gate and above its concurrency, at most two such cards at once. Only for a card of the owner's own request on the main chat; the engine refuses it on any other turn (ADR-0103). Only with kind `owner_task` (the default) and a head: not with `every`, `cron` or `reminder`."},
            "hold": {"type": "boolean", "description": "Park the card until the owner runs `hive.py release`. Not with `not_before`, `every` or `cron`."},
            "owner_visible": {"type": "boolean", "description": "Notify the owner about the result; default by class. A series (`series_run`) reports only news unless this is true."},
            "idem_key": {"type": "string", "description": "Idempotency key of the request: a second create with it returns the first card.", "pattern": LABEL},
            "not_before": {"type": "string", "description": "Hold the card until this time, then run it once (`--not-before`): RFC 3339 with an offset, such as `2026-11-27T09:00:00+03:00`. With `every` or `cron` the first period starts then. Not with `hold`.", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]{1,6})?(Z|[+-][0-9]{2}:[0-9]{2})$"},
            "every": {"type": "string", "description": "Make a series: one card per period of this length (`--every`), `Ns`, `Nm`, `Nh` or `Nd` up to 366 days, such as `15m`. Needs `until` or `max_runs`; not with `cron`, `kind`, `hold` or a recheck.", "pattern": "^[1-9][0-9]{0,6}[smhd]$"},
            "cron": {"type": "string", "description": "Make a series on a five-field cron schedule (`--cron`), such as `*/15 9-18 * * 1-5` or `0 9 * * *`; minute hour day month weekday, single spaces. Unbounded unless `until` or `max_runs`; not with `every`, `kind`, `hold` or a recheck.", "maxLength": 100, "pattern": "^[A-Za-z0-9*/,?#-]+( [A-Za-z0-9*/,?#-]+){4}$"},
            "tz": {"type": "string", "description": "IANA time zone of `cron` (`--tz`), such as `Europe/Moscow`; default the hive's owner zone. Only with `cron`.", "maxLength": 64, "pattern": "^[A-Za-z][A-Za-z0-9_+-]{0,31}(/[A-Za-z0-9_+-]{1,32}){0,2}$"},
            "until": {"type": "string", "description": "No period of the series starts at or after this time (`--until`): RFC 3339 with an offset. Only with `every` or `cron`.", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]{1,6})?(Z|[+-][0-9]{2}:[0-9]{2})$"},
            "max_runs": {"type": "integer", "description": "At most this many cards of the series (`--max-runs`). Only with `every` or `cron`.", "minimum": 1, "maximum": 10000},
            "recheck_every": {"type": "string", "description": "After the card is done, run its acceptance again every this long (`--recheck-every`), `Ns`, `Nm`, `Nh` or `Nd` up to 366 days; a failed recheck blocks the card and reports it. Needs `recheck_until` or `recheck_max`; not with `every` or `cron`.", "pattern": "^[1-9][0-9]{0,6}[smhd]$"},
            "recheck_until": {"type": "string", "description": "No recheck at or after this time (`--recheck-until`): RFC 3339 with an offset. Only with `recheck_every`.", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]{1,6})?(Z|[+-][0-9]{2}:[0-9]{2})$"},
            "recheck_max": {"type": "integer", "description": "At most this many rechecks (`--recheck-max`). Only with `recheck_every`.", "minimum": 1, "maximum": 1000},
            "reminder": {"type": "string", "description": "Make the card a reminder (`--reminder`): this exact text, verbatim, falls due at `not_before` (or each period of `every` or `cron`, or when the owner releases a `hold`) and comes back to you as a `reminder_due` event to claim, then send or withdraw with `hive_reminder`; no head runs. Not blank, at most 32768 bytes of UTF-8. Give `acceptance` `true`; not with `accept_review`, `review` or a recheck.", "minLength": 1, "maxLength": 32_768}
        },
        "required": ["title", "goal", "step", "source"],
        "additionalProperties": false
    })
}

/// What each tool does, for the model.
fn description(name: &str) -> &'static str {
    match name {
        "hive_create" => {
            "Delegate a task to the hive: create a card that a hive head carries out; its result comes back to you through the owner inbox. Give exactly one of `acceptance` (a bash check, exit 0 = done) and `accept_review` (criteria a judge checks the report against). The hive is also the scheduler. A reminder (\"remind me tomorrow at 9\") is one card with `reminder` the exact text the owner gets, `not_before` set to that time with its offset, such as `2026-11-27T09:00:00+03:00`, and `acceptance` `true`; no head runs, and at that time you get a `reminder_due` event to send it with `hive_reminder`. A watch (\"check every 15 minutes until evening\") is a series: `every` `15m` with `until` the evening's time (or `max_runs`), or `cron` such as `*/15 9-18 * * *` with `tz` such as `Europe/Moscow`; leave `kind` out. A series reports only news (a result line `RESULT: ... [NEWS]`) unless `owner_visible` is true; its own card waits in state `blocked` and makes one card per period, and `hive_cancel` on it ends the series. Set `urgent` only when the owner, in this turn on the main chat, waits for the result now: it runs ahead of and above the other cards. Answers the card id, its state and class."
        }
        "hive_list" => {
            "List hive cards, newest first: optionally of one state or class, the children of an epic, the followups of a card, the cards an actor created, those on the board or hidden from it (series runs, schedule rules), and with a text in the title or goal. Use it before `hive_create` to find a card the new one would duplicate."
        }
        "hive_show" => {
            "Show a hive card: state, round, title, the first line of its result and of its last report."
        }
        "hive_steer" => {
            "Send a clarification to the head working on a hive card; a card without a live head gets it in its next round."
        }
        "hive_cancel" => "Cancel a hive card with a reason; its active heads are stopped.",
        "hive_resolve" => {
            "Decide a disputed hive card (a head answered `RESULT: disputed`; the card is `blocked`, waiting `disputed` for its author). `accept`: the head is right, the card finishes done with its result; `text` says why. `rework`: the card goes back to the queue and its next round follows `text` as instructions. Only the card's author, the secretary or the owner may decide; any other card or state is refused."
        }
        "hive_reminder" => {
            "Act on a due reminder or hive watchdog alert (a `reminder_due` or `watchdog_alert` event; its card waits `blocked`, wait `reminder`). `claim` it first: that stops the hive sending it by itself while you decide. Then `send` it, the original text or your edited `text`, which the owner gets through Telegram, or `withdraw` it with a `reason` when the owner does not need it; nothing else sends it, and your own reply never repeats it. Unclaimed after the event's `unclaimed_s`, or claimed and silent for its `claim_silence_s`, the hive sends the original itself. A refusal names why (not a reminder, not due, already claimed, sent or withdrawn); a failed send stays claimed and is retried."
        }
        "hive_inspect" => {
            "Look inside a hive card, read-only, as of now: every round with its runs (worker or reviewer, model, effort, unit, start, end, outcome, report), the gate's result, the judge's or reviewer's verdict and reason (why a round was sent back); the live heads (systemd unit, the command each waits on and for how long, its last actions with their times, the units it started, the pull requests and CI runs it waits on with their checks; command lines and actions have credentials masked); the workspace's repositories (branch, last commit, latest pull request); where the card waits (board column, queue position, who holds the slots, hold, backoff, blockers, budget); its chain (parent, children, followup_of, followups, blockers); steers with their text and delivery; whether its result reached your inbox and your ACK; the workspace's files for `hive_file`. `waiting_on` is the one-line answer to «is it working or stuck»."
        }
        "hive_health" => {
            "The hive as a whole, read-only, as of now: config limits (slots, the urgent lane of at most two cards above them, the ladder of models and efforts per step, vendors), whether the dispatcher ticks and its last tick, every run holding a slot (series runs and reviewers too), the launch queue, every series with its schedule and next start, cooldowns, and the outbox of events for you with their inbox status."
        }
        "hive_file" => {
            "Read a file of a hive card's workspace, read-only: a path from `hive_inspect`'s `files`, such as `REPORT.md` or `ev/<name>`, a directory to list it, or `@out/<name>` for a results file the card's goal names. Up to 256 KiB per call; `offset` reads on."
        }
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
    fn accept_review_alone_asks_for_no_review() {
        let mut judged = with(&json!({"accept_review": "criteria"}));
        judged.as_object_mut().unwrap().remove("acceptance");
        let args = args_of("hive_create", &judged, false);
        assert!(args.contains(&"--accept-review=criteria".to_owned()));
        assert!(!args.contains(&"--review".to_owned()));
        judged["review"] = json!(false);
        assert!(!args_of("hive_create", &judged, false).contains(&"--review".to_owned()));
        judged["review"] = json!(true);
        assert!(args_of("hive_create", &judged, false).contains(&"--review".to_owned()));
    }

    #[test]
    fn urgent_true_alone_becomes_the_urgent_flag() {
        let urgent = args_of(
            "hive_create",
            &with(&json!({"step": 3, "urgent": true})),
            false,
        );
        assert!(urgent.contains(&"--urgent".to_owned()));
        assert!(urgent.contains(&"--kind=owner_task".to_owned()));
        for plain in [with(&json!({"urgent": false})), base()] {
            assert!(
                !args_of("hive_create", &plain, false)
                    .iter()
                    .any(|a| a.contains("urgent"))
            );
        }
    }

    #[test]
    fn urgent_is_refused_off_an_owner_task_run_by_a_head() {
        for extra in [
            json!({"kind": "system"}),
            json!({"kind": "followup"}),
            json!({"every": "15m", "max_runs": 2}),
            json!({"cron": "0 9 * * *"}),
            json!({"reminder": "r", "acceptance": "true", "not_before": "2026-11-27T09:00:00Z"}),
        ] {
            let mut args = with(&extra);
            args["urgent"] = json!(true);
            assert!(
                refused("hive_create", &args).starts_with("urgent applies only"),
                "{extra}"
            );
            args["urgent"] = json!(false);
            assert!(parse("hive_create", &args).unwrap().is_ok(), "{extra}");
        }
        let text = refused("hive_create", &with(&json!({"urgent": "yes"})));
        assert!(text.contains("invalid arguments"), "{text}");
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
            args_of(
                "hive_resolve",
                &json!({"card_id": "h_0123abcd", "action": "rework", "text": "--by=owner -x"}),
                false
            ),
            [
                "resolve",
                "--by=secretary",
                "--",
                "h_0123abcd",
                "rework",
                "--by=owner -x"
            ]
        );
        assert_eq!(
            args_of("hive_show", &json!({"card_id": "h_0123abcd"}), false),
            ["show", "--json", "--", "h_0123abcd"]
        );
        assert_eq!(
            args_of("hive_list", &json!({"state": "queued"}), false),
            ["list", "--json", "--state=queued", "--limit=20"]
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
            "misfire",
            "start_window",
            "overlap",
            "deliver",
            "tag",
            "type",
            "priority",
            "gate_timeout",
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
    fn timed_creates_pass_each_value_as_one_flag_word() {
        let reminder = args_of(
            "hive_create",
            &with(&json!({"acceptance": "true", "not_before": "2026-11-27T09:00:00+03:00"})),
            false,
        );
        assert_eq!(
            &reminder[7..],
            [
                "--created-by=secretary",
                "--kind=owner_task",
                "--not-before=2026-11-27T09:00:00+03:00"
            ]
        );
        let every = args_of(
            "hive_create",
            &with(&json!({"every": "15m", "until": "2026-11-27T18:00:00Z", "owner_visible": true})),
            false,
        );
        assert_eq!(
            &every[7..],
            [
                "--created-by=secretary",
                "--kind=series_run",
                "--owner-visible",
                "--every=15m",
                "--until=2026-11-27T18:00:00Z"
            ]
        );
        let cron = args_of(
            "hive_create",
            &with(&json!({
                "cron": "*/15 9-18 * * 1-5", "tz": "Europe/Moscow", "max_runs": 40,
                "until": "2026-11-27T18:00:00.5+03:00", "not_before": "2026-11-27T09:00:00+03:00"
            })),
            false,
        );
        assert_eq!(
            &cron[7..],
            [
                "--created-by=secretary",
                "--kind=series_run",
                "--not-before=2026-11-27T09:00:00+03:00",
                "--cron=*/15 9-18 * * 1-5",
                "--tz=Europe/Moscow",
                "--until=2026-11-27T18:00:00.5+03:00",
                "--max-runs=40"
            ]
        );
        let recheck = args_of(
            "hive_create",
            &with(
                &json!({"recheck_every": "1h", "recheck_until": "2026-11-28T00:00:00Z", "recheck_max": 5}),
            ),
            false,
        );
        assert_eq!(
            &recheck[9..],
            [
                "--recheck-every=1h",
                "--recheck-until=2026-11-28T00:00:00Z",
                "--recheck-max=5"
            ]
        );
        // An option-like cron stays one word; no flag outside the timing ones appears.
        let odd = args_of("hive_create", &with(&json!({"cron": "-1 * * * *"})), false);
        assert!(odd.contains(&"--cron=-1 * * * *".to_owned()));
        assert!(!odd.iter().any(|a| a.starts_with("--deliver")
            || a.starts_with("--script")
            || a.starts_with("--misfire")));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn timing_values_are_checked_as_the_hive_parses_them() {
        for bad in [
            "2026-11-27T09:00:00",
            "2026-11-27 09:00:00+03:00",
            "2026-11-27T09:00+03:00",
            "2026-11-27T09:00:00+0300",
            "2026-02-29T09:00:00Z",
            "2026-13-01T09:00:00Z",
            "2026-11-27T24:00:00Z",
            "2026-11-27T09:00:60Z",
            "2026-11-27T09:00:00.1234567Z",
            "2026-11-27T09:00:00+24:00",
            "--not-before",
            "tomorrow 9am",
            "",
        ] {
            for field in ["not_before", "until", "recheck_until"] {
                let mut args = json!({ field: bad });
                if field == "until" {
                    args["cron"] = json!("0 9 * * *");
                }
                if field == "recheck_until" {
                    args["recheck_every"] = json!("1h");
                }
                let text = refused("hive_create", &with(&args));
                assert!(text.contains("RFC 3339"), "{field} {bad}: {text}");
            }
        }
        for bad in [
            "0m",
            "15",
            "m",
            "15min",
            "1w",
            "-5m",
            "015m",
            "367d",
            "99999999s",
            "1.5h",
        ] {
            let text = refused("hive_create", &with(&json!({"every": bad, "max_runs": 2})));
            assert!(
                text.contains("every must be a positive duration"),
                "{bad}: {text}"
            );
            let text = refused(
                "hive_create",
                &with(&json!({"recheck_every": bad, "recheck_max": 2})),
            );
            assert!(text.contains("recheck_every must be"), "{bad}: {text}");
        }
        for bad in [
            "* * * *",
            "0 */15 9-18 * * 1-5",
            "0  9 * * *",
            " 0 9 * * *",
            "0 9 * * *;rm",
            "0 9 * * $(id)",
            "0 9 * * *\n",
        ] {
            assert!(
                refused("hive_create", &with(&json!({"cron": bad}))).contains("cron must be"),
                "{bad}"
            );
        }
        for bad in [
            "",
            "/UTC",
            "Europe/",
            "Europe/Moscow/A/B",
            "../etc/passwd",
            "1UTC",
            "Europe Moscow",
        ] {
            assert!(
                refused(
                    "hive_create",
                    &with(&json!({"cron": "0 9 * * *", "tz": bad}))
                )
                .contains("tz must be"),
                "{bad}"
            );
        }
        for (field, value, needs) in [
            ("max_runs", 0, json!({"every": "1h"})),
            ("max_runs", 10_001, json!({"every": "1h"})),
            ("recheck_max", 0, json!({"recheck_every": "1h"})),
            ("recheck_max", 1_001, json!({"recheck_every": "1h"})),
        ] {
            let mut args = needs;
            args[field] = json!(value);
            assert!(
                refused("hive_create", &with(&args)).contains(&format!("{field} must be 1 to")),
                "{field} {value}"
            );
        }
        assert!(
            refused(
                "hive_create",
                &with(&json!({"every": "1h", "max_runs": -1}))
            )
            .contains("invalid arguments")
        );
        assert!(
            refused("hive_create", &with(&json!({"every": 60, "max_runs": 1})))
                .contains("invalid arguments")
        );
    }

    #[test]
    fn timing_pairs_exclude_and_need_each_other() {
        let cases = [
            (
                json!({"every": "1h", "cron": "0 9 * * *", "max_runs": 2}),
                "at most one of every and cron",
            ),
            (
                json!({"every": "1h", "tz": "UTC", "max_runs": 2}),
                "tz is the zone of cron",
            ),
            (json!({"tz": "UTC"}), "tz is the zone of cron"),
            (
                json!({"until": "2026-11-27T18:00:00Z"}),
                "until and max_runs bound a series",
            ),
            (json!({"max_runs": 3}), "until and max_runs bound a series"),
            (json!({"every": "1h"}), "every needs until or max_runs"),
            (json!({"recheck_max": 3}), "need recheck_every"),
            (
                json!({"recheck_until": "2026-11-27T18:00:00Z"}),
                "need recheck_every",
            ),
            (
                json!({"recheck_every": "1h"}),
                "recheck_every needs recheck_until or recheck_max",
            ),
            (
                json!({"every": "1h", "max_runs": 2, "recheck_every": "1h", "recheck_max": 2}),
                "takes no recheck_every",
            ),
            (
                json!({"cron": "0 9 * * *", "kind": "owner_task"}),
                "leave kind out with every or cron",
            ),
            (
                json!({"not_before": "2026-11-27T09:00:00+03:00", "hold": true}),
                "hold parks a card",
            ),
            (
                json!({"cron": "0 9 * * *", "hold": true}),
                "hold parks a card",
            ),
            (
                json!({"not_before": "2026-11-27T18:00:00+03:00", "every": "1h", "until": "2026-11-27T15:00:00Z"}),
                "until must be after not_before",
            ),
            (
                json!({"not_before": "2026-11-27T18:00:00Z", "recheck_every": "1h", "recheck_until": "2026-11-27T18:00:00Z"}),
                "recheck_until must be after not_before",
            ),
        ];
        for (extra, want) in cases {
            let text = refused("hive_create", &with(&extra));
            assert!(text.contains(want), "{extra}: {text}");
        }
        // Accepted: an unbounded cron, a cron with tz, hold without timing, a recheck with hold.
        for extra in [
            json!({"cron": "0 9 * * *"}),
            json!({"cron": "0 9 * * MON-FRI", "tz": "America/Argentina/Buenos_Aires"}),
            json!({"hold": true}),
            json!({"hold": true, "recheck_every": "1h", "recheck_max": 3}),
            json!({"not_before": "2026-11-27T09:00:00+03:00", "every": "1d", "until": "2026-11-27T06:00:01Z"}),
        ] {
            assert!(
                parse("hive_create", &with(&extra)).unwrap().is_ok(),
                "{extra}"
            );
        }
    }

    #[test]
    fn config_hold_refuses_timed_cards() {
        let call = |extra: Value| parse("hive_create", &with(&extra)).unwrap().unwrap();
        for extra in [
            json!({"not_before": "2026-11-27T09:00:00+03:00"}),
            json!({"every": "1h", "max_runs": 2}),
            json!({"cron": "0 9 * * *"}),
        ] {
            let text = admit(&call(extra.clone()), &settings(true)).unwrap_err().0;
            assert!(text.contains("hive_mcp.hold"), "{extra}: {text}");
            assert_eq!(admit(&call(extra), &settings(false)), Ok(()));
        }
        assert_eq!(admit(&call(json!({})), &settings(true)), Ok(()));
        assert_eq!(
            admit(
                &call(json!({"recheck_every": "1h", "recheck_max": 2})),
                &settings(true)
            ),
            Ok(())
        );
    }

    #[test]
    fn rfc3339_times_are_unix_seconds() {
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_secs("2026-11-27T09:00:00+03:00"),
            Some(1_795_759_200)
        );
        assert_eq!(rfc3339_secs("2026-11-27T06:00:00Z"), Some(1_795_759_200));
        assert_eq!(
            rfc3339_secs("2026-11-27T01:30:00-04:30"),
            Some(1_795_759_200)
        );
        assert_eq!(
            rfc3339_secs("2028-02-29T00:00:00.999999Z"),
            Some(1_835_395_200)
        );
        assert_eq!(rfc3339_secs("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(rfc3339_secs("1900-02-29T00:00:00Z"), None);
        assert_eq!(rfc3339_secs("2026-11-27T09:00:00z"), None);
        assert_eq!(rfc3339_secs("2026-11-27t09:00:00Z"), None);
        assert_eq!(rfc3339_secs("2026-11-27T09:00:00.Z"), None);
        assert_eq!(rfc3339_secs("2026-11-27T09:00:00+03:00 "), None);
        assert_eq!(duration_secs("15m"), Some(900));
        assert_eq!(duration_secs("366d"), Some(MAX_DURATION_SECS));
        assert_eq!(duration_secs("1s"), Some(1));
        assert!(is_cron("0 9 * * *") && is_cron("*/5 9-18 ? JAN-MAR MON#2"));
        assert!(
            is_zone("UTC") && is_zone("Etc/GMT+3") && is_zone("America/Argentina/Buenos_Aires")
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
    fn resolve_takes_only_a_card_an_action_and_a_text() {
        let ok = json!({"card_id": "h_0123abcd", "action": "accept", "text": "The head is right."});
        assert!(parse("hive_resolve", &ok).unwrap().is_ok());
        for bad in ["--by=owner", "h_0123ABCD", ""] {
            assert!(
                refused(
                    "hive_resolve",
                    &json!({"card_id": bad, "action": "accept", "text": "t"})
                )
                .contains("card_id"),
                "{bad}"
            );
        }
        for action in ["close", "Accept", "", "done"] {
            assert!(
                refused(
                    "hive_resolve",
                    &json!({"card_id": "h_0123abcd", "action": action, "text": "t"})
                )
                .contains("invalid arguments"),
                "{action}"
            );
        }
        assert!(
            refused(
                "hive_resolve",
                &json!({"card_id": "h_0123abcd", "action": "accept"})
            )
            .contains("missing field `text`")
        );
        assert_eq!(
            refused(
                "hive_resolve",
                &json!({"card_id": "h_0123abcd", "action": "rework", "text": " \n "})
            ),
            "text is required"
        );
        assert!(
            refused(
                "hive_resolve",
                &json!({"card_id": "h_0123abcd", "action": "rework", "text": "x".repeat(4001)})
            )
            .contains("text must be 1 to 4000")
        );
        assert!(
            refused(
                "hive_resolve",
                &json!({"card_id": "h_0123abcd", "action": "rework", "text": "a\u{0}b"})
            )
            .contains("NUL")
        );
        for extra in ["by", "decision", "owner"] {
            assert!(
                refused(
                    "hive_resolve",
                    &json!({"card_id": "h_0123abcd", "action": "accept", "text": "t", extra: "owner"})
                )
                .contains("unknown field"),
                "{extra}"
            );
        }
        let call = parse("hive_resolve", &ok).unwrap().unwrap();
        assert_eq!(call.card(), Some("h_0123abcd"));
        assert_eq!(
            answer(
                &call,
                &exit(0, "h_0123abcd dispute resolved: accept, now done\n", "")
            )
            .unwrap(),
            json!({"ok": true, "message": "h_0123abcd dispute resolved: accept, now done"})
        );
        assert_eq!(
            answer(
                &call,
                &exit(
                    2,
                    "",
                    "hive: resolve rejected: card h_0123abcd is not disputed (state done, wait None)\n"
                )
            )
            .unwrap_err()
            .0,
            "resolve rejected: card h_0123abcd is not disputed (state done, wait None)"
        );
    }

    #[test]
    fn reminder_verbs_run_the_hive_cli_with_only_their_fields() {
        let id = "h_0123abcd";
        assert_eq!(
            args_of(
                "hive_reminder",
                &json!({"card_id": id, "action": "claim"}),
                false
            ),
            ["reminder", "claim", "--by=secretary", "--", id]
        );
        assert_eq!(
            args_of(
                "hive_reminder",
                &json!({"card_id": id, "action": "send"}),
                false
            ),
            ["reminder", "send", "--by=secretary", "--", id]
        );
        assert_eq!(
            args_of(
                "hive_reminder",
                &json!({"card_id": id, "action": "send", "text": "--by=owner\n Water the plants. "}),
                false
            ),
            [
                "reminder",
                "send",
                "--by=secretary",
                "--text=--by=owner\n Water the plants. ",
                "--",
                id
            ]
        );
        assert_eq!(
            args_of(
                "hive_reminder",
                &json!({"card_id": id, "action": "withdraw", "reason": "-x already done"}),
                false
            ),
            [
                "reminder",
                "withdraw",
                "--by=secretary",
                "--reason=-x already done",
                "--",
                id
            ]
        );
    }

    #[test]
    fn reminder_fields_are_refused_before_anything_runs() {
        let id = "h_0123abcd";
        let refusals = [
            (
                json!({"card_id": id, "action": "claim", "text": "t"}),
                "claim takes no text and no reason",
            ),
            (
                json!({"card_id": id, "action": "claim", "reason": "r"}),
                "claim takes no text and no reason",
            ),
            (
                json!({"card_id": id, "action": "send", "reason": "r"}),
                "send takes no reason: give the edited text as text, or nothing",
            ),
            (
                json!({"card_id": id, "action": "send", "text": " \n"}),
                "text is required",
            ),
            (
                json!({"card_id": id, "action": "send", "text": "a\u{0}b"}),
                "text must not hold a NUL character",
            ),
            (
                json!({"card_id": id, "action": "send", "text": "\u{e9}".repeat(16_385)}),
                "text must be at most 32768 bytes of UTF-8",
            ),
            (
                json!({"card_id": id, "action": "withdraw"}),
                "withdraw needs a reason",
            ),
            (
                json!({"card_id": id, "action": "withdraw", "reason": "  "}),
                "reason is required",
            ),
            (
                json!({"card_id": id, "action": "withdraw", "reason": "r", "text": "t"}),
                "withdraw takes no text: nothing is sent",
            ),
            (
                json!({"card_id": "h_0123ABCD", "action": "claim"}),
                "card_id must be h_ followed by 8 lowercase hex digits",
            ),
        ];
        for (args, want) in refusals {
            assert_eq!(refused("hive_reminder", &args), want, "{args}");
        }
        assert!(
            refused(
                "hive_reminder",
                &json!({"card_id": id, "action": "withdraw", "reason": "x".repeat(1001)})
            )
            .contains("reason must be 1 to 1000")
        );
        for bad in [
            json!({"card_id": id, "action": "close"}),
            json!({"card_id": id, "action": "claim", "by": "owner"}),
            json!({"card_id": id}),
        ] {
            assert!(
                refused("hive_reminder", &bad).contains("invalid arguments"),
                "{bad}"
            );
        }
        assert_eq!(
            args_of(
                "hive_reminder",
                &json!({"card_id": id, "action": "send", "text": "\u{e9}".repeat(16_384)}),
                false
            )
            .len(),
            6
        );
    }

    #[test]
    fn reminder_answers_and_refusals_map_to_ack_and_typed_errors() {
        let send = parse(
            "hive_reminder",
            &json!({"card_id": "h_0123abcd", "action": "send"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(send.card(), Some("h_0123abcd"));
        let receipt = "{\"card\": \"h_0123abcd\", \"state\": \"delivered\", \"receipt\": {\"chats\": [{\"chat_id\": 1, \"message_ids\": [7]}]}}\n";
        assert_eq!(
            answer(&send, &exit(0, receipt, "")).unwrap(),
            json!({"ok": true, "message": receipt.trim()})
        );
        assert_eq!(
            answer(
                &send,
                &exit(
                    1,
                    "",
                    "hive: reminder send failed: ulei-tg notify: exit 1: synthetic\n"
                )
            )
            .unwrap_err()
            .0,
            "reminder send failed: ulei-tg notify: exit 1: synthetic"
        );
        let claim = parse(
            "hive_reminder",
            &json!({"card_id": "h_0123abcd", "action": "claim"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            answer(
                &claim,
                &exit(0, "h_0123abcd reminder claimed by secretary\n", "")
            )
            .unwrap(),
            json!({"ok": true, "message": "h_0123abcd reminder claimed by secretary"})
        );
        assert_eq!(
            answer(
                &claim,
                &exit(
                    2,
                    "",
                    "hive: reminder claim refused: reminder h_0123abcd is claimed; this needs due\n"
                )
            )
            .unwrap_err()
            .0,
            "reminder claim refused: reminder h_0123abcd is claimed; this needs due"
        );
        // Exit 1 of another tool stays a failed CLI run.
        let cancel = parse(
            "hive_cancel",
            &json!({"card_id": "h_0123abcd", "reason": "r"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            answer(&cancel, &exit(1, "", "hive: boom\n")).unwrap_err().0,
            "the hive CLI failed (exit 1): hive: boom"
        );
    }

    #[test]
    fn create_takes_a_verbatim_reminder_without_a_head() {
        let text = "  Synthetic reminder: --by=owner\nwater the plants ";
        let args = args_of(
            "hive_create",
            &with(
                &json!({"acceptance": "true", "not_before": "2026-11-27T09:00:00+03:00", "reminder": text}),
            ),
            false,
        );
        assert!(args.contains(&format!("--reminder={text}")), "{args:?}");
        assert!(args.contains(&"--not-before=2026-11-27T09:00:00+03:00".to_owned()));
        let held = args_of(
            "hive_create",
            &with(&json!({"acceptance": "true", "hold": true, "reminder": "r"})),
            false,
        );
        assert!(held.contains(&"--reminder=r".to_owned()) && held.contains(&"--hold".to_owned()));
        let series = args_of(
            "hive_create",
            &with(&json!({"acceptance": "true", "every": "1d", "max_runs": 3, "reminder": "r"})),
            false,
        );
        assert!(series.contains(&"--reminder=r".to_owned()));
        let timed = json!({"not_before": "2026-11-27T09:00:00+03:00", "reminder": "r"});
        let mut judged = with(&timed);
        judged.as_object_mut().unwrap().remove("acceptance");
        judged["accept_review"] = json!("criteria");
        let refusals = [
            (
                with(&json!({"reminder": "r"})),
                "a reminder needs not_before, every, cron or hold",
            ),
            (
                with(&json!({"not_before": "2026-11-27T09:00:00+03:00", "reminder": " "})),
                "reminder is required",
            ),
            (
                with(
                    &json!({"not_before": "2026-11-27T09:00:00+03:00", "reminder": "x".repeat(32_769)}),
                ),
                "reminder must be at most 32768 bytes of UTF-8",
            ),
            (
                with(
                    &json!({"not_before": "2026-11-27T09:00:00+03:00", "reminder": "r", "review": true}),
                ),
                "a reminder runs no head: give acceptance `true`, and no accept_review, review or recheck",
            ),
            (
                judged,
                "a reminder runs no head: give acceptance `true`, and no accept_review, review or recheck",
            ),
            (
                with(&json!({"reminder": "r", "recheck_every": "1h", "recheck_max": 2})),
                "a reminder runs no head: give acceptance `true`, and no accept_review, review or recheck",
            ),
        ];
        for (args, want) in refusals {
            assert_eq!(refused("hive_create", &args), want, "{args}");
        }
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
            json!({"card_id": "h_0123abcd", "created": false, "state": "queued", "card_class": "owner_task"})
        );
        // A series: its own card waits in `blocked`; the answer says it is one.
        let series = parse(
            "hive_create",
            &with(&json!({"every": "15m", "until": "2026-11-27T18:00:00+03:00"})),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            answer(&series, &exit(0, "{\"id\": \"h_4567abcd\", \"created\": true, \"state\": \"blocked\", \"card_class\": \"series_run\", \"owner_visible\": false}\n", "")).unwrap(),
            json!({"card_id": "h_4567abcd", "created": true, "state": "blocked", "card_class": "series_run", "owner_visible": false})
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

    #[test]
    fn snapshot_tools_run_read_only_commands() {
        assert_eq!(
            args_of("hive_inspect", &json!({"card_id": "h_0123abcd"}), false),
            ["inspect", "--json", "--", "h_0123abcd"]
        );
        assert_eq!(
            args_of("hive_health", &json!({}), false),
            ["health", "--json"]
        );
        assert_eq!(
            args_of("hive_health", &Value::Null, false),
            ["health", "--json"]
        );
        assert_eq!(
            args_of(
                "hive_file",
                &json!({"card_id": "h_0123abcd", "path": "--json", "offset": 10, "limit": 20}),
                false
            ),
            [
                "file",
                "--offset=10",
                "--limit=20",
                "--",
                "h_0123abcd",
                "--json"
            ]
        );
        assert_eq!(
            args_of(
                "hive_list",
                &json!({"card_class": "followup", "parent": "h_0123abcd", "followup_of": "h_4567abcd",
                    "created_by": "secretary", "placement": "hidden", "query": "--state=x", "limit": 5}),
                false
            ),
            [
                "list",
                "--json",
                "--class=followup",
                "--parent=h_0123abcd",
                "--followup-of=h_4567abcd",
                "--creator=secretary",
                "--placement=hidden",
                "--query=--state=x",
                "--limit=5"
            ]
        );
        for (name, args, why) in [
            (
                "hive_file",
                json!({"card_id": "h_0123abcd", "path": "/etc/passwd"}),
                "relative",
            ),
            (
                "hive_file",
                json!({"card_id": "h_0123abcd", "path": "ev/../../x"}),
                "..",
            ),
            (
                "hive_file",
                json!({"card_id": "h_0123abcd", "path": "x", "limit": 0}),
                "limit",
            ),
            (
                "hive_file",
                json!({"card_id": "h_0123abcd", "path": "x", "limit": 262_145}),
                "limit",
            ),
            (
                "hive_file",
                json!({"card_id": "h_0123abcd", "path": "x", "mode": "w"}),
                "unknown field",
            ),
            ("hive_inspect", json!({"card_id": "../x"}), "card_id"),
            ("hive_health", json!({"verbose": true}), "unknown field"),
            ("hive_list", json!({"parent": "x"}), "card_id"),
            ("hive_list", json!({"query": "  "}), "query"),
            (
                "hive_list",
                json!({"placement": "everywhere"}),
                "invalid arguments",
            ),
        ] {
            let text = refused(name, &args);
            assert!(text.contains(why), "{name} {args}: {text}");
        }
    }

    #[test]
    fn snapshot_answers_map_to_c2_outputs() {
        let inspect = parse("hive_inspect", &json!({"card_id": "h_0123abcd"}))
            .unwrap()
            .unwrap();
        let snap = json!({"version": 2, "card_id": "h_0123abcd",
            "card": {"state": "running", "round": 2, "title": "T", "result": null},
            "rounds": [{"runs": [{"report": "RESULT: first"}]}, {"runs": [{"report": null}]}],
            "heads": {"heads": [{"waiting_on": {"command": "until gh pr checks 7;\n  do sleep 30; done",
                "for_s": 900.4, "pull_requests": [7], "workflow_runs": [55]}}]}});
        let out = answer(&inspect, &exit(0, &snap.to_string(), "")).unwrap();
        assert_eq!(out["card_id"], "h_0123abcd");
        assert_eq!(out["state"], "running");
        assert_eq!(out["round"], 2);
        assert_eq!(out["report_line"], "RESULT: first");
        assert_eq!(
            out["waiting_on"],
            "900 s: until gh pr checks 7; do sleep 30; done PR #7 run 55"
        );
        assert_eq!(out["snapshot"], snap);
        assert_eq!(
            answer(&inspect, &exit(1, "", "no such card\n")),
            Err(Refusal("no such card h_0123abcd".into()))
        );
        assert!(answer(&inspect, &exit(0, "{}", "")).is_err());

        let health = parse("hive_health", &json!({})).unwrap().unwrap();
        let snap = json!({"dispatcher": {"ticking": true, "tick_age_s": 3.5},
            "slots": {"used": 4, "concurrency": 5}, "queue": {"length": 11}});
        let out = answer(&health, &exit(0, &snap.to_string(), "")).unwrap();
        assert_eq!(
            [
                &out["ticking"],
                &out["tick_age_s"],
                &out["slots_used"],
                &out["concurrency"],
                &out["queue_length"]
            ],
            [&json!(true), &json!(3.5), &json!(4), &json!(5), &json!(11)]
        );

        let file = parse(
            "hive_file",
            &json!({"card_id": "h_0123abcd", "path": "REPORT.md"}),
        )
        .unwrap()
        .unwrap();
        let text = json!({"path": "REPORT.md", "size": 5, "offset": 0, "text": "hello", "truncated": false});
        assert_eq!(
            answer(&file, &exit(0, &text.to_string(), "")).unwrap(),
            text
        );
        assert_eq!(
            answer(
                &file,
                &exit(
                    2,
                    "",
                    "hive: ../x: only a relative path inside the workspace\n"
                )
            ),
            Err(Refusal(
                "../x: only a relative path inside the workspace".into()
            ))
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
