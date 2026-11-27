//! The places bed (ADR-0080): places as garden entities with typed attributes, each with its
//! own `checked_at`, `source`, `confidence` and the TTL of its `attr_policy` row, the owner's
//! verdicts and the food profile. Every write is one unit of work of the store; a refused write
//! is a [`Refusal`] and writes nothing. [`Bed::groom`] is the recheck pass of stale attributes
//! through a [`Recheck`] source (ADR-0080 section 6).

use std::fmt;

use rusqlite::{Connection, OptionalExtension, params};

use crate::edge::{Edge, EdgeConfig, EdgeError, Named, free_id, one_edit_alike};
use crate::model::{Attrs, Entity, EntityId, EntityKind, PlaceAttrs, UnixTime};
use crate::places_search::haversine;
use crate::second::SourceError;
use crate::secret::may_carry_credential;
use crate::store::{Store, StoreError};
use crate::translit;
use crate::wire::c7;

/// The bed type of places; its entities are of kind [`EntityKind::Place`].
pub const PLACE: &str = "place";

const DAY: i64 = 86_400;

/// The distance in metres within which a place of a similar name is the same place.
pub const SAME_PLACE_M: f64 = 75.0;

/// The wait after the first failed or unanswered recheck of a key; it doubles with each try.
pub const RECHECK_FIRST_RETRY: UnixTime = 3_600;

/// The longest wait between two rechecks of a key.
pub const RECHECK_MAX_RETRY: UnixTime = 7 * DAY;

/// The keys of `bed_profile` with the forms of their values.
const PROFILE_FORMS: [(&str, Form); 5] = [
    ("home", Form::Point),
    ("min_rating", Form::Number),
    ("budget_for_two", Form::Range),
    ("currency", Form::Text),
    ("ban", Form::Text),
];

/// The status of a place in the bed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Found but not yet confirmed by the owner.
    Candidate,
    /// On the owner's list.
    Active,
    /// Closed for good; kept so it is not suggested again.
    Gone,
}

impl Status {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Gone => "gone",
        }
    }
}

/// The form of a value (ADR-0080 section 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// A decimal such as `4.5`.
    Number,
    /// An integer from 0.
    Count,
    /// `A` or `A-B`.
    Range,
    /// Terms separated by commas.
    List,
    /// Opening hours; any text is stored, [`hours`] reads the subset of section 2.
    Hours,
    /// `open`, `temporarily_closed` or `permanently_closed`.
    Status,
    /// `lat, lon` in degrees.
    Point,
    /// `node/<n>`, `way/<n>` or `relation/<n>`.
    OsmId,
    /// An `https://` URL.
    Url,
    /// Free text.
    Text,
    /// The key is written to `bed_verdict`, never to `bed_attr`.
    Verdict,
}

impl Form {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Count => "count",
            Self::Range => "range",
            Self::List => "list",
            Self::Hours => "hours",
            Self::Status => "status",
            Self::Point => "point",
            Self::OsmId => "osm_id",
            Self::Url => "url",
            Self::Text => "text",
            Self::Verdict => "verdict",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::Number,
            Self::Count,
            Self::Range,
            Self::List,
            Self::Hours,
            Self::Status,
            Self::Point,
            Self::OsmId,
            Self::Url,
            Self::Text,
            Self::Verdict,
        ]
        .into_iter()
        .find(|f| f.as_str() == s)
    }

    /// Free forms can hold any text, so their values go through the bed's secret screen.
    fn is_free(self) -> bool {
        matches!(self, Self::List | Self::Hours | Self::Url | Self::Text)
    }
}

/// Why a bed write was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// The type has no `attr_policy` row of the key, or the key is not one of `bed_profile`.
    UnknownKey(String),
    /// The value does not fit the form of its key.
    BadValue {
        /// The key.
        key: String,
        /// Its form.
        form: Form,
    },
    /// A blank `source`.
    EmptySource,
    /// A blank `source_ref`.
    EmptySourceRef,
    /// An observation without `checked_at`.
    MissingCheckedAt,
    /// `checked_at` plus the key's TTL leaves the range of the clock.
    TimeOutOfRange,
    /// A confidence outside `[0, 1]`.
    BadConfidence,
    /// A blank place name.
    EmptyName,
    /// A text that may carry a credential (`secret::may_carry_credential`).
    SecretValue,
    /// No place of the bed has this id.
    UnknownPlace(EntityId),
    /// Two or more places of the bed have this name: the write names one by id.
    AmbiguousPlace(String),
    /// The entity is not a place.
    NotAPlace(EntityId),
    /// The key's form is `verdict`: it is written with [`Bed::verdict`].
    VerdictKey,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(k) => write!(f, "unknown key {k:?}"),
            Self::BadValue { key, form } => {
                write!(f, "the value of {key:?} is not a {}", form.as_str())
            }
            Self::EmptySource => f.write_str("empty source"),
            Self::EmptySourceRef => f.write_str("empty source_ref"),
            Self::MissingCheckedAt => f.write_str("missing checked_at"),
            Self::TimeOutOfRange => f.write_str("checked_at out of range"),
            Self::BadConfidence => f.write_str("confidence outside [0, 1]"),
            Self::EmptyName => f.write_str("empty name"),
            Self::SecretValue => f.write_str("the text may carry a secret"),
            Self::UnknownPlace(id) => write!(f, "no place {id:?} in the bed"),
            Self::AmbiguousPlace(name) => write!(f, "two or more places are named {name:?}"),
            Self::NotAPlace(id) => write!(f, "entity {id:?} is not a place"),
            Self::VerdictKey => f.write_str("a verdict is written as a verdict"),
        }
    }
}

/// A failure of a bed write.
#[derive(Debug)]
pub enum BedError {
    /// The write was refused and wrote nothing.
    Refused(Refusal),
    /// The store failed.
    Store(StoreError),
    /// Naming the place through the edge failed.
    Edge(EdgeError),
}

impl fmt::Display for BedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(r) => write!(f, "refused: {r}"),
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Edge(e) => write!(f, "edge: {e}"),
        }
    }
}

impl std::error::Error for BedError {}

impl BedError {
    /// A short name of the kind of failure, free of any stored or proposed text.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Refused(_) => "refused",
            Self::Store(e) => e.class(),
            Self::Edge(e) => e.class(),
        }
    }
}

