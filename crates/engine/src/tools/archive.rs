//! The `archive_search` and `archive_read` tools. `archive_search` walks the tiers in order: the
//! garden's C7 `search`, the history index, then the archive's C10 `search`, which it asks only
//! when both earlier tiers find nothing; an archive hit loses its forgotten dialogue entries,
//! and every archive hit it returns is lifted into the garden's seedbed through C7 `propose`.

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::{c4g, c7, c10};

use super::forget::{garden_failure, history_hit};
use super::{
    ARCHIVE_READ, ARCHIVE_SEARCH, Args, Context, MAX_READ_CHARS, Output, json_array, json_object,
    json_str, object_schema,
};
use crate::compress::to_u64;
use crate::journal::Journal;
use crate::journal::search::{Hit, Search, Sort};
use crate::zone::rfc3339;
use crate::{archive, garden};

/// Most hits of each tier, and the default.
const MAX_LIMIT: i64 = 10;
/// History rows read before the current session's live ones are left out.
const HISTORY_SCAN: u32 = 300;
/// The C10 source of the engine's dialogue, whose `external_id` is
/// `<session_key>/<session>/<seq>`.
const DIALOGUE: &str = "dialogue";

const SEARCH_DESCRIPTION: &str = "Search what is remembered, tier by tier: the garden's facts \
    and its seedbed, then the history of past sessions, then the archive of raw originals \
    (dialogue and connector items). The archive is the last tier: it is not searched when the \
    garden or the history finds something, and the answer says so. since and until narrow the \
    history and the archive; source, who and kind narrow the archive only. Open an archive hit's \
    original with archive_read. Every archive hit is lifted into the seedbed as an unprocessed \
    record: its seed is the record's ref seed:<seq>, which forget takes, and lift_error says how \
    many hits were not lifted and why.";

const READ_DESCRIPTION: &str = "Read the original of an archive item by the id archive_search \
    gave: a text original as text, any other as its length and media type.";

const NOT_SEARCHED: &str =
    "the archive was not searched: the garden or the history found something";
const NO_GARDEN: &str = "the garden is not configured (garden.socket is unset)";
const NO_ARCHIVE: &str = "the archive is not configured (memory.socket is unset)";
const NOT_SYNCED: &str = "the archive could not pull the engine's latest dialogue first; recent \
    messages may be missing from its hits";

/// `archive_search` of one turn: the garden, the journal and the archive it asks, and the
/// session the turn runs in.
pub struct ArchiveSearch {
    journal: PathBuf,
    current: u64,
    garden: Option<garden::Client>,
    archive: Option<archive::Client>,
}

/// The filters of one call.
struct Query<'a> {
    text: &'a str,
    source: Option<&'a str>,
    who: Option<&'a str>,
    kind: Option<&'a str>,
    since: Option<i64>,
    until: Option<i64>,
    limit: u64,
}

impl ArchiveSearch {
    /// The tool of a turn of session `current` over the journal file `journal`, the garden of
    /// `garden` and the archive of `archive`, each `None` when not configured.
    pub fn new(
        journal: PathBuf,
        current: u64,
        garden: Option<garden::Client>,
        archive: Option<archive::Client>,
    ) -> Self {
        Self {
            journal,
            current,
            garden,
            archive,
        }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let text = args.str("query")?.trim();
        if text.is_empty() {
            return Err("query must not be empty".into());
        }
        let query = Query {
            text,
            source: args.opt_str("source")?,
            who: args.opt_str("who")?,
            kind: args.opt_str("kind")?,
            since: args.opt_utc("since", false)?,
            until: args.opt_utc("until", true)?,
            limit: args
                .opt_i64("limit")?
                .unwrap_or(MAX_LIMIT)
                .clamp(1, MAX_LIMIT)
                .cast_unsigned(),
        };
        let (facts, garden_error) = match self.garden_tier(&query) {
            Ok(facts) => (facts, None),
            Err(why) => (Vec::new(), Some(why)),
        };
        let journal = Journal::open(&self.journal).map_err(|e| e.to_string())?;
        let history = self
            .history_tier(&journal, &query)
            .map_err(|e| format!("history search failed: {e}"))?;
        let mut fields = vec![("success", "true".to_owned())];
        if let Some(why) = &garden_error {
            fields.push(("garden_error", json_str(why)));
        }
        fields.push(("garden", json_array(facts.iter().map(fact))));
        fields.push(("history", json_array(history.iter().map(history_hit))));
        if !facts.is_empty() || !history.is_empty() {
            fields.push(("note", json_str(NOT_SEARCHED)));
            return Ok(Output::ok(json_object(fields)));
        }
        let (hits, synced) = self.archive_tier(&journal, &query).map_err(|why| {
            let garden = garden_error.map_or_else(String::new, |why| format!("; garden: {why}"));
            format!(
                "the archive was not searched: {why}; the garden and the history found nothing{garden}"
            )
        })?;
        let (seeds, lift_error) = self.lift(&hits);
        let shown = hits
            .iter()
            .zip(&seeds)
            .map(|(hit, seed)| archive_hit(hit, seed.as_deref()));
        fields.push(("archive", json_array(shown)));
        if let Some(line) = lift_error {
            fields.push(("lift_error", json_str(&line)));
        }
        if !synced {
            fields.push(("archive_sync", json_str(NOT_SYNCED)));
        }
        Ok(Output::ok(json_object(fields)))
    }

