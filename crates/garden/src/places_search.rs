//! `places_search` over the places bed (ADR-0080 section 5): one SQL query for the status, the
//! bounding box, the rating and price thresholds and the latest verdict, then the distance,
//! cuisine and bans in Rust, the lexicographic order and the answer with its marks.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fmt;

use rusqlite::{Connection, params};

use crate::edge::normalise;
use crate::model::UnixTime;
use crate::morph;
use crate::places::{PLACE, decimal, point, range};
use crate::search;
use crate::store::StoreError;
use crate::translit;
use crate::wire::c7;

/// Places answered when the request names no `limit`.
pub const DEFAULT_LIMIT: u64 = 10;
/// Places answered at most.
pub const MAX_LIMIT: u64 = 50;

const MAX_CUISINE_TERMS: usize = 10;
const MAX_CUISINE_CHARS: usize = 100;
const MAX_DISTANCE_M: u64 = 100_000;
/// Statuses of a `status` filter at most, one per [`c7::PlaceStatus`]; the query binds three.
const MAX_STATUSES: usize = 3;

const DAY: i64 = 86_400;
/// Metres of one degree of latitude on the sphere below, rounded down.
const METRES_PER_DEGREE: f64 = 111_195.0;
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A failure of a search.
#[derive(Debug)]
pub enum FindError {
    /// The request breaks a rule of section 5 item 1; the text says which.
    Invalid(&'static str),
    /// The store failed or holds a value outside its form.
    Store(StoreError),
}

impl fmt::Display for FindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl From<rusqlite::Error> for FindError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(StoreError::Sqlite(e))
    }
}

/// A threshold on a `num` and whether it came from the request, which drops a place without
/// the attribute, or from the profile, which drops only a place whose value misses it.
#[derive(Clone, Copy)]
struct Threshold {
    value: f64,
    asked: bool,
}

/// The bed profile of type `place` as a search reads it.
#[derive(Default)]
struct Profile {
    home: Option<(f64, f64)>,
    min_rating: Option<f64>,
    budget_high: Option<f64>,
    bans: Vec<String>,
}

/// One `bed_attr` row of a candidate.
struct Attr {
    key: String,
    value: String,
    num: Option<f64>,
    checked_at: UnixTime,
    source: String,
    confidence: f64,
    due_at: Option<UnixTime>,
    demotes: bool,
}

impl Attr {
    fn stale(&self, now: UnixTime) -> bool {
        self.due_at.is_some_and(|d| d < now)
    }

    fn age_days(&self, now: UnixTime) -> i64 {
        (now - self.checked_at).max(0) / DAY
    }
}

/// A place the query returned, with what the order reads.
struct Candidate {
    id: String,
    name: String,
    status: c7::PlaceStatus,
    lat_lon: Option<(f64, f64)>,
    verdict: Option<c7::PlaceVerdict>,
    attrs: Vec<Attr>,
    distance: Option<f64>,
}

impl Candidate {
    fn attr(&self, key: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.key == key)
    }

    fn list(&self, key: &str) -> Vec<&str> {
        self.attr(key)
            .map(|a| a.value.split(',').map(str::trim).collect())
            .unwrap_or_default()
    }
}