impl From<Refusal> for BedError {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

impl From<StoreError> for BedError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for BedError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::Sqlite(e))
    }
}

impl From<EdgeError> for BedError {
    fn from(e: EdgeError) -> Self {
        match e {
            EdgeError::Store(e) => Self::Store(e),
            e => Self::Edge(e),
        }
    }
}

/// How a place write names its place.
#[derive(Clone, Copy, Debug)]
pub enum PlaceRef<'a> {
    /// A stored place entity (a merged one stands for its survivor).
    Id(&'a EntityId),
    /// A name, matched against the bed's places by the dedup rule of ADR-0080 section 6 with
    /// the write's `osm_id` and `location`; without a match, the one place entity `Edge::name`
    /// finds that has no bed row is adopted, otherwise a new entity is written.
    Name(&'a str),
}

/// A place to create or find in the bed.
#[derive(Clone, Copy, Debug)]
pub struct PlaceWrite<'a> {
    /// The place.
    pub place: PlaceRef<'a>,
    /// Its status. A found place keeps its own on `candidate` and takes `active` or `gone`,
    /// with `status_at` the time of the write, so a write never demotes a place to a candidate.
    pub status: Status,
    /// Where it was found; kept from the write that created its bed row.
    pub source: &'a str,
    /// The `osm_id` the write observes; one that does not fit its form counts as none.
    pub osm_id: Option<&'a str>,
    /// The `location` the write observes; one that does not fit its form counts as none.
    pub location: Option<&'a str>,
}

/// What a place write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placed {
    /// A new entity with its bed row.
    Created(EntityId),
    /// A bed row for an entity that had none.
    Adopted(EntityId),
    /// The place was in the bed already.
    Found(EntityId),
}

impl Placed {
    /// The place's id.
    pub fn id(&self) -> &EntityId {
        match self {
            Self::Created(id) | Self::Adopted(id) | Self::Found(id) => id,
        }
    }
}

/// One observation of an attribute.
#[derive(Clone, Copy, Debug)]
pub struct Observation<'a> {
    /// A key of the type's `attr_policy`.
    pub key: &'a str,
    /// The value in the key's form.
    pub value: &'a str,
    /// When it was last checked against its source; required.
    pub checked_at: Option<UnixTime>,
    /// Where it was checked.
    pub source: &'a str,
    /// How sure the source is, in `[0, 1]`.
    pub confidence: f64,
}

/// What an observation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// The first value of the key.
    Written,
    /// It replaced the current value, which moved into the history.
    Replaced,
    /// Older than the current value, it went into the history only.
    History,
    /// The same observation was stored already.
    Unchanged,
}

/// One verdict of the owner about a place.
#[derive(Clone, Copy, Debug)]
pub struct VerdictWrite<'a> {
    /// -1, 0 or 1.
    pub verdict: i64,
    /// What the owner said with it.
    pub note: Option<&'a str>,
    /// When the owner said it.
    pub said_at: UnixTime,
    /// The message that carries it.
    pub source_ref: &'a str,
    /// A place of the bed the owner said this one beats.
    pub better_than: Option<&'a EntityId>,
}

/// What a verdict write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Said {
    /// A new row of `bed_verdict` with this `seq`.
    Written(i64),
    /// The place already has the row `seq` of this `source_ref`.
    Repeated(i64),
}

/// What a profile write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profiled {
    /// The row was written, replacing the key's row unless the key is `ban`.
    Written,
    /// The same row was stored already.
    Unchanged,
}

/// A place handed to a [`Recheck`] source with its queued keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The place.
    pub place: EntityId,
    /// Its current `osm_id`.
    pub osm_id: String,
    /// Its stale keys due for a recheck, in key order.
    pub keys: Vec<String>,
}

/// A value a [`Recheck`] source observed.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    /// A key of the place's type.
    pub key: String,
    /// The value in the key's form.
    pub value: String,
    /// When the source last checked it.
    pub checked_at: UnixTime,
    /// The source.
    pub source: String,
    /// How sure the source is, in `[0, 1]`.
    pub confidence: f64,
}

/// A closing date a [`Recheck`] source found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closing {
    /// When the place closed for good.
    pub at: UnixTime,
    /// When the source last checked it.
    pub checked_at: UnixTime,
    /// The source.
    pub source: String,
}

/// What a [`Recheck`] source answers about one place it was handed.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The place, one of those handed.
    pub place: EntityId,
    /// The values observed, of queued keys or others.
    pub seen: Vec<Seen>,
    /// The closing date, when the source has one.
    pub closed: Option<Closing>,
}

/// Where the recheck pass looks up the stale attributes of places by their `osm_id`.
pub trait Recheck {
    /// The answers about `places` at `now`; a place or key the source knows nothing about is
    /// left out.
    ///
    /// # Errors
    ///
    /// The source could not be asked or its answer could not be read.
    fn recheck(&self, places: &[Asked], now: UnixTime) -> Result<Vec<Answer>, SourceError>;
}

/// What one recheck pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Groomed {
    /// The source's failure; every handed key backed off.
    pub failed: Option<SourceError>,
    /// Observations and closings of the answer the bed refused; their keys backed off.
    pub refused: usize,
}

/// When the next recheck pass has work: the first second after the earliest `due_at` of an
/// attribute not yet queued, or the earliest `next_attempt` of a queued place with an
/// `osm_id`; none when there is neither.
///
/// # Errors
///
/// The store failed.
pub fn next_groom(c: &Connection) -> Result<Option<UnixTime>, StoreError> {
    Ok(c.query_row(
        "SELECT MIN(t) FROM (
             SELECT a.due_at + 1 AS t FROM bed_attr a JOIN bed_entity e ON e.entity = a.entity
             WHERE a.due_at IS NOT NULL AND e.type = ?1 AND e.status <> 'gone'
                 AND NOT EXISTS (SELECT 1 FROM bed_recheck r
                     WHERE r.entity = a.entity AND r.key = a.key)
             UNION ALL
             SELECT r.next_attempt FROM bed_recheck r
             WHERE EXISTS (SELECT 1 FROM bed_attr o
                 WHERE o.entity = r.entity AND o.key = 'osm_id')
         )",
        [PLACE],
        |r| r.get(0),
    )?)
}