    /// The garden's facts and seedbed records for the query. A miss here is an ordinary garden
    /// miss, so the search queues it as any other.
    fn garden_tier(&self, query: &Query<'_>) -> Result<Vec<c7::Fact>, String> {
        let garden = self.garden.as_ref().ok_or(NO_GARDEN)?;
        let request = c7::SearchRequest {
            q: query.text.to_owned(),
            k: Some(query.limit),
            as_of: None,
            private: Some(true),
            no_miss: None,
        };
        let found = garden.search(&request).map_err(|e| garden_failure(&e))?;
        Ok(found.facts)
    }

    /// The history hits for the query within its time filters, the current session's live
    /// messages left out; forgotten messages are never among them.
    fn history_tier(
        &self,
        journal: &Journal,
        query: &Query<'_>,
    ) -> Result<Vec<Hit>, crate::journal::Error> {
        let hits = journal.search(&Search {
            query: query.text,
            roles: &[],
            session: None,
            since: query.since,
            until: query.until,
            sort: Sort::Rank,
            limit: HISTORY_SCAN,
        })?;
        let mut kept = Vec::new();
        for hit in hits {
            if kept.len() as u64 >= query.limit {
                break;
            }
            if hit.session == self.current && journal.placement(hit.n)?.is_none_or(|(_, live)| live)
            {
                continue;
            }
            kept.push(hit);
        }
        Ok(kept)
    }

    /// The archive hits the answer carries, the ones a lift into the seedbed takes, and whether
    /// the archive pulled the dialogue before it searched.
    fn archive_tier(
        &self,
        journal: &Journal,
        query: &Query<'_>,
    ) -> Result<(Vec<c10::Hit>, bool), String> {
        let archive = self.archive.as_ref().ok_or(NO_ARCHIVE)?;
        let request = c10::SearchRequest {
            q: query.text.to_owned(),
            source: query.source.map(str::to_owned),
            who: query.who.map(str::to_owned),
            kind: query.kind.map(str::to_owned),
            since: query.since.map(unix),
            until: query.until.map(unix),
            limit: Some(query.limit),
        };
        let found = archive.search(&request).map_err(|e| archive_failure(&e))?;
        Ok((unforgotten(journal, found.hits)?, found.synced))
    }

    /// The seedbed ref of each of `hits` lifted, in order, and the line naming how many were not
    /// and why. A failed call leaves it and the hits after it unlifted; a refused hit only itself.
    fn lift(&self, hits: &[c10::Hit]) -> (Vec<Option<String>>, Option<String>) {
        let mut seeds = vec![None; hits.len()];
        let mut reasons = Vec::new();
        match &self.garden {
            None => reasons.push(NO_GARDEN.to_owned()),
            Some(garden) => {
                for (seed, hit) in seeds.iter_mut().zip(hits) {
                    match lift_one(garden, hit) {
                        Ok(Ok(record)) => *seed = Some(record),
                        Ok(Err(why)) => reasons.push(format!("{} {why}", hit.id)),
                        Err(why) => {
                            reasons.push(why);
                            break;
                        }
                    }
                }
            }
        }
        let failed = seeds.iter().filter(|seed| seed.is_none()).count();
        let line = (failed > 0).then(|| {
            format!(
                "{failed} of {} archive hits were not lifted into the seedbed: {}",
                hits.len(),
                reasons.join("; ")
            )
        });
        (seeds, line)
    }
}

