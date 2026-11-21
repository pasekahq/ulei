//! Vocabulary of the garden: entities of a small fixed set of kinds, domains bound to Wikidata
//! classes, and facts with a valid-time and a transaction-time axis, provenance, an optional
//! expiry for facts from the internet and a sensitivity that keeps secret values out.

use std::fmt;

use crate::secret::{has_token, may_carry_secret};

/// Unix time in whole seconds (UTC).
pub type UnixTime = i64;

/// Kind of an entity. The set is fixed; topics such as food or theatre are [`Domain`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntityKind {
    /// A person from the owner's life (the dossier).
    Person,
    /// A physical place: home, office, cafe, clinic.
    Place,
    /// Something that happens at a time: a flight, a birthday, a show.
    Event,
    /// A thing: a document, a device, a medicine, a dish.
    Item,
    /// A like or dislike of the owner or of a person (see [`PreferenceAttrs`]).
    Preference,
    /// A visit of a place (see [`VisitAttrs`]).
    Visit,
}

impl EntityKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Person,
        Self::Place,
        Self::Event,
        Self::Item,
        Self::Preference,
        Self::Visit,
    ];

    /// Stable lowercase name used in storage and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Place => "place",
            Self::Event => "event",
            Self::Item => "item",
            Self::Preference => "preference",
            Self::Visit => "visit",
        }
    }

    /// Inverse of [`EntityKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable identifier of an entity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(pub String);

/// Stable identifier of one fact row (one version of a statement).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FactId(pub String);

/// Most digits of a [`WikidataQid`]; item ids have nine today.
pub const MAX_QID_DIGITS: usize = 10;

/// Longest identifier of a fact, an entity or a domain, in bytes.
pub const MAX_ID: usize = 200;

/// A Wikidata item id such as `Q2095` (food); validated on construction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WikidataQid(String);

impl WikidataQid {
    /// Accepts `Q` followed by a decimal number of at most [`MAX_QID_DIGITS`] digits without a
    /// leading zero.
    ///
    /// # Errors
    ///
    /// [`ModelError::InvalidQid`] for anything else.
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        let digits = s.strip_prefix('Q').unwrap_or("");
        let ok = !digits.is_empty()
            && digits.len() <= MAX_QID_DIGITS
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit());
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(ModelError::InvalidQid(s.to_owned()))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn invalid(msg: String) -> Result<(), ModelError> {
    Err(ModelError::Invalid(msg))
}

fn check_id(what: &str, id: &str) -> Result<(), ModelError> {
    if id.trim().is_empty() || id.len() > MAX_ID || id.chars().any(char::is_control) {
        return invalid(format!("bad {what} id {id:?}"));
    }
    Ok(())
}

/// A topic of the garden (food, doctors, medicines, theatre). Domains are data, not code: the
/// starting set is seeded, later ones are born from clustering. The Wikidata class gives
/// deduplication and a hierarchy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// Short stable key, lowercase ASCII with `-`.
    pub slug: String,
    /// Human-readable name.
    pub label: String,
    /// The Wikidata class this domain stands for, if known.
    pub wikidata: Option<WikidataQid>,
    /// Slug of the parent domain, if any.
    pub parent: Option<String>,
}

impl Domain {
    /// Checks the invariants: slug of lowercase ASCII letters, digits and `-` of at most
    /// [`MAX_ID`] bytes, a non-empty label, not its own parent.
    ///
    /// # Errors
    ///
    /// [`ModelError::Invalid`] naming the broken rule.
    pub fn validate(&self) -> Result<(), ModelError> {
        let s = &self.slug;
        let ok = !s.is_empty()
            && s.len() <= MAX_ID
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return invalid(format!("bad domain slug {s:?}"));
        }
        if self.label.trim().is_empty() {
            return invalid(format!("domain {s}: empty label"));
        }
        if self.parent.as_ref() == Some(s) {
            return invalid(format!("domain {s}: unknown parent {s}"));
        }
        Ok(())
    }
}

/// An entity: the subject that facts are about.
#[derive(Clone, Debug, PartialEq)]
pub struct Entity {
    /// Identifier.
    pub id: EntityId,
    /// Kind.
    pub kind: EntityKind,
    /// Canonical name.
    pub name: String,
    /// Other names the owner uses (nicknames, transliterations); retrieval matches them too.
    pub aliases: Vec<String>,
    /// Domain slug, if the entity belongs to one.
    pub domain: Option<String>,
    /// Kind-specific attributes.
    pub attrs: Attrs,
}

