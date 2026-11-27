//! `ulei-garden places search|add-candidate|verdict`: the secretary's commands over the places
//! bed (ADR-0080 section 7), clients of the daemon's socket that answer one line of JSON. A write
//! is rehearsed on a scratch in-memory garden first, so a refusal the bed's rules decide writes
//! nothing.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::api::as_f64;
use crate::model::UnixTime;
use crate::places::point;
use crate::places_wire::{BedWire, CallError, Rehearsal, Socket, write_refusal};
use crate::serve::socket;
use crate::wire::c7;

/// One rule a command broke, with the attribute it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The attribute's key; absent for the place, the verdict or the search.
    pub key: Option<String>,
    /// A C7 `PlaceRefusal` or error code.
    pub rule: String,
    /// What is wrong.
    pub message: String,
}

/// Why a command wrote nothing, or stopped.
#[derive(Debug)]
pub enum CliError {
    /// The arguments do not form a command.
    Usage(String),
    /// The bed or the daemon refused the command.
    Refused(Vec<Refused>),
    /// The config, the socket or the daemon failed.
    Failed(String),
}

impl CliError {
    /// The one JSON error the command prints.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Usage(message) => json!({"error": "usage", "message": message}),
            Self::Refused(refused) => json!({
                "error": "refused",
                "refused": refused.iter().map(|r| {
                    let mut v = json!({"rule": r.rule, "message": r.message});
                    if let Some(key) = &r.key {
                        v["key"] = Value::from(key.as_str());
                    }
                    v
                }).collect::<Vec<_>>(),
            }),
            Self::Failed(message) => json!({"error": "failed", "message": message}),
        }
    }

    /// The exit code: 2 for arguments that form no command, else 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Refused(_) | Self::Failed(_) => 1,
        }
    }
}

impl From<CallError> for CliError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Refused(rule, message) => Self::Refused(vec![Refused {
                key: None,
                rule,
                message,
            }]),
            CallError::Failed(m) => Self::Failed(m),
        }
    }
}

/// Runs the subcommand `sub` (`search`, `add-candidate` or `verdict`) with `args` at `now`; the
/// one line of JSON it prints on success.
///
/// # Errors
///
/// [`CliError`]; every refusal and every usage error writes nothing.
pub fn run(sub: &str, args: &[OsString], now: UnixTime) -> Result<String, CliError> {
    let mut flags = Flags::read(args)?;
    let config = flags.one("--config")?.map(PathBuf::from);
    let out = match sub {
        "search" => {
            let req = search_request(&mut flags)?;
            let answer = Socket(path(config.as_deref())?).places_search(&req)?;
            String::from_utf8(answer)
                .map_err(|_| CliError::Failed("places_search: malformed answer".into()))?
                .trim()
                .to_owned()
        }
        "add-candidate" => add_candidate(&mut flags, config.as_deref(), now)?.to_string(),
        "verdict" => verdict(&mut flags, config.as_deref(), now)?.to_string(),
        _ => return Err(CliError::Usage(format!("no places command {sub:?}"))),
    };
    Ok(out)
}

fn path(config: Option<&Path>) -> Result<PathBuf, CliError> {
    socket(config).map_err(CliError::Failed)
}

/// The arguments of a command: its flags with their values, in order, and its positionals.
struct Flags {
    pairs: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

/// Flags that take no value.
const SWITCHES: [&str; 1] = ["--open-now"];

impl Flags {
    fn read(args: &[OsString]) -> Result<Self, CliError> {
        let text = |a: &OsStr| {
            a.to_str()
                .map(str::to_owned)
                .ok_or_else(|| CliError::Usage(format!("{} is not UTF-8", a.display())))
        };
        let (mut pairs, mut positional) = (Vec::new(), Vec::new());
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = text(arg)?;
            if SWITCHES.contains(&arg.as_str()) {
                pairs.push((arg, None));
            } else if arg.starts_with("--") {
                let value = args
                    .next()
                    .ok_or_else(|| CliError::Usage(format!("{arg} needs a value")))?;
                pairs.push((arg, Some(text(value)?)));
            } else {
                positional.push(arg);
            }
        }
        Ok(Self { pairs, positional })
    }

    /// Every value of `flag`, taken out of the arguments.
    fn all(&mut self, flag: &str) -> Vec<String> {
        let (taken, rest) = std::mem::take(&mut self.pairs)
            .into_iter()
            .partition::<Vec<_>, _>(|(f, _)| f == flag);
        self.pairs = rest;
        taken.into_iter().filter_map(|(_, v)| v).collect()
    }