/// The places of the bed for `req` at `now`, best first (ADR-0080 section 5).
///
/// # Errors
///
/// [`FindError::Invalid`] for `open_now` without `local_time`, `max_distance_m` without a point,
/// a `limit` outside 1 to [`MAX_LIMIT`], a `near` outside WGS 84 or a filter outside its C7 bounds;
/// [`FindError::Store`] for a failure of the store.
pub fn search(
    c: &Connection,
    req: &c7::PlacesSearchRequest,
    now: UnixTime,
) -> Result<c7::PlacesSearchResponse, FindError> {
    let open_now = req.open_now.unwrap_or(false);
    if open_now && req.local_time.is_none() {
        return Err(FindError::Invalid("`open_now` needs `local_time`"));
    }
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(FindError::Invalid("`limit` is 1 to 50"));
    }
    if let Some(p) = &req.near
        && !((-90.0..=90.0).contains(&p.lat) && (-180.0..=180.0).contains(&p.lon))
    {
        return Err(FindError::Invalid("`near` is outside WGS 84"));
    }
    if let Some(f) = &req.filters {
        in_bounds(f)?;
    }
    let default_filters = c7::PlacesFilters {
        cuisine: None,
        max_distance_m: None,
        min_rating: None,
        max_price_for_two: None,
        status: None,
    };
    let f = req.filters.as_ref().unwrap_or(&default_filters);
    let profile = profile(c)?;
    let at = req.near.as_ref().map(|p| (p.lat, p.lon)).or(profile.home);
    #[allow(clippy::cast_precision_loss)]
    let max_distance = f.max_distance_m.map(|m| m as f64);
    if max_distance.is_some() && at.is_none() {
        return Err(FindError::Invalid(
            "`max_distance_m` needs `near` or a profile `home`",
        ));
    }
    let statuses = f
        .status
        .clone()
        .unwrap_or_else(|| vec![c7::PlaceStatus::Active, c7::PlaceStatus::Candidate]);
    let threshold = |asked: Option<f64>, profile: Option<f64>| {
        asked
            .map(|value| Threshold { value, asked: true })
            .or(profile.map(|value| Threshold {
                value,
                asked: false,
            }))
    };
    let rating = threshold(f.min_rating, profile.min_rating);
    let price = threshold(f.max_price_for_two, profile.budget_high);
    let bbox = at.zip(max_distance).map(|(p, d)| bounding_box(p, d));

    let mut found = query(c, &statuses, bbox, rating, price)?;

    let cuisine: Vec<Vec<String>> = f
        .cuisine
        .iter()
        .flatten()
        .map(|t| search::tokens(&normalise(t)))
        .collect();
    let bans: Vec<(&str, Vec<String>)> = profile
        .bans
        .iter()
        .map(|b| (b.as_str(), search::tokens(&normalise(b))))
        .collect();
    found.retain_mut(|p| {
        p.distance = at.zip(p.lat_lon).map(|(a, b)| haversine(a, b));
        if let Some(max) = max_distance
            && p.distance.is_none_or(|d| d > max)
        {
            return false;
        }
        let terms: Vec<Vec<String>> = p
            .list("cuisine")
            .into_iter()
            .map(|t| search::tokens(&normalise(t)))
            .collect();
        if !cuisine.is_empty() && !cuisine.iter().any(|w| terms.iter().any(|t| names(t, w))) {
            return false;
        }
        !bans.iter().any(|(_, w)| terms.iter().any(|t| names(t, w)))
    });

    let vibe: BTreeSet<String> = req.vibe.as_deref().map(stems).unwrap_or_default();
    let mut ranked: Vec<(Key, Candidate)> = found
        .into_iter()
        .map(|p| (key(&p, &vibe, now), p))
        .collect();
    ranked.sort_by(|(a, _), (b, _)| a.cmp(b));
    #[allow(clippy::cast_possible_truncation)]
    ranked.truncate(limit as usize);

    let mut places = Vec::with_capacity(ranked.len());
    for (_, p) in ranked {
        let others = duplicates(c, &p.id)?;
        places.push(hit(p, &bans, others, open_now, now));
    }
    Ok(c7::PlacesSearchResponse { places })
}

/// Refuses a filter outside its C7 bounds (`PlacesFilters`).
fn in_bounds(f: &c7::PlacesFilters) -> Result<(), FindError> {
    if let Some(terms) = &f.cuisine {
        if !(1..=MAX_CUISINE_TERMS).contains(&terms.len()) {
            return Err(FindError::Invalid("`cuisine` holds 1 to 10 terms"));
        }
        if terms
            .iter()
            .any(|t| !(1..=MAX_CUISINE_CHARS).contains(&t.chars().count()))
        {
            return Err(FindError::Invalid(
                "a `cuisine` term holds 1 to 100 characters",
            ));
        }
    }
    if f.max_distance_m
        .is_some_and(|m| !(1..=MAX_DISTANCE_M).contains(&m))
    {
        return Err(FindError::Invalid("`max_distance_m` is 1 to 100000"));
    }
    if f.min_rating.is_some_and(|r| !(0.0..=5.0).contains(&r)) {
        return Err(FindError::Invalid("`min_rating` is 0 to 5"));
    }
    if f.max_price_for_two.is_some_and(|p| p < 0.0) {
        return Err(FindError::Invalid("`max_price_for_two` is at least 0"));
    }
    if let Some(statuses) = &f.status {
        if !(1..=MAX_STATUSES).contains(&statuses.len()) {
            return Err(FindError::Invalid("`status` holds 1 to 3 statuses"));
        }
        if statuses
            .iter()
            .enumerate()
            .any(|(i, s)| statuses[..i].contains(s))
        {
            return Err(FindError::Invalid("`status` repeats a status"));
        }
    }
    Ok(())
}