impl Entity {
    /// Checks the invariants: a valid id, a non-empty name, attributes that fit the kind, a
    /// preference weight in `[0, 1]`, a visit rating in `1..=5`, finite coordinates with the
    /// latitude in `[-90, 90]` and the longitude in `[-180, 180]`.
    ///
    /// # Errors
    ///
    /// [`ModelError::Invalid`] naming the broken rule.
    pub fn validate(&self) -> Result<(), ModelError> {
        check_id("entity", &self.id.0)?;
        let id = &self.id.0;
        if self.name.trim().is_empty() {
            return invalid(format!("entity {id}: empty name"));
        }
        match (self.kind, &self.attrs) {
            (_, Attrs::None) => {}
            (EntityKind::Place, Attrs::Place(p)) => {
                if let Some((lat, lon)) = p.lat_lon
                    && !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
                {
                    return invalid(format!("entity {id}: lat_lon out of range or not finite"));
                }
            }
            (EntityKind::Preference, Attrs::Preference(p)) => {
                if !(0.0..=1.0).contains(&p.weight) {
                    return invalid(format!("entity {id}: weight outside [0, 1]"));
                }
            }
            (EntityKind::Visit, Attrs::Visit(v)) => {
                if v.rating.is_some_and(|r| !(1..=5).contains(&r)) {
                    return invalid(format!("entity {id}: rating outside 1..=5"));
                }
            }
            _ => {
                return invalid(format!(
                    "entity {id}: attributes do not fit kind {}",
                    self.kind
                ));
            }
        }
        Ok(())
    }
}

/// Kind-specific attributes of an entity.
#[derive(Clone, Debug, PartialEq)]
pub enum Attrs {
    /// No structured attributes.
    None,
    /// Attributes of a [`EntityKind::Place`].
    Place(PlaceAttrs),
    /// Attributes of a [`EntityKind::Preference`].
    Preference(PreferenceAttrs),
    /// Attributes of a [`EntityKind::Visit`].
    Visit(VisitAttrs),
}

/// Where a place is.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceAttrs {
    /// OpenStreetMap object id such as `node/123`, if known.
    pub osm_id: Option<String>,
    /// Latitude and longitude in degrees, if known.
    pub lat_lon: Option<(f64, f64)>,
}

/// Direction of a preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// The holder likes it.
    Like,
    /// The holder dislikes or must avoid it.
    Dislike,
}

/// How a preference constrains a recommendation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hardness {
    /// A filter: an allergy, a diet, "never". Only from the holder's explicit words.
    Hard,
    /// A ranking signal: "not a fan of coriander".
    Soft,
}

/// A preference: who holds it, about what, how strongly.
#[derive(Clone, Debug, PartialEq)]
pub struct PreferenceAttrs {
    /// The person holding the preference.
    pub holder: EntityId,
    /// What it is about (an item, a place, a domain-level concept as an entity).
    pub object: EntityId,
    /// Like or dislike.
    pub polarity: Polarity,
    /// Strength in `[0, 1]`, grown by confirmations and decayed over time.
    pub weight: f32,
    /// Filter or ranking signal.
    pub hardness: Hardness,
}

/// A visit of a place.
#[derive(Clone, Debug, PartialEq)]
pub struct VisitAttrs {
    /// The place visited.
    pub place: EntityId,
    /// When.
    pub at: UnixTime,
    /// The owner's rating from 1 to 5, if given.
    pub rating: Option<u8>,
    /// People who were there.
    pub with: Vec<EntityId>,
}

/// Where a fact comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// The owner said it.
    Owner,
    /// Extracted from the owner's raw material: chats, diaries, uploads, sessions.
    Raw,
    /// Fetched from the internet; carries an expiry by its [`TtlKind`].
    Internet,
    /// Proposed by an agent without a direct source.
    Agent,
}

/// Evidence of a fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// Kind of source.
    pub kind: SourceKind,
    /// Locator of the evidence: a message id, a file and line, a URL.
    pub source_ref: String,
    /// When the source was observed.
    pub observed_at: UnixTime,
    /// The exact words of the source that state the fact, if kept.
    pub quote: Option<String>,
    /// How many sources confirm the fact; at least 1.
    pub proof_count: u32,
}