/// The bed's writes over a store.
pub struct Bed<'a> {
    store: &'a mut Store,
}

/// A row of `bed_attr` or `bed_attr_history` as compared between observations.
#[derive(PartialEq)]
struct Reading {
    value: String,
    checked_at: UnixTime,
    source: String,
    confidence: f64,
}

impl<'a> Bed<'a> {
    /// The bed over `store`.
    pub fn new(store: &'a mut Store) -> Self {
        Self { store }
    }

    /// Creates or finds a place with its `entities` and `bed_entity` rows at `now`.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a blank or secret-looking name or source, a name of two or more bed
    /// places, an unknown id or one of an entity that is not a place; failures of the store.
    pub fn place(&mut self, w: &PlaceWrite<'_>, now: UnixTime) -> Result<Placed, BedError> {
        let source = provenance(w.source, Refusal::EmptySource)?;
        self.store.atomically(|s| {
            let (id, created, pairs) = match w.place {
                PlaceRef::Id(id) => {
                    let e = s
                        .survivor(id)?
                        .ok_or_else(|| Refusal::UnknownPlace(id.clone()))?;
                    if e.kind != EntityKind::Place {
                        return Err(Refusal::NotAPlace(e.id).into());
                    }
                    (e.id, false, Vec::new())
                }
                PlaceRef::Name(name) => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(Refusal::EmptyName.into());
                    }
                    if may_carry_credential(name) {
                        return Err(Refusal::SecretValue.into());
                    }
                    let osm_id = w
                        .osm_id
                        .map(str::trim)
                        .filter(|v| fit("osm_id", Form::OsmId, v).is_ok());
                    let at = w.location.and_then(|v| point(v.trim()));
                    match dedup(s, name, osm_id, at)? {
                        Dedup::Same(id) => (id, false, Vec::new()),
                        Dedup::New(pairs) => match unplaced_namesake(s, name)? {
                            Some(id) => (id, false, pairs),
                            None => (create_place(s, name)?, true, pairs),
                        },
                    }
                }
            };
            let status: Option<String> = s
                .conn()
                .query_row(
                    "SELECT status FROM bed_entity WHERE entity = ?1",
                    [&id.0],
                    |r| r.get(0),
                )
                .optional()?;
            match status {
                None => {
                    s.conn().execute(
                        "INSERT INTO bed_entity (entity, type, status, source, created_at,
                             status_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![id.0, PLACE, w.status.as_str(), source, now],
                    )?;
                    pairs_of(s.conn(), &id, &pairs, now)?;
                    Ok(if created {
                        Placed::Created(id)
                    } else {
                        Placed::Adopted(id)
                    })
                }
                Some(st) => {
                    if w.status != Status::Candidate && st != w.status.as_str() {
                        s.conn().execute(
                            "UPDATE bed_entity SET status = ?2, status_at = ?3 WHERE entity = ?1",
                            params![id.0, w.status.as_str(), now],
                        )?;
                    }
                    Ok(Placed::Found(id))
                }
            }
        })
    }

    /// Writes an observation of an attribute of `place` at `now`: the first value of its key,
    /// a newer one replacing the current row (moved into `bed_attr_history` with
    /// `replaced_at`), or an older one into the history only. `num` and `due_at` follow the
    /// key's form and TTL; a current `location` sets the place's `lat` and `lon` and a current
    /// `location` or `osm_id` the entity's place attributes.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a missing `checked_at`, a blank or secret-looking source, a confidence
    /// outside `[0, 1]`, an unknown place or key, a `verdict` key, a value that does not fit
    /// its form or a free-form value that may carry a secret; failures of the store.
    pub fn observe(
        &mut self,
        place: &EntityId,
        o: &Observation<'_>,
        now: UnixTime,
    ) -> Result<Observed, BedError> {
        let checked_at = o.checked_at.ok_or(Refusal::MissingCheckedAt)?;
        let source = provenance(o.source, Refusal::EmptySource)?;
        if !(0.0..=1.0).contains(&o.confidence) {
            return Err(Refusal::BadConfidence.into());
        }
        let value = o.value.trim();
        self.store.atomically(|s| {
            let ty = bed_type(s.conn(), place)?;
            let (form, ttl_days) = policy(s.conn(), &ty, o.key)?;
            if form == Form::Verdict {
                return Err(Refusal::VerdictKey.into());
            }
            let num = fit(o.key, form, value)?;
            let due_at = ttl_days
                .map(|d| {
                    d.checked_mul(DAY)
                        .and_then(|t| checked_at.checked_add(t))
                        .ok_or(Refusal::TimeOutOfRange)
                })
                .transpose()?;
            let new = Reading {
                value: value.to_owned(),
                checked_at,
                source: source.to_owned(),
                confidence: o.confidence,
            };
            let current = current(s.conn(), place, o.key)?;
            if current.as_ref() == Some(&new) {
                return Ok(Observed::Unchanged);
            }
            if let Some(cur) = &current
                && checked_at < cur.checked_at
            {
                if in_history(s.conn(), place, o.key, &new)? {
                    return Ok(Observed::Unchanged);
                }
                into_history(s.conn(), place, o.key, &new, now)?;
                return Ok(Observed::History);
            }
            if let Some(cur) = &current {
                into_history(s.conn(), place, o.key, cur, now)?;
            }
            s.conn().execute(
                "INSERT INTO bed_attr (entity, type, key, value, num, checked_at, source,
                     confidence, due_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(entity, key) DO UPDATE SET value = ?4, num = ?5, checked_at = ?6,
                     source = ?7, confidence = ?8, due_at = ?9",
                params![
                    place.0,
                    ty,
                    o.key,
                    new.value,
                    num,
                    checked_at,
                    new.source,
                    new.confidence,
                    due_at
                ],
            )?;
            follow(s, place, o.key, value)?;
            Ok(if current.is_some() {
                Observed::Replaced
            } else {
                Observed::Written
            })
        })
    }

    /// The recheck pass at `now` (ADR-0080 section 6): queues in `bed_recheck` every attribute
    /// of a place not `gone` with `due_at` before now and drops the rows of every other one,
    /// then hands the places with an `osm_id` of the oldest rows due, at most `batch`, with
    /// their due keys to `source`, outside any unit of work. Each place's answer is one unit of
    /// work: an observation is [`Bed::observe`] and clears its row when it leaves the attribute
    /// current; a closing date before now writes `operating_status` `permanently_closed`, sets
    /// the place `gone` and clears its rows. Every handed key still queued after its answer, and
    /// every one of a failed call, raises `tries` and waits [`RECHECK_FIRST_RETRY`] doubling up
    /// to [`RECHECK_MAX_RETRY`]. An answer about a place not handed is ignored.
    ///
    /// # Errors
    ///
    /// Failures of the store; a place's unit that failed writes nothing of its answer.
    pub fn groom(
        &mut self,
        source: &dyn Recheck,
        batch: usize,
        now: UnixTime,
    ) -> Result<Groomed, BedError> {
        let asked = self.store.atomically(|s| {
            let c = s.conn();
            c.execute(
                "DELETE FROM bed_recheck WHERE NOT EXISTS (
                     SELECT 1 FROM bed_attr a JOIN bed_entity e ON e.entity = a.entity
                     WHERE a.entity = bed_recheck.entity AND a.key = bed_recheck.key
                         AND a.due_at < ?1 AND e.status <> 'gone')",
                [now],
            )?;
            c.execute(
                "INSERT OR IGNORE INTO bed_recheck (entity, key, queued_at, next_attempt)
                 SELECT a.entity, a.key, ?1, ?1 FROM bed_attr a
                 JOIN bed_entity e ON e.entity = a.entity
                 WHERE a.due_at < ?1 AND e.type = ?2 AND e.status <> 'gone'",
                params![now, PLACE],
            )?;
            due_places(c, batch, now)
        })?;
        let mut groomed = Groomed::default();
        if asked.is_empty() {
            return Ok(groomed);
        }
        let answers = match source.recheck(&asked, now) {
            Ok(answers) => answers,
            Err(e) => {
                groomed.failed = Some(e);
                self.store
                    .atomically(|s| asked.iter().try_for_each(|a| back_off(s.conn(), a, now)))?;
                return Ok(groomed);
            }
        };
        for a in &asked {
            let mine = answers.iter().filter(|x| x.place == a.place);
            groomed.refused += self.store.atomically(|s| {
                let mut refused = 0;
                for answer in mine {
                    for seen in &answer.seen {
                        refused += usize::from(!settle(s, &a.place, seen, now)?);
                    }
                    if let Some(closing) = answer.closed.as_ref().filter(|c| c.at < now) {
                        refused += usize::from(!close(s, &a.place, closing, now)?);
                    }
                }
                back_off(s.conn(), a, now)?;
                Ok::<_, BedError>(refused)
            })?;
        }
        Ok(groomed)
    }

    /// Appends a verdict about `place` written at `now`; a second write of its `source_ref`
    /// writes nothing.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a blank or secret-looking `source_ref`, a verdict outside -1..=1, a
    /// blank note or one that may carry a secret, an unknown place or `better_than`, a type
    /// without a `verdict` policy row; failures of the store.
    pub fn verdict(
        &mut self,
        place: &EntityId,
        v: &VerdictWrite<'_>,
        now: UnixTime,
    ) -> Result<Said, BedError> {
        let source_ref = provenance(v.source_ref, Refusal::EmptySourceRef)?;
        let bad = || Refusal::BadValue {
            key: "verdict".into(),
            form: Form::Verdict,
        };
        if !(-1..=1).contains(&v.verdict) {
            return Err(bad().into());
        }
        let note = v.note.map(str::trim);
        if note.is_some_and(str::is_empty) {
            return Err(bad().into());
        }
        if note.is_some_and(may_carry_credential) {
            return Err(Refusal::SecretValue.into());
        }
        self.store.atomically(|s| {
            let ty = bed_type(s.conn(), place)?;
            if policy(s.conn(), &ty, "verdict")?.0 != Form::Verdict {
                return Err(Refusal::UnknownKey("verdict".into()).into());
            }
            if let Some(b) = v.better_than {
                bed_type(s.conn(), b)?;
            }
            let seen: Option<i64> = s
                .conn()
                .query_row(
                    "SELECT seq FROM bed_verdict WHERE entity = ?1 AND source_ref = ?2",
                    params![place.0, source_ref],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(seq) = seen {
                return Ok(Said::Repeated(seq));
            }
            s.conn().execute(
                "INSERT INTO bed_verdict (entity, verdict, note, said_at, source_ref, better_than,
                     written_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    place.0,
                    v.verdict,
                    note,
                    v.said_at,
                    source_ref,
                    v.better_than.map(|b| &b.0),
                    now
                ],
            )?;
            Ok(Said::Written(s.conn().last_insert_rowid()))
        })
    }

    /// Writes the profile row of `key` of type [`PLACE`], set by the owner at `set_at`: a key
    /// other than `ban` keeps one row, which the write replaces; `ban` keeps one row per value.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a key that is not one of `bed_profile`, a value that does not fit the
    /// key's form or a free-form one that may carry a secret, a blank or secret-looking
    /// `source_ref`; failures of the store.
    pub fn profile(
        &mut self,
        key: &str,
        value: &str,
        source_ref: &str,
        set_at: UnixTime,
    ) -> Result<Profiled, BedError> {
        let Some(&(_, form)) = PROFILE_FORMS.iter().find(|(k, _)| *k == key) else {
            return Err(Refusal::UnknownKey(key.into()).into());
        };
        let source_ref = provenance(source_ref, Refusal::EmptySourceRef)?;
        let value = value.trim();
        fit(key, form, value)?;
        self.store.atomically(|s| {
            let c = s.conn();
            if key == "ban" {
                let n = c.execute(
                    "INSERT INTO bed_profile (type, key, value, source_ref, set_at)
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(type, key, value) DO NOTHING",
                    params![PLACE, key, value, source_ref, set_at],
                )?;
                return Ok(if n == 0 {
                    Profiled::Unchanged
                } else {
                    Profiled::Written
                });
            }
            let same: bool = c.query_row(
                "SELECT EXISTS (SELECT 1 FROM bed_profile WHERE type = ?1 AND key = ?2
                     AND value = ?3 AND source_ref = ?4 AND set_at = ?5)",
                params![PLACE, key, value, source_ref, set_at],
                |r| r.get(0),
            )?;
            if same {
                return Ok(Profiled::Unchanged);
            }
            c.execute(
                "DELETE FROM bed_profile WHERE type = ?1 AND key = ?2",
                params![PLACE, key],
            )?;
            c.execute(
                "INSERT INTO bed_profile (type, key, value, source_ref, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![PLACE, key, value, source_ref, set_at],
            )?;
            Ok(Profiled::Written)
        })
    }
}

