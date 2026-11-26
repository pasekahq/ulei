//! `ulei-garden places import`: reads a places file in the format of ADR-0080 section 4. Every
//! line is checked first, by its syntax and then by rehearsing the writes of the whole file on a
//! scratch in-memory garden through the same C7 methods; only a file without a refused line is
//! written, through the daemon's `places_write`, `places_verdict` and `places_profile`.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::api::{MAX_PLACE_ATTRS, MAX_PLACES_BATCH, as_f64};
use crate::edge::normalise;
use crate::model::UnixTime;
use crate::places_wire::{BedWire, CallError, Rehearsal};
use crate::wire::c7;

const DAY: i64 = 86_400;

/// A line of the file the import refuses, with the rule it breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedLine {
    /// Its number from 1.
    pub line: usize,
    /// The rule: a syntax rule of the format or the C7 `PlaceRefusal` of the bed.
    pub rule: String,
    /// What is wrong.
    pub message: String,
}

impl RefusedLine {
    fn new(line: usize, rule: &str, message: impl Into<String>) -> Self {
        Self {
            line,
            rule: rule.to_owned(),
            message: message.into(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"line": self.line, "rule": self.rule, "message": self.message})
    }
}

/// Why an import wrote nothing, or stopped.
#[derive(Debug)]
pub enum ImportError {
    /// These lines are refused; nothing was written.
    Refused(Vec<RefusedLine>),
    /// The file, the rehearsal, the socket or the daemon failed.
    Failed(String),
}

impl ImportError {
    /// The one JSON error the command prints.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Refused(lines) => json!({
                "error": "refused",
                "lines": lines.iter().map(RefusedLine::to_json).collect::<Vec<_>>(),
            }),
            Self::Failed(message) => json!({"error": "failed", "message": message}),
        }
    }
}

/// A place of the file with its fields, each with its line.
#[derive(Debug)]
struct Place {
    line: usize,
    name: String,
    status: c7::PlaceStatus,
    attrs: Vec<(usize, c7::PlaceObservation)>,
    verdicts: Vec<(usize, Verdict)>,
}

#[derive(Debug)]
struct Verdict {
    grade: i64,
    note: Option<String>,
    said_at: f64,
    source_ref: String,
}

/// The writes of a places file, every line accepted by its syntax.
#[derive(Debug)]
pub struct Plan {
    source: String,
    places: Vec<Place>,
    profile: Vec<(usize, c7::PlacesProfileRequest)>,
    unconfirmed: usize,
}

impl Plan {
    /// The counts of `--dry-run`: places by status, attribute observations, unconfirmed values,
    /// verdicts and profile rows.
    pub fn counts(&self) -> Value {
        let by = |s: c7::PlaceStatus| self.places.iter().filter(|p| p.status == s).count();
        json!({
            "dry_run": true,
            "places": {
                "active": by(c7::PlaceStatus::Active),
                "candidate": by(c7::PlaceStatus::Candidate),
                "gone": by(c7::PlaceStatus::Gone),
            },
            "attrs": self.places.iter().map(|p| p.attrs.len()).sum::<usize>(),
            "unconfirmed": self.unconfirmed,
            "verdicts": self.places.iter().map(|p| p.verdicts.len()).sum::<usize>(),
            "profile": self.profile.len(),
        })
    }
}

/// What a write of a plan did.
#[derive(Debug, Default)]
pub struct Written {
    places: HashMap<&'static str, usize>,
    attrs: HashMap<&'static str, usize>,
    verdicts: HashMap<&'static str, usize>,
    profile: HashMap<&'static str, usize>,
    unconfirmed: usize,
    /// Lines the bed refused; the rest was written.
    pub refused: Vec<RefusedLine>,
}

