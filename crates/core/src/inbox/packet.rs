//! C1 packet rules of ADR-0007 section 5: canonical JSON, digest, MAC and the publish checks.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use ulei_contracts::generated::c1::{EventTrust, Kind};

use super::Error;

pub(super) type PacketMac = Hmac<Sha256>;

const FIELDS: [&str; 7] = [
    "id",
    "kind",
    "mac",
    "payload",
    "source_ts",
    "trust",
    "version",
];
const MAX_ID: usize = 96;
const MAX_PAYLOAD: usize = 65_536;
const FUTURE_SECONDS: f64 = 300.0;
/// Python `repr` prints a float in fixed notation, as `serde_json` does, only in this range.
const FIXED_MIN: f64 = 1e-4;
const FIXED_END: f64 = 1e16;

/// Why `publish` refused a packet with `invalid_packet`; variants follow the check order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// The packet text is not JSON `serde_json` can hold (`NaN`, `Infinity`, `1e999`).
    Json,
    /// Not an object of exactly the seven C1 fields.
    Fields,
    /// A float Python prints with an exponent, so its canonical bytes differ (rule 4).
    ExponentFloat,
    /// `version` is not the integer 1.
    Version,
    /// `kind` is not a C1 `Kind`.
    Kind,
    /// `trust` is not `untrusted-source-data`.
    Trust,
    /// `id` does not match `[a-zA-Z0-9:_-]{1,96}`.
    Id,
    /// `payload` is not a string, is blank or exceeds 65,536 UTF-8 bytes.
    Payload,
    /// `source_ts` is not a number, or lies outside 0 to now + 300 s.
    SourceTs,
}

/// A packet that passed every publish check, with the bytes the `events` row stores.
pub(super) struct Checked<'a> {
    pub(super) id: &'a str,
    pub(super) digest: String,
    pub(super) stored: Vec<u8>,
}

/// Python `json.dumps(v, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()`
/// (rules 1-3); refuses a float whose Python form has an exponent (rule 4).
///
/// # Errors
/// [`Error::InvalidPacket`] with [`Invalid::ExponentFloat`].
pub fn canonical(value: &Value) -> Result<Vec<u8>, Error> {
    Ok(sorted(value)?.to_string().into_bytes())
}

/// A copy with object keys in code point order: a workspace build may unify `serde_json`
/// with `preserve_order`, and then `Map` keeps insertion order.
fn sorted(value: &Value) -> Result<Value, Error> {
    Ok(match value {
        Value::Number(n) if n.is_f64() && !n.as_f64().is_some_and(fixed_notation) => {
            return Err(Error::InvalidPacket(Invalid::ExponentFloat));
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect::<Result<_, _>>()?),
        Value::Object(fields) => {
            let mut entries: Vec<_> = fields.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (key, item) in entries {
                out.insert(key.clone(), sorted(item)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// True when Python `repr` and `serde_json` print `x` alike, in fixed notation (rule 4).
pub(crate) fn fixed_notation(x: f64) -> bool {
    x == 0.0 || (FIXED_MIN..FIXED_END).contains(&x.abs())
}

/// Runs the publish checks in the order of the Python inbox: fields, MAC, then values.
pub(super) fn check<'a>(
    packet: &'a Value,
    mac: &PacketMac,
    now: f64,
) -> Result<Checked<'a>, Error> {
    let invalid = Error::InvalidPacket;
    seven_fields(packet).ok_or(invalid(Invalid::Fields))?;
    let signed = signed_bytes(packet)?;
    verify(mac, &signed, &packet["mac"])?;
    if packet["version"].as_u64() != Some(1) {
        return Err(invalid(Invalid::Version));
    }
    if Kind::deserialize(&packet["kind"]).is_err() {
        return Err(invalid(Invalid::Kind));
    }
    if packet["trust"].as_str() != Some(EventTrust::UntrustedSourceData.as_str()) {
        return Err(invalid(Invalid::Trust));
    }
    let id = packet["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or(invalid(Invalid::Id))?;
    if !packet["payload"].as_str().is_some_and(valid_payload) {
        return Err(invalid(Invalid::Payload));
    }
    if !valid_source_ts(&packet["source_ts"], now + FUTURE_SECONDS) {
        return Err(invalid(Invalid::SourceTs));
    }
    Ok(Checked {
        id,
        digest: digest_hex(&signed),
        stored: canonical(packet)?,
    })
}

/// The bytes under the digest and the MAC (rule 5): `canonical(packet without mac)`, or
/// `canonical(packet)` when it is not an object.
pub(super) fn signed_bytes(packet: &Value) -> Result<Vec<u8>, Error> {
    match packet {
        Value::Object(fields) => {
            let mut body = fields.clone();
            body.remove("mac");
            canonical(&Value::Object(body))
        }
        other => canonical(other),
    }
}

/// The `digest` column of `signed`: its lowercase hex SHA-256.
pub(super) fn digest_hex(signed: &[u8]) -> String {
    lower_hex(&Sha256::digest(signed))
}

fn seven_fields(packet: &Value) -> Option<&Map<String, Value>> {
    packet
        .as_object()
        .filter(|m| m.len() == FIELDS.len() && FIELDS.iter().all(|f| m.contains_key(*f)))
}

/// Constant-time check of the `mac` field `given` over `signed`.
pub(super) fn verify(mac: &PacketMac, signed: &[u8], given: &Value) -> Result<(), Error> {
    let tag = given
        .as_str()
        .and_then(parse_lower_hex_tag)
        .ok_or(Error::Unauthenticated)?;
    let mut mac = mac.clone();
    mac.update(signed);
    mac.verify_slice(&tag).map_err(|_| Error::Unauthenticated)
}

/// C1 `EventId`: `^[a-zA-Z0-9:_-]{1,96}$`.
pub(crate) fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
}

fn valid_payload(payload: &str) -> bool {
    payload.len() <= MAX_PAYLOAD && payload.chars().any(|c| !python_space(c))
}

/// Python `str.isspace`: Unicode `White_Space` plus the separators U+001C to U+001F.
fn python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn valid_source_ts(ts: &Value, bound: f64) -> bool {
    let Value::Number(n) = ts else {
        return false;
    };
    if let Some(int) = n.as_u64() {
        return int_not_after(int, bound);
    }
    if n.is_i64() {
        return false;
    }
    n.as_f64().is_some_and(|x| x >= 0.0 && x <= bound)
}

/// Exact `int <= bound` as Python compares them; `int as f64` would round above 2^53.
fn int_not_after(int: u64, bound: f64) -> bool {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative bound saturates at u64::MAX, which every u64 is under"
    )]
    let floor = bound.floor() as u64;
    bound >= 0.0 && int <= floor
}

/// True when `text` is exactly `len` lowercase hex digits.
pub(crate) fn lower_hex_of_len(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| lower_nibble(b).is_some())
}

fn parse_lower_hex_tag(text: &str) -> Option<[u8; 32]> {
    let text = text.as_bytes();
    if text.len() != 64 {
        return None;
    }
    let mut tag = [0u8; 32];
    for (byte, pair) in tag.iter_mut().zip(text.as_chunks::<2>().0) {
        *byte = (lower_nibble(pair[0])? << 4) | lower_nibble(pair[1])?;
    }
    Some(tag)
}

fn lower_nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

pub(super) fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}