/// The places with an `osm_id` of the oldest `bed_recheck` rows due at `now`, at most `batch`,
/// each with its due keys.
fn due_places(c: &Connection, batch: usize, now: UnixTime) -> Result<Vec<Asked>, BedError> {
    let limit = i64::try_from(batch).unwrap_or(i64::MAX);
    let mut places = c.prepare(
        "SELECT r.entity, o.value FROM bed_recheck r
         JOIN bed_attr o ON o.entity = r.entity AND o.key = 'osm_id'
         WHERE r.next_attempt <= ?1
         GROUP BY r.entity, o.value ORDER BY MIN(r.queued_at), r.entity LIMIT ?2",
    )?;
    let mut keys = c.prepare(
        "SELECT key FROM bed_recheck WHERE entity = ?1 AND next_attempt <= ?2 ORDER BY key",
    )?;
    let found = places
        .query_map(params![now, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<(String, String)>, _>>()?;
    found
        .into_iter()
        .map(|(place, osm_id)| {
            let keys = keys
                .query_map(params![place, now], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(Asked {
                place: EntityId(place),
                osm_id,
                keys,
            })
        })
        .collect()
}

/// Writes `seen` about `place` and clears its `bed_recheck` row when the attribute is current
/// after it; false when the bed refuses it.
fn settle(s: &mut Store, place: &EntityId, seen: &Seen, now: UnixTime) -> Result<bool, BedError> {
    let o = Observation {
        key: &seen.key,
        value: &seen.value,
        checked_at: Some(seen.checked_at),
        source: &seen.source,
        confidence: seen.confidence,
    };
    match Bed::new(s).observe(place, &o, now) {
        Ok(_) => {}
        Err(BedError::Refused(_)) => return Ok(false),
        Err(e) => return Err(e),
    }
    s.conn().execute(
        "DELETE FROM bed_recheck WHERE entity = ?1 AND key = ?2 AND NOT EXISTS (
             SELECT 1 FROM bed_attr WHERE entity = ?1 AND key = ?2 AND due_at < ?3)",
        params![place.0, seen.key, now],
    )?;
    Ok(true)
}

/// Writes `operating_status` `permanently_closed` from `closing` and sets `place` `gone` with
/// its `bed_recheck` rows cleared, in one unit of work; false when the bed refuses it.
fn close(
    s: &mut Store,
    place: &EntityId,
    closing: &Closing,
    now: UnixTime,
) -> Result<bool, BedError> {
    let written = s.atomically(|s| {
        let o = Observation {
            key: "operating_status",
            value: "permanently_closed",
            checked_at: Some(closing.checked_at),
            source: &closing.source,
            confidence: 1.0,
        };
        Bed::new(s).observe(place, &o, now)?;
        let w = PlaceWrite {
            place: PlaceRef::Id(place),
            status: Status::Gone,
            source: &closing.source,
            osm_id: None,
            location: None,
        };
        Bed::new(s).place(&w, now)?;
        s.conn()
            .execute("DELETE FROM bed_recheck WHERE entity = ?1", [&place.0])?;
        Ok::<_, BedError>(())
    });
    match written {
        Ok(()) => Ok(true),
        Err(BedError::Refused(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Raises `tries` of the handed keys of `a` still queued and puts off their `next_attempt`.
fn back_off(c: &Connection, a: &Asked, now: UnixTime) -> Result<(), BedError> {
    let mut stmt = c.prepare(
        "UPDATE bed_recheck SET tries = tries + 1,
             next_attempt = ?3 + MIN(?4 << MIN(tries, 20), ?5)
         WHERE entity = ?1 AND key = ?2",
    )?;
    for key in &a.keys {
        stmt.execute(params![
            a.place.0,
            key,
            now,
            RECHECK_FIRST_RETRY,
            RECHECK_MAX_RETRY
        ])?;
    }
    Ok(())
}

/// `text` trimmed, refused with `blank` when empty and when it looks secret.
fn provenance(text: &str, blank: Refusal) -> Result<&str, Refusal> {
    let t = text.trim();
    if t.is_empty() {
        return Err(blank);
    }
    if may_carry_credential(t) {
        return Err(Refusal::SecretValue);
    }
    Ok(t)
}

/// What the dedup rule found for a write by name.
enum Dedup {
    /// The bed's place the write is.
    Same(EntityId),
    /// A new place, a possible duplicate of each of these places at its distance in metres.
    New(Vec<(EntityId, Option<f64>)>),
}

/// The dedup rule of ADR-0080 section 6 for a write of `name` with `osm_id` and the point
/// `at`: the place with that `osm_id`; else, with a point, the nearest place within
/// [`SAME_PLACE_M`] of a similar name, and with neither, the one place of an equal name or
/// alias; otherwise the places within [`SAME_PLACE_M`] of another name and the places of a
/// similar name where either side has no point.
fn dedup(
    s: &Store,
    name: &str,
    osm_id: Option<&str>,
    at: Option<(f64, f64)>,
) -> Result<Dedup, BedError> {
    if let Some(osm_id) = osm_id {
        if let Some(id) = by_osm_id(s.conn(), osm_id)? {
            return Ok(Dedup::Same(id));
        }
    } else if at.is_none()
        && let Some(id) = bedded_namesake(s, name)?
    {
        return Ok(Dedup::Same(id));
    }
    let equal: Vec<EntityId> = s.find_entities(name)?.into_iter().map(|e| e.id).collect();
    let want: Vec<char> = translit::key(name).chars().collect();
    let mut same: Option<(f64, EntityId)> = None;
    let mut pairs = Vec::new();
    for (id, lat_lon, names) in bed_places(s.conn())? {
        let distance = at.zip(lat_lon).map(|(a, b)| haversine(a, b));
        let near = distance.is_some_and(|d| d <= SAME_PLACE_M);
        if distance.is_some() && !near {
            continue;
        }
        let similar = equal.contains(&id)
            || names.iter().any(|n| {
                let key: Vec<char> = translit::key(n).chars().collect();
                !want.is_empty() && (key == want || one_edit_alike(&want, &key))
            });
        match (distance, similar) {
            (Some(d), true) => {
                if same.as_ref().is_none_or(|(best, _)| d < *best) {
                    same = Some((d, id));
                }
            }
            (Some(_), false) | (None, true) => pairs.push((id, distance)),
            (None, false) => {}
        }
    }
    Ok(match same {
        Some((_, id)) => Dedup::Same(id),
        None => Dedup::New(pairs),
    })
}

/// The first place of type [`PLACE`] by id whose current `osm_id` is `osm_id`.
fn by_osm_id(c: &Connection, osm_id: &str) -> Result<Option<EntityId>, BedError> {
    Ok(c.query_row(
        "SELECT a.entity FROM bed_attr a JOIN bed_entity e ON e.entity = a.entity
         WHERE e.type = ?1 AND a.key = 'osm_id' AND a.value = ?2 ORDER BY a.entity LIMIT 1",
        params![PLACE, osm_id],
        |r| r.get(0).map(EntityId),
    )
    .optional()?)
}

/// A place of the bed with its point and its name and aliases.
type BedPlace = (EntityId, Option<(f64, f64)>, Vec<String>);

/// The places of type [`PLACE`] by id.
fn bed_places(c: &Connection) -> Result<Vec<BedPlace>, BedError> {
    let mut stmt = c.prepare(
        "SELECT b.entity, b.lat, b.lon, e.name FROM bed_entity b
         JOIN entities e ON e.id = b.entity WHERE b.type = ?1 AND e.merged_into IS NULL
         UNION ALL
         SELECT b.entity, b.lat, b.lon, a.alias FROM bed_entity b
         JOIN entities e ON e.id = b.entity JOIN entity_aliases a ON a.entity = e.id
         WHERE b.type = ?1 AND e.merged_into IS NULL
         ORDER BY 1",
    )?;
    let rows = stmt.query_map([PLACE], |r| {
        let lat: Option<f64> = r.get(1)?;
        let lon: Option<f64> = r.get(2)?;
        Ok((r.get::<_, String>(0)?, lat.zip(lon), r.get::<_, String>(3)?))
    })?;
    let mut out: Vec<BedPlace> = Vec::new();
    for row in rows {
        let (id, lat_lon, name) = row?;
        match out.last_mut() {
            Some(last) if last.0.0 == id => last.2.push(name),
            _ => out.push((EntityId(id), lat_lon, vec![name])),
        }
    }
    Ok(out)
}

/// Writes one `bed_dup_pair` row of `place` with each of `others`, in canonical order.
fn pairs_of(
    c: &Connection,
    place: &EntityId,
    others: &[(EntityId, Option<f64>)],
    now: UnixTime,
) -> Result<(), BedError> {
    let mut stmt = c.prepare(
        "INSERT INTO bed_dup_pair (a, b, distance_m, found_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(a, b) DO NOTHING",
    )?;
    for (other, distance) in others {
        let (a, b) = if place.0 < other.0 {
            (place, other)
        } else {
            (other, place)
        };
        #[allow(clippy::cast_possible_truncation)]
        let metres = distance.map(|d| d.round() as i64);
        stmt.execute(params![a.0, b.0, metres, now])?;
    }
    Ok(())
}

/// The one place of the bed whose entity's name or alias equals `name`.
fn bedded_namesake(s: &Store, name: &str) -> Result<Option<EntityId>, BedError> {
    let mut found = Vec::new();
    for e in s.find_entities(name)? {
        if e.kind == EntityKind::Place && in_bed(s.conn(), &e.id)? {
            found.push(e.id);
        }
    }
    if found.len() > 1 {
        return Err(Refusal::AmbiguousPlace(name.to_owned()).into());
    }
    Ok(found.pop())
}

/// The one place entity `Edge::name` finds for `name` when it has no bed row.
fn unplaced_namesake(s: &mut Store, name: &str) -> Result<Option<EntityId>, BedError> {
    let naming = c7::ProposeRequest {
        fact: String::new(),
        entity: Some(name.to_owned()),
        source_ref: String::new(),
        valid_from: None,
        kind: Some(c7::EntityKind::Place),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: None,
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    };
    let Named::Known(e) = Edge::new(s, EdgeConfig::default()).name(&naming)? else {
        return Ok(None);
    };
    if e.kind != EntityKind::Place || in_bed(s.conn(), &e.id)? {
        return Ok(None);
    }
    Ok(Some(e.id))
}

fn create_place(s: &mut Store, name: &str) -> Result<EntityId, BedError> {
    let id = free_id(s, name)?;
    s.upsert_entity_unresolved(&Entity {
        id: id.clone(),
        kind: EntityKind::Place,
        name: name.to_owned(),
        aliases: Vec::new(),
        domain: None,
        attrs: Attrs::Place(PlaceAttrs {
            osm_id: None,
            lat_lon: None,
        }),
    })?;
    Ok(id)
}

fn in_bed(c: &Connection, id: &EntityId) -> Result<bool, BedError> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM bed_entity WHERE entity = ?1)",
        [&id.0],
        |r| r.get(0),
    )?)
}

/// The bed type of `place`.
fn bed_type(c: &Connection, place: &EntityId) -> Result<String, BedError> {
    c.query_row(
        "SELECT type FROM bed_entity WHERE entity = ?1",
        [&place.0],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Refusal::UnknownPlace(place.clone()).into())
}

/// The form and `ttl_days` of `key` of type `ty`.
fn policy(c: &Connection, ty: &str, key: &str) -> Result<(Form, Option<i64>), BedError> {
    let row: Option<(String, Option<i64>)> = c
        .query_row(
            "SELECT form, ttl_days FROM attr_policy WHERE type = ?1 AND key = ?2",
            params![ty, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (form, ttl) = row.ok_or_else(|| Refusal::UnknownKey(key.into()))?;
    let form = Form::parse(&form)
        .ok_or_else(|| StoreError::Corrupt(format!("attr_policy form {form:?}")))?;
    Ok((form, ttl))
}

/// `num` of `value` when it fits `form`: the number of a `number` or `count`, the low end of
/// a `range`. A free-form value that may carry a credential is refused.
fn fit(key: &str, form: Form, value: &str) -> Result<Option<f64>, Refusal> {
    let bad = || Refusal::BadValue {
        key: key.into(),
        form,
    };
    if value.is_empty() {
        return Err(bad());
    }
    if form.is_free() && may_carry_credential(value) {
        return Err(Refusal::SecretValue);
    }
    let num = match form {
        Form::Number => Some(decimal(value, true).ok_or_else(bad)?),
        Form::Count => {
            if !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            Some(value.parse::<f64>().map_err(|_| bad())?)
        }
        Form::Range => Some(range(value).ok_or_else(bad)?.0),
        Form::List => {
            if value.split(',').any(|t| t.trim().is_empty()) {
                return Err(bad());
            }
            None
        }
        Form::Status => {
            if !matches!(value, "open" | "temporarily_closed" | "permanently_closed") {
                return Err(bad());
            }
            None
        }
        Form::Point => {
            point(value).ok_or_else(bad)?;
            None
        }
        Form::OsmId => {
            let n = ["node/", "way/", "relation/"]
                .iter()
                .find_map(|p| value.strip_prefix(p))
                .ok_or_else(bad)?;
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            None
        }
        Form::Url => {
            let rest = value.strip_prefix("https://").ok_or_else(bad)?;
            if rest.is_empty() || rest.chars().any(char::is_whitespace) {
                return Err(bad());
            }
            None
        }
        Form::Hours | Form::Text => None,
        Form::Verdict => return Err(Refusal::VerdictKey),
    };
    Ok(num)
}

/// Digits with an optional fraction, and an optional leading `-` when `signed`.
pub(crate) fn decimal(s: &str, signed: bool) -> Option<f64> {
    let body = if signed {
        s.strip_prefix('-').unwrap_or(s)
    } else {
        s
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !(digits(int) && digits(frac)) {
        return None;
    }
    s.parse().ok()
}

/// The low and high ends of a `range`, equal for `A`.
pub(crate) fn range(value: &str) -> Option<(f64, f64)> {
    match value.split_once('-') {
        Some((a, b)) => {
            let (a, b) = (decimal(a.trim(), false)?, decimal(b.trim(), false)?);
            (a <= b).then_some((a, b))
        }
        None => decimal(value, false).map(|a| (a, a)),
    }
}

/// The latitude and longitude of a `point`.
pub(crate) fn point(value: &str) -> Option<(f64, f64)> {
    let (lat, lon) = value.split_once(',')?;
    let (lat, lon) = (decimal(lat.trim(), true)?, decimal(lon.trim(), true)?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
const DAY_MINUTES: u16 = 1_440;

/// The intervals of each weekday from Monday of an `hours` value, in minutes from that day's
/// midnight; an end past [`DAY_MINUTES`] reaches into the next day.
#[derive(Debug, PartialEq)]
pub(crate) struct Hours([Vec<(u16, u16)>; 7]);

impl Hours {
    /// Whether the hours say open at `minute` of `weekday`, 0 being Monday: inside an interval
    /// of that day, or of the day before when it ends past midnight.
    pub(crate) fn open_at(&self, weekday: usize, minute: u16) -> bool {
        let (today, before) = (&self.0[weekday % 7], &self.0[(weekday + 6) % 7]);
        today.iter().any(|&(s, e)| s <= minute && minute < e)
            || before.iter().any(|&(_, e)| minute + DAY_MINUTES < e)
    }
}

/// The [`Hours`] of an `hours` value in the subset of ADR-0080 section 2, `None` for any
/// other text. A day no rule names is closed.
pub(crate) fn hours(value: &str) -> Option<Hours> {
    let mut week: [Vec<(u16, u16)>; 7] = Default::default();
    if value.trim() == "24/7" {
        for day in &mut week {
            day.push((0, DAY_MINUTES));
        }
        return Some(Hours(week));
    }
    for rule in value.split(';') {
        let (days, times) = rule.trim().split_once(char::is_whitespace)?;
        let times = times.trim();
        let intervals = if times == "off" {
            Vec::new()
        } else {
            times
                .split(',')
                .map(|t| interval(t.trim()))
                .collect::<Option<Vec<_>>>()?
        };
        for day in weekdays(days)? {
            week[day].clone_from(&intervals);
        }
    }
    Some(Hours(week))
}

/// The weekdays from Monday 0 of a comma list of days `Mo` and ranges `Mo-Fr`; a range may run
/// past Sunday.
fn weekdays(days: &str) -> Option<Vec<usize>> {
    let day = |d: &str| WEEKDAYS.iter().position(|w| *w == d);
    let mut out = Vec::new();
    for part in days.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (day(a)?, day(b)?),
            None => (day(part)?, day(part)?),
        };
        let mut d = first;
        out.push(d);
        while d != last {
            d = (d + 1) % 7;
            out.push(d);
        }
    }
    Some(out)
}

/// `HH:MM-HH:MM` in minutes; an end at or before its start is past midnight.
fn interval(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.split_once('-')?;
    let (start, end) = (clock(a)?, clock(b)?);
    if start == DAY_MINUTES {
        return None;
    }
    Some((start, if end <= start { end + DAY_MINUTES } else { end }))
}

/// The minutes of `HH:MM` from `00:00` to `24:00`.
pub(crate) fn clock(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    let two = |p: &str| {
        (p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u16>().ok())
            .flatten()
    };
    let (h, m) = (two(h)?, two(m)?);
    (m < 60 && (h < 24 || (h == 24 && m == 0))).then_some(h * 60 + m)
}

fn current(c: &Connection, place: &EntityId, key: &str) -> Result<Option<Reading>, BedError> {
    Ok(c.query_row(
        "SELECT value, checked_at, source, confidence FROM bed_attr
         WHERE entity = ?1 AND key = ?2",
        params![place.0, key],
        |r| {
            Ok(Reading {
                value: r.get(0)?,
                checked_at: r.get(1)?,
                source: r.get(2)?,
                confidence: r.get(3)?,
            })
        },
    )
    .optional()?)
}

fn in_history(c: &Connection, place: &EntityId, key: &str, v: &Reading) -> Result<bool, BedError> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM bed_attr_history WHERE entity = ?1 AND key = ?2
             AND value = ?3 AND checked_at = ?4 AND source = ?5 AND confidence = ?6)",
        params![place.0, key, v.value, v.checked_at, v.source, v.confidence],
        |r| r.get(0),
    )?)
}

fn into_history(
    c: &Connection,
    place: &EntityId,
    key: &str,
    v: &Reading,
    replaced_at: UnixTime,
) -> Result<(), BedError> {
    c.execute(
        "INSERT INTO bed_attr_history (entity, key, value, checked_at, source, confidence,
             replaced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            place.0,
            key,
            v.value,
            v.checked_at,
            v.source,
            v.confidence,
            replaced_at
        ],
    )?;
    Ok(())
}