impl Written {
    /// The report the command prints.
    pub fn to_json(&self) -> Value {
        let n = |m: &HashMap<&str, usize>, k: &str| m.get(k).copied().unwrap_or(0);
        json!({
            "dry_run": false,
            "places": {
                "created": n(&self.places, "created"),
                "adopted": n(&self.places, "adopted"),
                "found": n(&self.places, "found"),
            },
            "attrs": {
                "written": n(&self.attrs, "written"),
                "replaced": n(&self.attrs, "replaced"),
                "history": n(&self.attrs, "history"),
                "unchanged": n(&self.attrs, "unchanged"),
            },
            "unconfirmed": self.unconfirmed,
            "verdicts": {
                "written": n(&self.verdicts, "written"),
                "repeated": n(&self.verdicts, "repeated"),
            },
            "profile": {
                "written": n(&self.profile, "written"),
                "unchanged": n(&self.profile, "unchanged"),
            },
            "refused": self.refused.iter().map(RefusedLine::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Parses `text`, the places file `file_name`, and rehearses its writes at `now`: the plan of a
/// file without a refused line.
///
/// # Errors
///
/// [`ImportError::Refused`] with every refused line, by line number;
/// [`ImportError::Failed`] when the rehearsal itself fails.
pub fn check(text: &str, file_name: &str, now: UnixTime) -> Result<Plan, ImportError> {
    let (plan, mut refused) = parse(text, file_name);
    let mut rehearsal = Rehearsal::new(now).map_err(ImportError::Failed)?;
    refused.extend(write(&plan, &mut rehearsal)?.refused);
    if refused.is_empty() {
        return Ok(plan);
    }
    refused.sort_by_key(|r| r.line);
    Err(ImportError::Refused(refused))
}

/// Writes `plan` through `wire`: the places in batches of [`MAX_PLACES_BATCH`], each place's
/// verdicts after its batch, then the profile rows.
///
/// # Errors
///
/// [`ImportError::Failed`] for a call that fails other than by a refusal; what was written
/// before it stays.
pub fn write(plan: &Plan, wire: &mut impl BedWire) -> Result<Written, ImportError> {
    let failed = |m: String| ImportError::Failed(m);
    let mut out = Written {
        unconfirmed: plan.unconfirmed,
        ..Written::default()
    };
    for batch in plan.places.chunks(MAX_PLACES_BATCH) {
        let req = c7::PlacesWriteRequest {
            places: batch
                .iter()
                .map(|p| c7::PlaceWrite {
                    id: None,
                    name: Some(p.name.clone()),
                    status: p.status,
                    source: plan.source.clone(),
                    attrs: (!p.attrs.is_empty())
                        .then(|| p.attrs.iter().map(|(_, o)| o.clone()).collect()),
                })
                .collect(),
        };
        let answer = match wire.places_write(&req) {
            Ok(answer) => answer,
            Err(CallError::Refused(rule, m)) => {
                return Err(failed(format!("places_write refused: {rule}: {m}")));
            }
            Err(CallError::Failed(m)) => return Err(failed(m)),
        };
        if answer.places.len() != batch.len() {
            return Err(failed("places_write: answer of another length".into()));
        }
        for (p, w) in batch.iter().zip(&answer.places) {
            let Some(id) = &w.id else {
                out.refused
                    .push(refusal(p.line, w.refused, w.message.as_deref()));
                continue;
            };
            *out.places.entry(w.outcome.as_str()).or_default() += 1;
            if w.attrs.len() != p.attrs.len() {
                return Err(failed("places_write: answer of another length".into()));
            }
            for ((line, _), a) in p.attrs.iter().zip(&w.attrs) {
                if a.outcome == c7::AttrOutcome::Refused {
                    out.refused
                        .push(refusal(*line, a.refused, a.message.as_deref()));
                } else {
                    *out.attrs.entry(a.outcome.as_str()).or_default() += 1;
                }
            }
            for (line, v) in &p.verdicts {
                let req = c7::PlacesVerdictRequest {
                    place: id.clone(),
                    verdict: v.grade,
                    note: v.note.clone(),
                    said_at: v.said_at,
                    source_ref: v.source_ref.clone(),
                    better_than: None,
                };
                match wire.places_verdict(&req) {
                    Ok(said) => *out.verdicts.entry(said.outcome.as_str()).or_default() += 1,
                    Err(CallError::Refused(rule, m)) => {
                        out.refused.push(RefusedLine::new(*line, &rule, m));
                    }
                    Err(CallError::Failed(m)) => return Err(failed(m)),
                }
            }
        }
    }
    for (line, req) in &plan.profile {
        match wire.places_profile(req) {
            Ok(set) => *out.profile.entry(set.outcome.as_str()).or_default() += 1,
            Err(CallError::Refused(rule, m)) => out.refused.push(RefusedLine::new(*line, &rule, m)),
            Err(CallError::Failed(m)) => return Err(failed(m)),
        }
    }
    Ok(out)
}

/// The line of a refused `places_write` answer; only a daemon that breaks C7 leaves out its rule
/// or message.
fn refusal(line: usize, rule: Option<c7::PlaceRefusal>, message: Option<&str>) -> RefusedLine {
    let rule = rule.map_or("refused", c7::PlaceRefusal::as_str);
    RefusedLine::new(line, rule, message.unwrap_or(rule))
}

/// Where the parser is in the file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Head,
    /// A refused section: its lines are not read.
    Unknown,
    Profile,
    Places(c7::PlaceStatus),
}

/// One field line `- <key>: <value> [<source>, <date>]`; `bracket` is `None` without one.
struct Field<'a> {
    key: &'a str,
    value: &'a str,
    bracket: Option<(&'a str, &'a str)>,
}

/// The plan of `text` with every line its syntax refuses.
fn parse(text: &str, file_name: &str) -> (Plan, Vec<RefusedLine>) {
    let mut p = Parser {
        plan: Plan {
            source: format!("import:{file_name}"),
            places: Vec::new(),
            profile: Vec::new(),
            unconfirmed: 0,
        },
        refused: Vec::new(),
        as_of: None,
        section: Section::Head,
        in_place: false,
        names: HashSet::new(),
        dated_keys: HashSet::new(),
        refs: HashSet::new(),
        profile_keys: HashSet::new(),
    };
    for (n, raw) in text.lines().enumerate() {
        let t = raw.trim();
        if !t.is_empty() {
            p.line(n + 1, t);
        }
    }
    if p.as_of.is_none() && p.section == Section::Head {
        p.refuse(1, "no_as_of", "no `as_of` line");
    }
    (p.plan, p.refused)
}

/// The state of [`parse`].
struct Parser {
    plan: Plan,
    refused: Vec<RefusedLine>,
    as_of: Option<i64>,
    section: Section,
    /// Whether a place heading opened the current place.
    in_place: bool,
    /// Normalised names of the place headings so far.
    names: HashSet<String>,
    /// Keys with their dates, verdict source refs: of the current place.
    dated_keys: HashSet<(String, i64)>,
    refs: HashSet<String>,
    /// Profile keys so far, a `ban` with its value.
    profile_keys: HashSet<(String, String)>,
}

impl Parser {
    fn refuse(&mut self, line: usize, rule: &str, message: impl Into<String>) {
        self.refused.push(RefusedLine::new(line, rule, message));
    }

    /// Reads the non-blank line `t` numbered `line`.
    fn line(&mut self, line: usize, t: &str) {
        if let Some(rest) = t.strip_prefix("as_of:") {
            self.as_of_line(line, rest.trim());
        } else if let Some(name) = t.strip_prefix("### ") {
            self.place(line, name.trim());
        } else if let Some(name) = t.strip_prefix("## ") {
            self.section(line, name.trim());
        } else if self.section == Section::Unknown
            || t.starts_with("# ") && self.section == Section::Head
        {
            // The lines of a refused section, and a title.
        } else if let Some(field) = field(t) {
            self.field(line, &field);
        } else {
            self.refuse(
                line,
                "unknown_line",
                "not a heading, an `as_of` line or a field",
            );
        }
    }

    fn as_of_line(&mut self, line: usize, date: &str) {
        if self.section != Section::Head {
            self.refuse(
                line,
                "misplaced_as_of",
                "`as_of` stands before the first section",
            );
        } else if self.as_of.is_some() {
            self.refuse(line, "repeated_as_of", "a second `as_of` line");
        } else if let Some(d) = full_date(date) {
            self.as_of = Some(d);
        } else {
            self.refuse(line, "bad_as_of", format!("{date:?} is not YYYY-MM-DD"));
        }
    }

    fn section(&mut self, line: usize, name: &str) {
        self.in_place = false;
        if self.as_of.is_none() && self.section == Section::Head {
            self.refuse(line, "no_as_of", "no `as_of` line before the first section");
        }
        self.section = match name.to_lowercase().as_str() {
            "profile" => Section::Profile,
            "active" => Section::Places(c7::PlaceStatus::Active),
            "candidates" => Section::Places(c7::PlaceStatus::Candidate),
            "gone" => Section::Places(c7::PlaceStatus::Gone),
            _ => {
                self.refuse(line, "unknown_section", format!("section {name:?}"));
                Section::Unknown
            }
        };
    }

    fn place(&mut self, line: usize, heading: &str) {
        self.in_place = false;
        let status = match self.section {
            Section::Places(status) => status,
            Section::Unknown => return,
            Section::Head | Section::Profile => {
                return self.refuse(
                    line,
                    "place_outside_section",
                    "a place stands in no places section",
                );
            }
        };
        let name = drop_number(heading);
        if name.is_empty() {
            return self.refuse(line, "empty_name", "the heading names no place");
        }
        if !self.names.insert(normalise(name)) {
            return self.refuse(
                line,
                "repeated_place",
                format!("{name:?} is a heading already"),
            );
        }
        self.plan.places.push(Place {
            line,
            name: name.to_owned(),
            status,
            attrs: Vec::new(),
            verdicts: Vec::new(),
        });
        self.dated_keys.clear();
        self.refs.clear();
        self.in_place = true;
    }

    /// A field: its bracket and date, then the profile row, verdict or observation it is.
    fn field(&mut self, line: usize, f: &Field<'_>) {
        let profile = self.section == Section::Profile;
        if !profile && !self.in_place {
            return self.refuse(
                line,
                "field_outside_place",
                "a field stands under no place heading",
            );
        }
        if f.value.eq_ignore_ascii_case("n/a") {
            self.plan.unconfirmed += 1;
            return;
        }
        let Some((source, date)) = f.bracket else {
            return self.refuse(line, "no_bracket", "the value has no `[<source>, <date>]`");
        };
        if source.is_empty() {
            return self.refuse(line, "no_source", "the bracket names no source");
        }
        if date.is_empty() {
            return self.refuse(line, "no_date", "the bracket has no date");
        }
        // Without `as_of`, which is refused already, no date can be read.
        let Some(as_of) = self.as_of else {
            return;
        };
        let Some(at) = read_date(date, as_of) else {
            return self.refuse(
                line,
                "bad_date",
                format!("{date:?} is no YYYY-MM-DD, DD.MM, YYYY-MM or MM.YYYY"),
            );
        };
        if at > as_of {
            return self.refuse(line, "date_after_as_of", format!("{date} is after `as_of`"));
        }
        if profile {
            self.profile_row(line, f, source, at);
        } else if f.key == "verdict" {
            self.verdict(line, f.value, source, at);
        } else {
            self.observation(line, f, source, at);
        }
    }

    fn profile_row(&mut self, line: usize, f: &Field<'_>, source: &str, at: i64) {
        let Ok(key) = serde_json::from_value::<c7::ProfileKey>(Value::from(f.key)) else {
            return self.refuse(
                line,
                "unknown_key",
                format!("{:?} is no profile key", f.key),
            );
        };
        let value = if key == c7::ProfileKey::Ban {
            f.value
        } else {
            ""
        };
        if !self
            .profile_keys
            .insert((f.key.to_owned(), value.to_owned()))
        {
            return self.refuse(
                line,
                "repeated_key",
                format!("a second line of {:?}", f.key),
            );
        }
        self.plan.profile.push((
            line,
            c7::PlacesProfileRequest {
                key,
                value: f.value.to_owned(),
                source_ref: source.to_owned(),
                set_at: as_f64(at),
            },
        ));
    }

    fn verdict(&mut self, line: usize, value: &str, source: &str, at: i64) {
        let (grade, note) = value
            .split_once(char::is_whitespace)
            .map_or((value, None), |(g, n)| (g, Some(n.trim())));
        let grade = match grade {
            "1" | "+1" => 1,
            "0" => 0,
            "-1" => -1,
            _ => {
                return self.refuse(
                    line,
                    "bad_verdict",
                    format!("{grade:?} is not 1, +1, 0 or -1"),
                );
            }
        };
        if !self.refs.insert(source.to_owned()) {
            return self.refuse(
                line,
                "repeated_source_ref",
                format!("a second verdict of {source:?}"),
            );
        }
        if let Some(place) = self.plan.places.last_mut() {
            place.verdicts.push((
                line,
                Verdict {
                    grade,
                    note: note.filter(|n| !n.is_empty()).map(str::to_owned),
                    said_at: as_f64(at),
                    source_ref: source.to_owned(),
                },
            ));
        }
    }

    fn observation(&mut self, line: usize, f: &Field<'_>, source: &str, at: i64) {
        if !self.dated_keys.insert((f.key.to_owned(), at)) {
            return self.refuse(
                line,
                "repeated_key",
                format!("a second line of {:?} with this date", f.key),
            );
        }
        let Some(place) = self.plan.places.last_mut() else {
            return;
        };
        if place.attrs.len() == MAX_PLACE_ATTRS {
            return self.refuse(
                line,
                "too_many_fields",
                format!("a place holds at most {MAX_PLACE_ATTRS} fields"),
            );
        }
        place.attrs.push((
            line,
            c7::PlaceObservation {
                key: f.key.to_owned(),
                value: f.value.to_owned(),
                checked_at: as_f64(at),
                source: source.to_owned(),
                confidence: None,
            },
        ));
    }
}

/// `name` without a leading `<n>. `.
fn drop_number(name: &str) -> &str {
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    match name[digits..].strip_prefix('.') {
        Some(rest) if digits > 0 && (rest.is_empty() || rest.starts_with(char::is_whitespace)) => {
            rest.trim()
        }
        _ => name,
    }
}

/// A field line: its key of lowercase letters, digits and `_`, its value and its bracket.
fn field(t: &str) -> Option<Field<'_>> {
    let rest = t.strip_prefix("- ")?;
    let (key, rest) = rest.split_once(':')?;
    let key = key.trim();
    let mut chars = key.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase())
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let rest = rest.trim();
    let bracket = rest
        .strip_suffix(']')
        .and_then(|r| r.rfind('[').map(|i| (r[..i].trim(), &r[i + 1..])));
    Some(match bracket {
        Some((value, inner)) => {
            let (source, date) = inner.rsplit_once(',').unwrap_or((inner, ""));
            Field {
                key,
                value,
                bracket: Some((source.trim(), date.trim())),
            }
        }
        None => Field {
            key,
            value: rest,
            bracket: None,
        },
    })
}

/// The Unix time of 00:00 UTC of `date`: `YYYY-MM-DD`; `DD.MM` in the latest year that puts it
/// on or before `as_of`; `YYYY-MM` or `MM.YYYY` the first day of the month.
fn read_date(date: &str, as_of: i64) -> Option<i64> {
    if let Some(t) = full_date(date) {
        return Some(t);
    }
    let num = |s: &str, len: usize| {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().ok())?
    };
    if let Some((y, m)) = date.split_once('-') {
        return civil(num(y, 4)?, num(m, 2)?, 1);
    }
    let (a, b) = date.split_once('.')?;
    if b.len() == 4 {
        return civil(num(b, 4)?, num(a, 2)?, 1);
    }
    let (d, m) = (num(a, 2)?, num(b, 2)?);
    let (year, _, _) = from_days(as_of.div_euclid(DAY));
    // February 29 waits for the latest leap year.
    (0..8)
        .filter_map(|back| civil(year - back, m, d))
        .find(|&t| t <= as_of)
}