/// Type of a fact for its lifetime: how fast what it says goes stale in the world. Keys of the
/// [`TtlTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TtlKind {
    /// Weather and forecasts.
    Weather,
    /// "Open now", live arrivals: never cached, the garden rejects it.
    NeverCache,
    /// Exchange rates.
    FxRate,
    /// Pharmacies on duty.
    DutyPharmacy,
    /// News.
    News,
    /// Prices and tariffs.
    Price,
    /// Availability, stock.
    Availability,
    /// Timetables and schedules.
    Timetable,
    /// Opening hours of a place.
    OpeningHours,
    /// Contacts of a place: phone, site.
    Contacts,
    /// Capabilities and limits of a product or service.
    Product,
    /// Laws, visas, taxes, procedures; re-checked before acting on them.
    Law,
    /// Existence of a place, address of an institution.
    Place,
    /// General knowledge, language.
    General,
    /// Birthdays, people's addresses, own documents: no expiry, changes are new versions.
    Personal,
}

impl TtlKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 15] = [
        Self::Weather,
        Self::NeverCache,
        Self::FxRate,
        Self::DutyPharmacy,
        Self::News,
        Self::Price,
        Self::Availability,
        Self::Timetable,
        Self::OpeningHours,
        Self::Contacts,
        Self::Product,
        Self::Law,
        Self::Place,
        Self::General,
        Self::Personal,
    ];

    /// Stable `snake_case` name used in storage, config and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weather => "weather",
            Self::NeverCache => "never_cache",
            Self::FxRate => "fx_rate",
            Self::DutyPharmacy => "duty_pharmacy",
            Self::News => "news",
            Self::Price => "price",
            Self::Availability => "availability",
            Self::Timetable => "timetable",
            Self::OpeningHours => "opening_hours",
            Self::Contacts => "contacts",
            Self::Product => "product",
            Self::Law => "law",
            Self::Place => "place",
            Self::General => "general",
            Self::Personal => "personal",
        }
    }

    /// Inverse of [`TtlKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    fn index(self) -> usize {
        // Declaration order equals the order of `ALL`.
        self as usize
    }
}

impl fmt::Display for TtlKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifetime of facts of one [`TtlKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ttl {
    /// Never stored.
    NeverCache,
    /// Expires this many seconds after the source was observed.
    Secs(i64),
    /// Never expires.
    Forever,
}

/// Lifetime rule of one [`TtlKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtlRule {
    /// Lifetime.
    pub ttl: Ttl,
    /// Whether a consumer must re-check the fact at its source before acting on it.
    pub recheck_before_acting: bool,
}

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// Lifetime of every [`TtlKind`]; [`TtlTable::default`] is the researched starting table, a
/// config may override single rows with [`TtlTable::set`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TtlTable([TtlRule; TtlKind::ALL.len()]);

impl Default for TtlTable {
    fn default() -> Self {
        let rule = |ttl| TtlRule {
            ttl,
            recheck_before_acting: false,
        };
        let mut rows = [rule(Ttl::Forever); TtlKind::ALL.len()];
        for k in TtlKind::ALL {
            rows[k.index()] = match k {
                TtlKind::Weather => rule(Ttl::Secs(3 * HOUR)),
                TtlKind::NeverCache => rule(Ttl::NeverCache),
                TtlKind::FxRate | TtlKind::DutyPharmacy | TtlKind::News => rule(Ttl::Secs(DAY)),
                TtlKind::Price | TtlKind::Availability | TtlKind::Timetable => {
                    rule(Ttl::Secs(7 * DAY))
                }
                TtlKind::OpeningHours | TtlKind::Contacts => rule(Ttl::Secs(30 * DAY)),
                // The research gives 30 to 90 days; the middle until the eval says otherwise.
                TtlKind::Product => rule(Ttl::Secs(60 * DAY)),
                TtlKind::Law => TtlRule {
                    ttl: Ttl::Secs(90 * DAY),
                    recheck_before_acting: true,
                },
                TtlKind::Place => rule(Ttl::Secs(180 * DAY)),
                TtlKind::General | TtlKind::Personal => rule(Ttl::Forever),
            };
        }
        Self(rows)
    }
}

impl TtlTable {
    /// The rule of `kind`.
    pub fn rule(&self, kind: TtlKind) -> TtlRule {
        self.0[kind.index()]
    }

    /// Replaces the rule of `kind`.
    pub fn set(&mut self, kind: TtlKind, rule: TtlRule) {
        self.0[kind.index()] = rule;
    }

    /// Expiry of a fact of `kind` observed at `observed_at`; `None` = no expiry. A
    /// [`Ttl::NeverCache`] kind expires at once.
    pub fn expiry(&self, kind: TtlKind, observed_at: UnixTime) -> Option<UnixTime> {
        match self.rule(kind).ttl {
            Ttl::NeverCache => Some(observed_at),
            Ttl::Secs(s) => Some(observed_at.saturating_add(s)),
            Ttl::Forever => None,
        }
    }
}

/// Who may see a fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sensitivity {
    /// Any runtime of the owner.
    Public,
    /// Only the owner's own runtime (C7 `private`).
    Private,
    /// The fact only says a secret exists; the value lives in the vault under `vault_ref` and
    /// never in the garden.
    Secret {
        /// Opaque reference of the vault entry.
        vault_ref: String,
    },
}