    /// The value of `flag` given at most once.
    fn one(&mut self, flag: &str) -> Result<Option<String>, CliError> {
        let mut values = self.all(flag);
        if values.len() > 1 {
            return Err(CliError::Usage(format!("{flag} is given twice")));
        }
        Ok(values.pop())
    }

    fn switch(&mut self, flag: &str) -> Result<bool, CliError> {
        let n = self.pairs.iter().filter(|(f, _)| f == flag).count();
        if n > 1 {
            return Err(CliError::Usage(format!("{flag} is given twice")));
        }
        self.pairs.retain(|(f, _)| f != flag);
        Ok(n == 1)
    }

    /// Refuses whatever the command did not take.
    fn done(&self, positionals: usize) -> Result<(), CliError> {
        if let Some((flag, _)) = self.pairs.first() {
            return Err(CliError::Usage(format!("unknown flag {flag}")));
        }
        if let Some(extra) = self.positional.get(positionals) {
            return Err(CliError::Usage(format!("unexpected argument {extra:?}")));
        }
        Ok(())
    }
}

/// `value` of `flag` read as `T`.
fn parsed<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<Option<T>, CliError> {
    value
        .map(|v| {
            v.parse()
                .map_err(|_| CliError::Usage(format!("{flag} {v:?} is not a valid value")))
        })
        .transpose()
}