/// `YYYY-MM-DD` as the Unix time of its 00:00 UTC.
fn full_date(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let ok = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some() || !ok(y, 4) || !ok(m, 2) || !ok(d, 2) {
        return None;
    }
    civil(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// The Unix time of 00:00 UTC of a real date from the year 1970.
fn civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if y < 1970 || !(1..=days_in).contains(&d) {
        return None;
    }
    Some(days(y, m, d) * DAY)
}

/// Days from 1970-01-01 of a proleptic Gregorian date.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year, month and day of `z` days from 1970-01-01.
fn from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AS_OF: i64 = 1_931_731_200; // 2031-03-20

    #[test]
    fn dates_read_as_their_earliest_midnight_on_or_before_as_of() {
        assert_eq!(full_date("2031-03-20"), Some(AS_OF));
        assert_eq!(read_date("17.03", AS_OF), Some(AS_OF - 3 * DAY));
        assert_eq!(read_date("20.03", AS_OF), Some(AS_OF));
        assert_eq!(read_date("21.03", AS_OF), full_date("2030-03-21"));
        assert_eq!(read_date("03.2031", AS_OF), full_date("2031-03-01"));
        assert_eq!(read_date("2031-02", AS_OF), full_date("2031-02-01"));
        assert_eq!(read_date("29.02", AS_OF), full_date("2028-02-29"));
        for bad in [
            "2031-02-30",
            "32.01",
            "2031/03/01",
            "3.2031",
            "2031-3",
            "March",
            "",
        ] {
            assert_eq!(read_date(bad, AS_OF), None, "{bad}");
        }
    }
}