/// The order of section 5 item 5, best first under `Ord`: a liked verdict, the vibe count,
/// the floored quality, the distance with none last, the id. A distance is never negative, so
/// its bits order as its value.
type Key = (
    Reverse<bool>,
    Reverse<usize>,
    Reverse<i64>,
    (bool, u64),
    String,
);

fn key(p: &Candidate, vibe: &BTreeSet<String>, now: UnixTime) -> Key {
    let liked = p.verdict.as_ref().is_some_and(|v| v.verdict == 1);
    let found = if vibe.is_empty() {
        0
    } else {
        let mut own = BTreeSet::new();
        for k in ["vibe", "cuisine"] {
            if let Some(a) = p.attr(k) {
                own.extend(stems(&a.value));
            }
        }
        if let Some(note) = p.verdict.as_ref().and_then(|v| v.note.as_deref()) {
            own.extend(stems(note));
        }
        vibe.intersection(&own).count()
    };
    let num = |k: &str| p.attr(k).and_then(|a| a.num).unwrap_or(0.0);
    let s = p.attrs.iter().filter(|a| a.demotes && a.stale(now)).count();
    let quality = num("rating") * (2.0 + num("review_count")).ln() * 0.5_f64.powi(exponent(s));
    #[allow(clippy::cast_possible_truncation)]
    let quality = quality.floor() as i64;
    (
        Reverse(liked),
        Reverse(found),
        Reverse(quality),
        (p.distance.is_none(), p.distance.map_or(0, f64::to_bits)),
        p.id.clone(),
    )
}

fn exponent(s: usize) -> i32 {
    i32::try_from(s).unwrap_or(i32::MAX)
}

/// The answer of one place: its attributes by key, the `stale` marks of the attributes that
/// demote it, `ban_risk` with the bans its `warnings` name and `possible_duplicate`.
fn hit(
    p: Candidate,
    bans: &[(&str, Vec<String>)],
    others: Vec<String>,
    open_now: bool,
    now: UnixTime,
) -> c7::PlaceHit {
    let mut marks: Vec<c7::PlaceMark> = p
        .attrs
        .iter()
        .filter(|a| a.demotes && a.stale(now))
        .map(|a| c7::PlaceMark {
            kind: c7::PlaceMarkKind::Stale,
            key: Some(a.key.clone()),
            age_days: Some(a.age_days(now).unsigned_abs()),
            terms: None,
            other: None,
        })
        .collect();
    let warnings: Vec<Vec<String>> = p
        .list("warnings")
        .into_iter()
        .map(|t| search::tokens(&normalise(t)))
        .collect();
    let risks: Vec<String> = bans
        .iter()
        .filter(|(_, w)| warnings.iter().any(|t| names(t, w)))
        .map(|(b, _)| (*b).to_owned())
        .collect();
    if !risks.is_empty() {
        marks.push(c7::PlaceMark {
            kind: c7::PlaceMarkKind::BanRisk,
            key: None,
            age_days: None,
            terms: Some(risks),
            other: None,
        });
    }
    marks.extend(others.into_iter().map(|o| c7::PlaceMark {
        kind: c7::PlaceMarkKind::PossibleDuplicate,
        key: None,
        age_days: None,
        terms: None,
        other: Some(o),
    }));
    let maps_url = maps_url(&p);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let distance_m = p.distance.map(|d| d.round() as u64);
    #[allow(clippy::cast_precision_loss)]
    let attrs = p
        .attrs
        .into_iter()
        .map(|a| c7::PlaceAttr {
            stale: a.stale(now),
            age_days: a.age_days(now).unsigned_abs(),
            key: a.key,
            value: a.value,
            checked_at: a.checked_at as f64,
            source: a.source,
            confidence: (a.confidence < 1.0).then_some(a.confidence),
        })
        .collect();
    c7::PlaceHit {
        id: p.id,
        name: p.name,
        status: p.status,
        distance_m,
        maps_url,
        attrs,
        verdict: p.verdict,
        open: open_now.then_some(c7::OpenState::Unknown),
        marks,
    }
}

