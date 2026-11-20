//! Vocabulary of the garden: entities of a small fixed set of kinds, domains bound to Wikidata
//! classes, and facts with a valid-time and a transaction-time axis, provenance, an optional
//! expiry for facts from the internet and a sensitivity that keeps secret values out.

use std::fmt;

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

/// A Wikidata item id such as `Q2095` (food); validated on construction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WikidataQid(String);

impl WikidataQid {
    /// Accepts `Q` followed by a decimal number without a leading zero.
    ///
    /// # Errors
    ///
    /// [`ModelError::InvalidQid`] for anything else.
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        let digits = s.strip_prefix('Q').unwrap_or("");
        let ok = !digits.is_empty()
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
    /// Fetched from the internet; carries an expiry by [`TtlClass`].
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
}

/// How fast an internet fact goes stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlClass {
    /// Prices, opening hours, availability.
    Volatile,
    /// Menus, schedules, programmes.
    Seasonal,
    /// Birthdays, addresses, founding dates: no expiry.
    Stable,
}

impl TtlClass {
    /// Lifetime in seconds after `observed_at`; `None` = no expiry.
    pub fn ttl_secs(self) -> Option<i64> {
        match self {
            Self::Volatile => Some(24 * 3600),
            Self::Seasonal => Some(7 * 24 * 3600),
            Self::Stable => None,
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
    /// Start of validity in the world.
    pub valid_from: UnixTime,
    /// End of validity in the world; `None` = still valid.
    pub valid_to: Option<UnixTime>,
    /// When the garden recorded this version.
    pub recorded_at: UnixTime,
    /// When the garden replaced or retracted this version; `None` = current belief.
    pub superseded_at: Option<UnixTime>,
    /// Evidence.
    pub provenance: Provenance,
    /// After this time the fact is stale and must be refreshed; `None` = no expiry.
    pub expires_at: Option<UnixTime>,
    /// Visibility.
    pub sensitivity: Sensitivity,
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

    /// Expiry of an internet fact of `class` observed at `observed_at`.
    pub fn expiry_for(class: TtlClass, observed_at: UnixTime) -> Option<UnixTime> {
        class.ttl_secs().map(|s| observed_at.saturating_add(s))
    }
}

/// A value that breaks the model's invariants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// Not a Wikidata item id.
    InvalidQid(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQid(s) => write!(f, "not a Wikidata item id: {s:?}"),
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
            valid_from: valid.0,
            valid_to: valid.1,
            recorded_at: recorded.0,
            superseded_at: recorded.1,
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: "msg:1".into(),
                observed_at: recorded.0,
            },
            expires_at: exp,
            sensitivity: Sensitivity::Public,
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
        for bad in ["", "Q", "Q0", "Q01", "q5", "P31", "Q5a"] {
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
    fn ttl_classes() {
        assert_eq!(Fact::expiry_for(TtlClass::Volatile, 0), Some(86_400));
        assert_eq!(Fact::expiry_for(TtlClass::Seasonal, 0), Some(604_800));
        assert_eq!(Fact::expiry_for(TtlClass::Stable, 0), None);
    }
}