/// One version of a statement about an entity.
///
/// Valid time (`valid_from`, `valid_to`) is when the statement is true in the world;
/// transaction time (`recorded_at`, `superseded_at`) is when the garden believed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    /// Identifier of this version.
    pub id: FactId,
    /// The entity it is about.
    pub entity: EntityId,
    /// Statement text, one sentence.
    pub text: String,
    /// Domain slug, if any.
    pub domain: Option<String>,
    /// Name of a single-valued attribute of the entity this fact sets (`address`, `birthday`,
    /// `phone`): a new current version of the same attribute closes the old one. `None` = a
    /// multi-valued statement that only adds.
    pub attribute: Option<String>,
    /// Type of the fact for its lifetime.
    pub ttl: TtlKind,
    /// Start of validity in the world.
    pub valid_from: UnixTime,
    /// End of validity in the world; `None` = still valid.
    pub valid_to: Option<UnixTime>,
    /// When the garden recorded this version.
    pub recorded_at: UnixTime,
    /// When the garden replaced or retracted this version; `None` = current belief.
    pub superseded_at: Option<UnixTime>,
    /// The version that replaced this one, if it was replaced rather than retracted or expired.
    pub superseded_by: Option<FactId>,
    /// Evidence.
    pub provenance: Provenance,
    /// After this time the fact is stale and must be refreshed; `None` = no expiry.
    pub expires_at: Option<UnixTime>,
    /// Visibility.
    pub sensitivity: Sensitivity,
    /// How sure the source is, in `[0, 1]`.
    pub confidence: f32,
}

impl Fact {
    /// Whether the garden, as it was at `known_at`, held this fact as true at `valid_at` and
    /// not yet expired at `known_at`.
    pub fn holds(&self, valid_at: UnixTime, known_at: UnixTime) -> bool {
        self.valid_from <= valid_at
            && self.valid_to.is_none_or(|t| valid_at < t)
            && self.recorded_at <= known_at
            && self.superseded_at.is_none_or(|t| known_at < t)
            && self.expires_at.is_none_or(|t| known_at < t)
    }

    /// Checks the invariants of a stored version: valid ids, a non-empty text, attribute,
    /// provenance ref and quote, a proof count of at least 1, a confidence in `[0, 1]`,
    /// `valid_to` after `valid_from`, `superseded_at` not before `recorded_at` and
    /// `superseded_by` only with `superseded_at`; a secret needs a vault reference, has no
    /// quote, and neither its text nor its provenance ref may look like they carry the value.
    ///
    /// # Errors
    ///
    /// [`ModelError::Invalid`] naming the broken rule.
    pub fn validate(&self) -> Result<(), ModelError> {
        check_id("fact", &self.id.0)?;
        check_id("entity", &self.entity.0)?;
        let id = &self.id.0;
        if self.text.trim().is_empty() {
            return invalid(format!("fact {id}: empty text"));
        }
        if self.provenance.source_ref.trim().is_empty() {
            return invalid(format!("fact {id}: empty provenance ref"));
        }
        if self
            .provenance
            .quote
            .as_ref()
            .is_some_and(|q| q.trim().is_empty())
        {
            return invalid(format!("fact {id}: empty provenance quote"));
        }
        if self.provenance.proof_count == 0 {
            return invalid(format!("fact {id}: proof_count is 0"));
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return invalid(format!("fact {id}: confidence outside [0, 1]"));
        }
        if self.valid_to.is_some_and(|t| t <= self.valid_from) {
            return invalid(format!("fact {id}: valid_to is not after valid_from"));
        }
        if self.superseded_at.is_some_and(|t| t < self.recorded_at) {
            return invalid(format!("fact {id}: superseded_at precedes recorded_at"));
        }
        if let Some(by) = &self.superseded_by {
            check_id("fact", &by.0)?;
            if self.superseded_at.is_none() {
                return invalid(format!("fact {id}: superseded_by without superseded_at"));
            }
        }
        if self.attribute.as_ref().is_some_and(|a| a.trim().is_empty()) {
            return invalid(format!("fact {id}: empty attribute"));
        }
        if let Sensitivity::Secret { vault_ref } = &self.sensitivity {
            if vault_ref.trim().is_empty() {
                return invalid(format!("fact {id}: secret without a vault reference"));
            }
            if may_carry_secret(&self.text) {
                return invalid(format!(
                    "fact {id}: the text of a secret fact may carry its value; say only that it exists"
                ));
            }
            if self.provenance.quote.is_some() {
                return invalid(format!(
                    "fact {id}: a secret fact keeps no quote; it may carry the value"
                ));
            }
            if has_token(&self.provenance.source_ref) {
                return invalid(format!(
                    "fact {id}: the provenance ref of a secret fact may carry its value"
                ));
            }
        }
        Ok(())
    }
}