/// The `maps_url` attribute, else a map search of the location, else of the name and address.
fn maps_url(p: &Candidate) -> String {
    if let Some(a) = p.attr("maps_url") {
        return a.value.clone();
    }
    let query = match p.attr("location").and_then(|a| a.value.split_once(',')) {
        Some((lat, lon)) => format!("{},{}", lat.trim(), lon.trim()),
        None => match p.attr("address") {
            Some(a) => format!("{} {}", p.name, a.value),
            None => p.name.clone(),
        },
    };
    format!(
        "https://www.google.com/maps/search/?api=1&query={}",
        percent_encode(&query)
    )
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `s` with every byte but the unreserved characters of RFC 3986 as `%XX`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
    out
}

/// Whether the list term `term` names `phrase`: the phrase's words appear in it in order.
fn names(term: &[String], phrase: &[String]) -> bool {
    !phrase.is_empty() && term.windows(phrase.len()).any(|w| w == phrase)
}

/// The distinct [`morph`] stems of the tokens of `text`.
fn stems(text: &str) -> BTreeSet<String> {
    translit::tokens(text)
        .iter()
        .map(|t| morph::stem(t))
        .collect()
}

/// Latitude and longitude bounds around `p` within `d` metres; the longitude bounds are absent
/// where the box would reach a pole or cross the antimeridian.
fn bounding_box((lat, lon): (f64, f64), d: f64) -> Bbox {
    let dlat = d / METRES_PER_DEGREE;
    let (lat_lo, lat_hi) = (lat - dlat, lat + dlat);
    let poleward = lat_lo.abs().max(lat_hi.abs());
    let lon_range = (poleward < 90.0)
        .then(|| dlat / poleward.to_radians().cos())
        .filter(|dlon| lon - dlon >= -180.0 && lon + dlon <= 180.0)
        .map(|dlon| (lon - dlon, lon + dlon));
    Bbox {
        lat: (lat_lo, lat_hi),
        lon: lon_range,
    }
}

#[derive(Clone, Copy)]
struct Bbox {
    lat: (f64, f64),
    lon: Option<(f64, f64)>,
}