/// Keeps the copies of a new current `location` or `osm_id` in step: `bed_entity`'s `lat` and
/// `lon`, and the entity's [`PlaceAttrs`].
fn follow(s: &mut Store, place: &EntityId, key: &str, value: &str) -> Result<(), BedError> {
    let lat_lon = match key {
        "location" => Some(point(value).ok_or_else(|| Refusal::BadValue {
            key: key.into(),
            form: Form::Point,
        })?),
        "osm_id" => None,
        _ => return Ok(()),
    };
    if let Some((lat, lon)) = lat_lon {
        s.conn().execute(
            "UPDATE bed_entity SET lat = ?2, lon = ?3 WHERE entity = ?1",
            params![place.0, lat, lon],
        )?;
    }
    let mut e = s
        .entity(place)?
        .ok_or_else(|| StoreError::NotFound(format!("entity {}", place.0)))?;
    let mut attrs = match e.attrs {
        Attrs::Place(p) => p,
        Attrs::None => PlaceAttrs {
            osm_id: None,
            lat_lon: None,
        },
        _ => {
            return Err(
                StoreError::Corrupt(format!("entity {}: attrs of a place", place.0)).into(),
            );
        }
    };
    match lat_lon {
        Some(ll) => attrs.lat_lon = Some(ll),
        None => attrs.osm_id = Some(value.to_owned()),
    }
    e.attrs = Attrs::Place(attrs);
    s.upsert_entity_unresolved(&e)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MO: usize = 0;
    const TU: usize = 1;
    const WE: usize = 2;
    const TH: usize = 3;
    const FR: usize = 4;
    const SA: usize = 5;
    const SU: usize = 6;

    fn at(h: u16, m: u16) -> u16 {
        h * 60 + m
    }

    /// The weekdays on which `value` says open at `minute`.
    fn open_days(value: &str, minute: u16) -> Vec<usize> {
        let read = hours(value).unwrap();
        (0..7).filter(|&d| read.open_at(d, minute)).collect()
    }

    #[test]
    fn twenty_four_seven_is_open_every_minute() {
        let read = hours("24/7").unwrap();
        for d in 0..7 {
            assert!(read.open_at(d, 0) && read.open_at(d, at(23, 59)), "{d}");
        }
    }

    #[test]
    fn day_ranges_and_lists_name_their_days_and_no_other() {
        assert_eq!(
            open_days("Mo-Fr 09:00-18:00", at(12, 0)),
            [MO, TU, WE, TH, FR]
        );
        assert_eq!(open_days("Mo,We 09:00-18:00", at(12, 0)), [MO, WE]);
        assert_eq!(
            open_days("Mo-We,Sa 09:00-18:00", at(12, 0)),
            [MO, TU, WE, SA]
        );
        assert_eq!(open_days("Sa-Mo 09:00-18:00", at(12, 0)), [MO, SA, SU]);
    }

    #[test]
    fn several_intervals_open_inside_each_and_close_between_and_at_the_end() {
        let v = "Mo-Su 12:00-15:00, 18:00-23:00";
        assert_eq!(open_days(v, at(12, 0)).len(), 7);
        assert_eq!(open_days(v, at(14, 59)).len(), 7);
        assert_eq!(open_days(v, at(15, 0)), [] as [usize; 0]);
        assert_eq!(open_days(v, at(19, 30)).len(), 7);
        assert_eq!(open_days(v, at(23, 0)), [] as [usize; 0]);
        assert_eq!(open_days(v, at(11, 59)), [] as [usize; 0]);
    }

    #[test]
    fn off_closes_the_days_it_names() {
        assert_eq!(
            open_days("Mo-Sa 12:00-22:00; Su off", at(13, 0)),
            [MO, TU, WE, TH, FR, SA]
        );
    }

    #[test]
    fn an_interval_past_midnight_belongs_to_the_day_it_starts() {
        let v = "Fr 20:00-02:00";
        assert_eq!(open_days(v, at(23, 0)), [FR]);
        assert_eq!(
            open_days(v, at(1, 30)),
            [SA],
            "after midnight of Friday's interval"
        );
        assert_eq!(open_days(v, at(2, 0)), [] as [usize; 0]);
        assert_eq!(open_days("Mo-Su 18:00-24:00", at(0, 30)), [] as [usize; 0]);
        assert_eq!(open_days("Su 22:00-00:00", at(23, 59)), [SU]);
    }

    #[test]
    fn a_later_rule_overrides_an_earlier_one_for_the_days_it_names() {
        let v = "Mo-Fr 09:00-18:00; We 12:00-14:00; Fr off";
        assert_eq!(open_days(v, at(10, 0)), [MO, TU, TH]);
        assert_eq!(open_days(v, at(13, 0)), [MO, TU, WE, TH]);
    }

    #[test]
    fn text_outside_the_subset_is_not_read() {
        for v in [
            "",
            "Mo-Fr",
            "09:00-18:00",
            "Mo-Fr 9:00-18:00",
            "Mo-Fr 09:00-18:60",
            "Mo-Fr 09:00-25:00",
            "Mo-Fr 24:00-02:00",
            "Mo-Fr 09:00-18:00;",
            "Mo-Fr 09:00-18:00; PH off",
            "Jan-Mar Mo 09:00-18:00",
            "Mo-Fr 09:00+",
            "Mon 09:00-18:00",
            "mo 09:00-18:00",
            "by appointment",
        ] {
            assert_eq!(hours(v), None, "{v:?}");
        }
    }
}