/// A value that breaks the model's invariants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// Not a Wikidata item id.
    InvalidQid(String),
    /// An entity, a fact or a domain breaks a rule; the text names the object and the rule.
    Invalid(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQid(s) => write!(f, "not a Wikidata item id: {s:?}"),
            Self::Invalid(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ModelError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(valid: (i64, Option<i64>), recorded: (i64, Option<i64>), exp: Option<i64>) -> Fact {
        Fact {
            id: FactId("f1".into()),
            entity: EntityId("e1".into()),
            text: "Office is at 1 Example Street.".into(),
            domain: None,
            attribute: None,
            ttl: TtlKind::General,
            valid_from: valid.0,
            valid_to: valid.1,
            recorded_at: recorded.0,
            superseded_at: recorded.1,
            superseded_by: None,
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: "msg:1".into(),
                observed_at: recorded.0,
                quote: None,
                proof_count: 1,
            },
            expires_at: exp,
            sensitivity: Sensitivity::Public,
            confidence: 1.0,
        }
    }

    #[test]
    fn kinds_round_trip() {
        for k in EntityKind::ALL {
            assert_eq!(EntityKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EntityKind::parse("domain"), None);
    }

    #[test]
    fn qid_is_validated() {
        assert_eq!(WikidataQid::parse("Q2095").unwrap().as_str(), "Q2095");
        assert!(WikidataQid::parse("Q1234567890").is_ok());
        for bad in ["", "Q", "Q0", "Q01", "q5", "P31", "Q5a", "Q12345678901"] {
            assert!(WikidataQid::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn holds_respects_both_axes_and_expiry() {
        let f = fact((100, Some(200)), (150, Some(300)), None);
        assert!(f.holds(150, 150));
        assert!(!f.holds(99, 150), "before valid_from");
        assert!(!f.holds(200, 150), "valid_to is exclusive");
        assert!(!f.holds(150, 149), "not yet recorded");
        assert!(!f.holds(150, 300), "superseded");
        let e = fact((0, None), (0, None), Some(10));
        assert!(e.holds(5, 9));
        assert!(!e.holds(5, 10), "expired");
    }

    #[test]
    fn ttl_table_defaults_and_override() {
        let mut t = TtlTable::default();
        assert_eq!(t.expiry(TtlKind::Weather, 0), Some(3 * 3600));
        assert_eq!(t.expiry(TtlKind::FxRate, 0), Some(86_400));
        assert_eq!(t.expiry(TtlKind::Price, 0), Some(7 * 86_400));
        assert_eq!(t.expiry(TtlKind::OpeningHours, 0), Some(30 * 86_400));
        assert_eq!(t.expiry(TtlKind::Place, 0), Some(180 * 86_400));
        assert_eq!(t.expiry(TtlKind::Personal, 0), None);
        assert_eq!(t.expiry(TtlKind::General, 0), None);
        assert_eq!(t.rule(TtlKind::NeverCache).ttl, Ttl::NeverCache);
        assert!(t.rule(TtlKind::Law).recheck_before_acting);
        let r = TtlRule {
            ttl: Ttl::Secs(10),
            recheck_before_acting: false,
        };
        t.set(TtlKind::News, r);
        assert_eq!(t.expiry(TtlKind::News, 5), Some(15));
        for k in TtlKind::ALL {
            assert_eq!(TtlKind::parse(k.as_str()), Some(k));
        }
    }
}