/// Great-circle metres between two points in degrees.
fn haversine((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

fn profile(c: &Connection) -> Result<Profile, FindError> {
    let mut stmt = c.prepare(
        "SELECT key, value FROM bed_profile WHERE type = ?1 ORDER BY key, set_at, value",
    )?;
    let rows = stmt.query_map([PLACE], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
    let mut out = Profile::default();
    for row in rows {
        let (key, value): (String, String) = row?;
        let corrupt = || FindError::Store(StoreError::Corrupt(format!("bed_profile {key}")));
        match key.as_str() {
            "home" => out.home = Some(point(&value).ok_or_else(corrupt)?),
            "min_rating" => out.min_rating = Some(decimal(&value, true).ok_or_else(corrupt)?),
            "budget_for_two" => out.budget_high = Some(range(&value).ok_or_else(corrupt)?.1),
            "ban" => out.bans.push(value),
            _ => {}
        }
    }
    Ok(out)
}

/// The places of type `place` of `statuses` inside `bbox`, whose rating and price pass their
/// thresholds and whose latest verdict is not -1, each with its attributes by key.
fn query(
    c: &Connection,
    statuses: &[c7::PlaceStatus],
    bbox: Option<Bbox>,
    rating: Option<Threshold>,
    price: Option<Threshold>,
) -> Result<Vec<Candidate>, FindError> {
    let mut stmt = c.prepare(
        "SELECT e.entity, n.name, e.status, e.lat, e.lon,
             v.verdict, v.note, v.said_at, v.source_ref, v.better_than,
             a.key, a.value, a.num, a.checked_at, a.source, a.confidence, a.due_at, p.on_stale
         FROM bed_entity e
         JOIN entities n ON n.id = e.entity
         LEFT JOIN bed_verdict v ON v.seq = (
             SELECT seq FROM bed_verdict WHERE entity = e.entity
             ORDER BY said_at DESC, seq DESC LIMIT 1)
         LEFT JOIN bed_attr a ON a.entity = e.entity
         LEFT JOIN attr_policy p ON p.type = a.type AND p.key = a.key
         WHERE e.type = ?1
           AND e.status IN (?2, ?3, ?4)
           AND (v.verdict IS NULL OR v.verdict <> -1)
           AND (?5 IS NULL OR (e.lat BETWEEN ?5 AND ?6
               AND (?7 IS NULL OR e.lon BETWEEN ?7 AND ?8)))
           AND (?9 IS NULL OR CASE WHEN ?10
               THEN EXISTS (SELECT 1 FROM bed_attr r WHERE r.entity = e.entity
                   AND r.key = 'rating' AND r.num >= ?9)
               ELSE NOT EXISTS (SELECT 1 FROM bed_attr r WHERE r.entity = e.entity
                   AND r.key = 'rating' AND r.num < ?9) END)
           AND (?11 IS NULL OR CASE WHEN ?12
               THEN EXISTS (SELECT 1 FROM bed_attr r WHERE r.entity = e.entity
                   AND r.key = 'price_for_two' AND r.num <= ?11)
               ELSE NOT EXISTS (SELECT 1 FROM bed_attr r WHERE r.entity = e.entity
                   AND r.key = 'price_for_two' AND r.num > ?11) END)
         ORDER BY e.entity, a.key",
    )?;
    let status = |i: usize| statuses.get(i).map(|s| s.as_str());
    let lat = bbox.map(|b| b.lat);
    let lon = bbox.and_then(|b| b.lon);
    let mut rows = stmt.query(params![
        PLACE,
        status(0),
        status(1),
        status(2),
        lat.map(|l| l.0),
        lat.map(|l| l.1),
        lon.map(|l| l.0),
        lon.map(|l| l.1),
        rating.map(|t| t.value),
        rating.is_some_and(|t| t.asked),
        price.map(|t| t.value),
        price.is_some_and(|t| t.asked),
    ])?;
    let mut out: Vec<Candidate> = Vec::new();
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        if out.last().is_none_or(|p| p.id != id) {
            out.push(candidate(r, id)?);
        }
        let Some(key) = r.get::<_, Option<String>>(10)? else {
            continue;
        };
        let on_stale: Option<String> = r.get(17)?;
        let attr = Attr {
            key,
            value: r.get(11)?,
            num: r.get(12)?,
            checked_at: r.get(13)?,
            source: r.get(14)?,
            confidence: r.get(15)?,
            due_at: r.get(16)?,
            demotes: on_stale.as_deref() == Some("demote"),
        };
        if let Some(p) = out.last_mut() {
            p.attrs.push(attr);
        }
    }
    Ok(out)
}

/// The place of a row of [`query`] with its verdict and without its attributes.
fn candidate(r: &rusqlite::Row<'_>, id: String) -> Result<Candidate, FindError> {
    let status: String = r.get(2)?;
    let status = match status.as_str() {
        "candidate" => c7::PlaceStatus::Candidate,
        "active" => c7::PlaceStatus::Active,
        "gone" => c7::PlaceStatus::Gone,
        _ => {
            return Err(FindError::Store(StoreError::Corrupt(format!(
                "bed_entity status {status:?}"
            ))));
        }
    };
    let lat: Option<f64> = r.get(3)?;
    let lon: Option<f64> = r.get(4)?;
    let verdict = match r.get::<_, Option<i64>>(5)? {
        Some(verdict) => {
            let said_at: i64 = r.get(7)?;
            #[allow(clippy::cast_precision_loss)]
            Some(c7::PlaceVerdict {
                verdict,
                note: r.get(6)?,
                said_at: said_at as f64,
                source_ref: r.get(8)?,
                better_than: r.get(9)?,
            })
        }
        None => None,
    };
    Ok(Candidate {
        id,
        name: r.get(1)?,
        status,
        lat_lon: lat.zip(lon),
        verdict,
        attrs: Vec::new(),
        distance: None,
    })
}

/// The other places of the unsettled `bed_dup_pair` rows of `id`.
fn duplicates(c: &Connection, id: &str) -> Result<Vec<String>, FindError> {
    let mut stmt = c.prepare(
        "SELECT CASE WHEN a = ?1 THEN b ELSE a END FROM bed_dup_pair
         WHERE settled_at IS NULL AND (a = ?1 OR b = ?1) ORDER BY 1",
    )?;
    let rows = stmt.query_map([id], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