/// The seedbed ref of `hit` proposed as a raw record, or why the garden refused it; the outer
/// error when the call itself failed.
fn lift_one(garden: &garden::Client, hit: &c10::Hit) -> Result<Result<String, String>, String> {
    let Some(text) = &hit.text else {
        return Ok(Err("has no text in the archive's answer".into()));
    };
    let request = c7::ProposeRequest {
        fact: text.clone(),
        entity: None,
        source_ref: format!("archive:{}", hit.id),
        valid_from: None,
        kind: None,
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(c7::SourceKind::Raw),
        observed_at: hit
            .entries
            .iter()
            .filter_map(|e| e.at)
            .min_by(f64::total_cmp),
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    };
    let answer = garden.propose(&request).map_err(|e| garden_failure(&e))?;
    Ok(match answer.status {
        c7::ProposalStatus::Seeded => Ok(answer.proposal_id),
        c7::ProposalStatus::Rejected => Err(answer.reason.map_or_else(
            || "was refused by the garden".to_owned(),
            |reason| format!("was refused by the garden: {}", reason.as_str()),
        )),
        status => Err(format!(
            "got the unexpected garden status {}",
            status.as_str()
        )),
    })
}

impl super::Tool for ArchiveSearch {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: ARCHIVE_SEARCH.into(),
            description: SEARCH_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Words to look for in every tier."
                    },
                    "source": {
                        "type": "string",
                        "description": "Archive only: items of this source, dialogue or a connector's name."
                    },
                    "who": {
                        "type": "string",
                        "description": "Archive only: items with this author (user or assistant in the dialogue)."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Archive only: items of this media type."
                    },
                    "since": {
                        "type": "string",
                        "description": "Only history messages and archive items not before this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "until": {
                        "type": "string",
                        "description": "Only history messages and archive items not after this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Most hits of each tier (default 10)."
                    }
                }),
                &["query"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

/// `archive_read`: the original of one archive item.
pub struct ArchiveRead {
    archive: Option<archive::Client>,
}

impl ArchiveRead {
    /// The tool over the archive of `archive`, `None` when not configured.
    pub fn new(archive: Option<archive::Client>) -> Self {
        Self { archive }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let id = args.str("id")?;
        let archive = self
            .archive
            .as_ref()
            .ok_or_else(|| format!("the archive was not read: {NO_ARCHIVE}"))?;
        let item = archive
            .read(&c10::ReadRequest { id: id.to_owned() })
            .map_err(|e| format!("the archive was not read: {}", archive_failure(&e)))?;
        let bytes = decode(&item.bytes_b64)
            .ok_or("the archive was not read: malformed archive answer: bytes_b64 is not base64")?;
        let mut fields = vec![
            ("success", "true".to_owned()),
            ("id", json_str(id)),
            ("kind", json_str(&item.kind)),
            ("len", item.len.to_string()),
            ("entries", json_array(item.entries.iter().map(entry))),
        ];
        match String::from_utf8(bytes) {
            Ok(text) if is_text(&item.kind) => match text.char_indices().nth(MAX_READ_CHARS) {
                Some((cut, _)) => {
                    fields.push(("text", json_str(&text[..cut])));
                    fields.push(("text_truncated", "true".into()));
                }
                None => fields.push(("text", json_str(&text))),
            },
            _ => fields.push((
                "note",
                json_str("the original is not text; only its length and media type are shown"),
            )),
        }
        Ok(Output::ok(json_object(fields)))
    }
}

impl super::Tool for ArchiveRead {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: ARCHIVE_READ.into(),
            description: READ_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "id": {
                        "type": "string",
                        "description": "The id of an archive hit."
                    }
                }),
                &["id"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

/// `hits` without their forgotten dialogue entries, a hit left with none dropped.
fn unforgotten(journal: &Journal, hits: Vec<c10::Hit>) -> Result<Vec<c10::Hit>, String> {
    let mut kept = Vec::with_capacity(hits.len());
    for mut hit in hits {
        let mut entries = Vec::with_capacity(hit.entries.len());
        for entry in hit.entries {
            if !forgotten(journal, &entry)? {
                entries.push(entry);
            }
        }
        if !entries.is_empty() {
            hit.entries = entries;
            kept.push(hit);
        }
    }
    Ok(kept)
}

/// Whether `entry` is a dialogue entry whose journal message is forgotten.
fn forgotten(journal: &Journal, entry: &c10::Entry) -> Result<bool, String> {
    if entry.source != DIALOGUE {
        return Ok(false);
    }
    let (key, session, seq) = dialogue_ref(&entry.external_id).ok_or_else(|| {
        format!(
            "archive dialogue entry {} is not <session_key>/<session>/<seq>",
            entry.external_id
        )
    })?;
    journal
        .forgotten(key, session, seq)
        .map_err(|e| format!("history check failed: {e}"))
}

/// The session key, session id and seq of a dialogue `external_id`; the key may hold `/`.
fn dialogue_ref(external_id: &str) -> Option<(&str, u64, u64)> {
    let (rest, seq) = external_id.rsplit_once('/')?;
    let (key, session) = rest.rsplit_once('/')?;
    let number = |text: &str| {
        text.parse::<u64>()
            .ok()
            .filter(|_| text.bytes().all(|b| b.is_ascii_digit()))
    };
    Some((key, number(session)?, number(seq)?)).filter(|(key, ..)| !key.is_empty())
}

#[allow(clippy::cast_precision_loss, reason = "Unix times stay far below 2^52")]
fn unix(at: i64) -> f64 {
    at as f64
}

/// A garden fact or seedbed record as the answer shows it.
fn fact(fact: &c7::Fact) -> String {
    let mut fields = Vec::new();
    if let Some(id) = &fact.fact_id {
        fields.push(("ref", json_str(id)));
    }
    fields.push(("fact", json_str(&fact.fact)));
    fields.push(("entity", json_str(&fact.entity)));
    fields.push(("when", json_str(&rfc3339(to_u64(fact.valid_from)))));
    fields.push(("source_ref", json_str(&fact.source_ref)));
    json_object(fields)
}

/// An archive hit as the answer shows it, with the ref of its seedbed record when lifted.
fn archive_hit(hit: &c10::Hit, seed: Option<&str>) -> String {
    let mut fields = vec![("id", json_str(&hit.id))];
    if let Some(seed) = seed {
        fields.push(("seed", json_str(seed)));
    }
    fields.push(("entries", json_array(hit.entries.iter().map(entry))));
    fields.push(("snippet", json_str(&hit.snippet)));
    if let Some(text) = &hit.text {
        fields.push(("text", json_str(text)));
    }
    if hit.text_truncated == Some(true) {
        fields.push(("text_truncated", "true".into()));
    }
    json_object(fields)
}

/// An archive entry's metadata as the answer shows it.
fn entry(entry: &c10::Entry) -> String {
    let mut fields = vec![
        ("source", json_str(&entry.source)),
        ("external_id", json_str(&entry.external_id)),
    ];
    if let Some(who) = &entry.who {
        fields.push(("who", json_str(who)));
    }
    if let Some(at) = entry.at {
        fields.push(("when", json_str(&rfc3339(to_u64(at)))));
    }
    fields.push(("kind", json_str(&entry.kind)));
    json_object(fields)
}

/// The line of a failed archive call.
fn archive_failure(e: &archive::Error) -> String {
    match e {
        archive::Error::Unreachable(_) => format!("the archive is not reachable ({e})"),
        archive::Error::Rejected { .. } => format!("the {e}"),
    }
}

/// Whether media type `kind` is text: `text/*`, JSON or XML.
fn is_text(kind: &str) -> bool {
    let essence = kind
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence.starts_with("text/")
        || matches!(essence.as_str(), "application/json" | "application/xml")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
}

/// The bytes of standard base64 with padding (RFC 4648 section 4), the C10 `bytes_b64`.
fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let groups = text.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    for (i, group) in text.chunks(4).enumerate() {
        let pad = match group {
            [.., b'=', b'='] => 2,
            [.., b'='] => 1,
            _ => 0,
        };
        if pad > 0 && i + 1 != groups {
            return None;
        }
        let mut acc = 0u32;
        for &c in &group[..4 - pad] {
            acc = acc << 6 | u32::from(sextet(c)?);
        }
        acc <<= 6 * pad;
        let [_, b0, b1, b2] = acc.to_be_bytes();
        out.extend_from_slice(&[b0, b1, b2][..3 - pad]);
    }
    Some(out)
}

fn sextet(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}