/// The `PlacesSearchRequest` of `--request`, else of the filter flags.
fn search_request(flags: &mut Flags) -> Result<c7::PlacesSearchRequest, CliError> {
    if let Some(json) = flags.one("--request")? {
        flags.done(0)?;
        return serde_json::from_str(&json).map_err(|e| CliError::Usage(format!("--request: {e}")));
    }
    let cuisine = flags.all("--cuisine");
    let status = flags
        .all("--status")
        .iter()
        .map(|s| {
            serde_json::from_value::<c7::PlaceStatus>(Value::from(s.as_str()))
                .map_err(|_| CliError::Usage(format!("--status {s:?} is not a place status")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let filters = c7::PlacesFilters {
        cuisine: (!cuisine.is_empty()).then_some(cuisine),
        max_distance_m: parsed("--max-distance-m", flags.one("--max-distance-m")?)?,
        min_rating: parsed("--min-rating", flags.one("--min-rating")?)?,
        max_price_for_two: parsed("--max-price-for-two", flags.one("--max-price-for-two")?)?,
        status: (!status.is_empty()).then_some(status),
    };
    let empty = c7::PlacesFilters {
        cuisine: None,
        max_distance_m: None,
        min_rating: None,
        max_price_for_two: None,
        status: None,
    };
    let req = c7::PlacesSearchRequest {
        filters: (filters != empty).then_some(filters),
        near: flags
            .one("--near")?
            .map(|v| {
                point(&v)
                    .map(|(lat, lon)| c7::GeoPoint { lat, lon })
                    .ok_or_else(|| CliError::Usage(format!("--near {v:?} is not a point")))
            })
            .transpose()?,
        open_now: flags.switch("--open-now")?.then_some(true),
        local_time: flags.one("--local-time")?,
        vibe: flags.one("--vibe")?,
        limit: parsed("--limit", flags.one("--limit")?)?,
    };
    flags.done(0)?;
    Ok(req)
}

/// The Unix time of `flag`, else `now`.
fn time(flags: &mut Flags, flag: &str, now: UnixTime) -> Result<f64, CliError> {
    Ok(as_f64(parsed(flag, flags.one(flag)?)?.unwrap_or(now)))
}

fn required(flags: &mut Flags, flag: &str) -> Result<String, CliError> {
    flags
        .one(flag)?
        .ok_or_else(|| CliError::Usage(format!("{flag} is required")))
}

/// The place answer of a one-place `places_write`, refused when the place or an attribute is.
fn one_place(answer: c7::PlacesWriteResponse) -> Result<c7::PlaceWritten, CliError> {
    let [place] = <[c7::PlaceWritten; 1]>::try_from(answer.places)
        .map_err(|_| CliError::Failed("places_write: answer of another length".into()))?;
    let refused = |key: Option<&str>, rule: Option<c7::PlaceRefusal>, message: &Option<String>| {
        let (rule, message) = write_refusal(rule, message.as_deref());
        Refused {
            key: key.map(str::to_owned),
            rule: rule.to_owned(),
            message: message.to_owned(),
        }
    };
    if place.outcome == c7::PlaceOutcome::Refused {
        return Err(CliError::Refused(vec![refused(
            None,
            place.refused,
            &place.message,
        )]));
    }
    let attrs: Vec<_> = place
        .attrs
        .iter()
        .filter(|a| a.outcome == c7::AttrOutcome::Refused)
        .map(|a| refused(Some(&a.key), a.refused, &a.message))
        .collect();
    if attrs.is_empty() {
        Ok(place)
    } else {
        Err(CliError::Refused(attrs))
    }
}

/// The id of a written place; only a daemon that breaks C7 leaves it out.
fn placed(place: c7::PlaceWritten) -> Result<String, CliError> {
    place
        .id
        .ok_or_else(|| CliError::Failed("places_write: a place without an id".into()))
}

/// The `places_write` of one place of `status` `candidate`.
fn candidate(
    id: Option<String>,
    name: Option<String>,
    source: &str,
    attrs: Option<Vec<c7::PlaceObservation>>,
) -> c7::PlacesWriteRequest {
    c7::PlacesWriteRequest {
        places: vec![c7::PlaceWrite {
            id,
            name,
            status: c7::PlaceStatus::Candidate,
            source: source.to_owned(),
            attrs,
        }],
    }
}

/// `add-candidate NAME --source SOURCE [--attr KEY=VALUE]... [--checked-at UNIX]`: the
/// per-place and per-attribute answer of its `places_write`.
fn add_candidate(
    flags: &mut Flags,
    config: Option<&Path>,
    now: UnixTime,
) -> Result<Value, CliError> {
    let source = required(flags, "--source")?;
    let checked_at = time(flags, "--checked-at", now)?;
    let attrs = flags
        .all("--attr")
        .into_iter()
        .map(|a| {
            let (key, value) = a
                .split_once('=')
                .ok_or_else(|| CliError::Usage(format!("--attr {a:?} is not KEY=VALUE")))?;
            Ok(c7::PlaceObservation {
                key: key.trim().to_owned(),
                value: value.trim().to_owned(),
                checked_at,
                source: source.clone(),
                confidence: None,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    flags.done(1)?;
    let name = flags
        .positional
        .first()
        .cloned()
        .ok_or_else(|| CliError::Usage("add-candidate needs the place's name".into()))?;
    let req = candidate(
        None,
        Some(name),
        &source,
        (!attrs.is_empty()).then_some(attrs),
    );
    one_place(
        Rehearsal::new(now)
            .map_err(CliError::Failed)?
            .places_write(&req)?,
    )?;
    let place = one_place(Socket(path(config)?).places_write(&req)?)?;
    serde_json::to_value(place).map_err(|e| CliError::Failed(e.to_string()))
}

/// `verdict (--id ID | --name NAME) --verdict V --source-ref REF [--note TEXT] [--said-at UNIX]
/// [--better-than ID]`: the outcome, the row's `seq` and the place's id. A name no bed place
/// holds becomes a candidate whose source is the `source_ref`.
fn verdict(flags: &mut Flags, config: Option<&Path>, now: UnixTime) -> Result<Value, CliError> {
    let id = flags.one("--id")?;
    let name = flags.one("--name")?;
    let grade = match required(flags, "--verdict")?.as_str() {
        "-1" => -1,
        "0" => 0,
        "1" | "+1" => 1,
        v => {
            return Err(CliError::Usage(format!(
                "--verdict {v:?} is not -1, 0, 1 or +1"
            )));
        }
    };
    let source_ref = required(flags, "--source-ref")?;
    let note = flags.one("--note")?;
    let said_at = time(flags, "--said-at", now)?;
    let better_than = flags.one("--better-than")?;
    flags.done(0)?;
    let request = |place: String, better_than: Option<String>| c7::PlacesVerdictRequest {
        place,
        verdict: grade,
        note: note.clone(),
        said_at,
        source_ref: source_ref.clone(),
        better_than,
    };
    let mut socket = Socket(path(config)?);
    let place = match (id, name) {
        (Some(id), None) => id,
        (None, Some(name)) => {
            // The place is written before the verdict, so the verdict is rehearsed with it and
            // `better_than` is looked up first: a refused verdict then writes no place.
            let mut rehearsal = Rehearsal::new(now).map_err(CliError::Failed)?;
            let place = candidate(None, Some(name), &source_ref, None);
            let rehearsed = placed(one_place(rehearsal.places_write(&place)?)?)?;
            rehearsal.places_verdict(&request(rehearsed, None))?;
            if let Some(better) = &better_than {
                let lookup = candidate(Some(better.clone()), None, &source_ref, None);
                one_place(socket.places_write(&lookup)?)?;
            }
            placed(one_place(socket.places_write(&place)?)?)?
        }
        _ => return Err(CliError::Usage("give one of --id and --name".into())),
    };
    let said = socket.places_verdict(&request(place.clone(), better_than))?;
    Ok(json!({"outcome": said.outcome.as_str(), "seq": said.seq, "place": place}))
}
