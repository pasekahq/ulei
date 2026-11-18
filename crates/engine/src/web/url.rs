// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! URLs as the reference reads them before a backend call: Python's `urlsplit`, the ASCII
//! normalisation of a tool URL, the credential-like query parameter check and the SSRF filter.

use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs as _};

use crate::tools::redact::unquote;
use crate::tools::{is_py_space, py_strip};

/// Query parameter names that carry a credential.
const SENSITIVE_QUERY_PARAM_NAMES: [&str; 20] = [
    "access_token",
    "api_key",
    "apikey",
    "auth_token",
    "authorization",
    "awsaccesskeyid",
    "client_secret",
    "credential",
    "credentials",
    "jwt",
    "password",
    "passwd",
    "secret",
    "session_id",
    "signature",
    "token",
    "x_amz_security_token",
    "x_amz_signature",
    "x-amz-security-token",
    "x-amz-signature",
];

/// Cloud metadata host names, refused whatever they resolve to.
const BLOCKED_HOSTNAMES: [&str; 2] = ["metadata.google.internal", "metadata.goog"];

/// A URL in the five parts of Python's `SplitResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split {
    pub(crate) scheme: String,
    pub(crate) netloc: String,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) fragment: String,
}

impl Split {
    /// Python's `SplitResult.hostname`: the host lowercased, without brackets, port or user
    /// info; `None` when empty.
    pub(crate) fn hostname(&self) -> Option<String> {
        let hostinfo = self
            .netloc
            .rsplit_once('@')
            .map_or(&*self.netloc, |(_, h)| h);
        let host = match hostinfo.split_once('[') {
            Some((_, bracketed)) => bracketed.split_once(']').map_or(bracketed, |(h, _)| h),
            None => hostinfo.split_once(':').map_or(hostinfo, |(h, _)| h),
        };
        if host.is_empty() {
            return None;
        }
        Some(match host.split_once('%') {
            Some((address, zone)) => format!("{}%{zone}", address.to_lowercase()),
            None => host.to_lowercase(),
        })
    }
}

/// Python's `urllib.parse.urlsplit`.
///
/// # Errors
/// The text of the `ValueError` Python raises for a malformed bracketed host.
pub(crate) fn urlsplit(url: &str) -> Result<Split, String> {
    let url: String = url
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = url.as_str();
    let mut scheme = String::new();
    if let Some(colon) = rest.find(':')
        && colon > 0
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest[..colon]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        scheme = rest[..colon].to_ascii_lowercase();
        rest = &rest[colon + 1..];
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
        if netloc.contains('[') != netloc.contains(']') {
            return Err("Invalid IPv6 URL".into());
        }
        if netloc.contains('[') {
            check_bracketed_netloc(netloc)?;
        }
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Ok(Split {
        scheme,
        netloc: netloc.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        fragment: fragment.to_owned(),
    })
}

fn check_bracketed_netloc(netloc: &str) -> Result<(), String> {
    let host_and_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let host = match host_and_port.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err("Invalid IPv6 URL".into());
            }
            let (host, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !port.is_empty() && !port.starts_with(':') {
                return Err("Invalid IPv6 URL".into());
            }
            host
        }
        None => host_and_port
            .split_once(':')
            .map_or(host_and_port, |(h, _)| h),
    };
    if let Some(future) = host.strip_prefix('v') {
        let valid = future.split_once('.').is_some_and(|(hex, rest)| {
            !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !rest.is_empty()
        });
        return if valid {
            Ok(())
        } else {
            Err("IPvFuture address is invalid".into())
        };
    }
    match parse_ip(host) {
        Some(IpAddr::V6(_)) => Ok(()),
        Some(IpAddr::V4(_)) => Err("An IPv4 address cannot be in brackets".into()),
        None => Err(format!(
            "{} does not appear to be an IPv4 or IPv6 address",
            crate::tools::repr(host)
        )),
    }
}

/// An IP literal as Python's `ipaddress.ip_address` reads it, a `%zone` suffix allowed on IPv6.
fn parse_ip(text: &str) -> Option<IpAddr> {
    if let Ok(v4) = text.parse::<Ipv4Addr>() {
        return Some(IpAddr::V4(v4));
    }
    let address = text.split_once('%').map_or(text, |(a, _)| a);
    address.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

/// Python's `urlunsplit` of the parts.
fn urlunsplit(scheme: &str, netloc: &str, path: &str, query: &str, fragment: &str) -> String {
    let mut url = String::new();
    if !scheme.is_empty() {
        url.push_str(scheme);
        url.push(':');
    }
    // Every scheme that reaches here (http, https) takes a netloc.
    url.push_str("//");
    url.push_str(netloc);
    if !path.is_empty() && !path.starts_with('/') {
        url.push('/');
    }
    url.push_str(path);
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    if !fragment.is_empty() {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

/// The reference's `normalize_url_for_request`: an http(s) URL with an IDNA host and the path,
/// query and fragment percent-encoded; any other text stripped and otherwise unchanged.
pub(crate) fn normalize_url_for_request(url: &str) -> String {
    let raw = py_strip(url);
    if raw.is_empty() {
        return String::new();
    }
    let raw = without_space_after_scheme(raw);
    let Ok(parsed) = urlsplit(&raw) else {
        return raw;
    };
    if parsed.scheme != "http" && parsed.scheme != "https" {
        return raw;
    }
    let mut netloc = parsed.netloc.clone();
    if let Some(hostname) = parsed.hostname() {
        let ascii = idna(&hostname).unwrap_or_else(|| hostname.clone());
        if ascii != hostname {
            netloc = netloc.replacen(&hostname, &ascii, 1);
        }
    }
    let path = quote(&parsed.path, "/%:@!$&'()*+,;=");
    let query = quote(&parsed.query, "/%:@!$&'()*+,;=?");
    let fragment = quote(&parsed.fragment, "/%:@!$&'()*+,;=?");
    urlunsplit(&parsed.scheme, &netloc, &path, &query, &fragment)
}

/// `raw` without the whitespace a model puts between `scheme://` and the authority.
fn without_space_after_scheme(raw: &str) -> String {
    let scheme_end = raw
        .bytes()
        .enumerate()
        .take_while(|&(i, b)| {
            if i == 0 {
                b.is_ascii_alphabetic()
            } else {
                b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')
            }
        })
        .count();
    if scheme_end > 0
        && let Some(after) = raw[scheme_end..].strip_prefix("://")
    {
        let trimmed = after.trim_start_matches(is_py_space);
        if trimmed.len() < after.len() {
            return format!("{}://{trimmed}", &raw[..scheme_end]);
        }
    }
    raw.to_owned()
}

/// Python's `urllib.parse.quote` of `text` with the extra `safe` characters.
fn quote(text: &str, safe: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || safe.as_bytes().contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Python's `str.encode("idna")` of a host name: ASCII labels unchanged, every other label
/// lowercased and Punycode-encoded behind `xn--`; `None` where Python raises.
fn idna(host: &str) -> Option<String> {
    let labels: Vec<&str> = host
        .split(['.', '\u{3002}', '\u{ff0e}', '\u{ff61}'])
        .collect();
    let mut out = Vec::with_capacity(labels.len());
    for (i, label) in labels.iter().enumerate() {
        let last = i + 1 == labels.len();
        let encoded = if label.is_ascii() {
            (*label).to_owned()
        } else {
            let mapped: String = label
                .chars()
                .filter(|c| !matches!(c, '\u{ad}' | '\u{200b}'..='\u{200d}' | '\u{feff}'))
                .flat_map(char::to_lowercase)
                .collect();
            if mapped.is_ascii() {
                mapped
            } else {
                format!("xn--{}", punycode(&mapped)?)
            }
        };
        if encoded.len() >= 64 || (encoded.is_empty() && !last) {
            return None;
        }
        out.push(encoded);
    }
    Some(out.join("."))
}

/// RFC 3492 Punycode of `label`.
fn punycode(label: &str) -> Option<String> {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    let digit = |d: u32| char::from_digit(d, 36);
    let adapt = |mut delta: u32, points: u32, first: bool| {
        delta /= if first { 700 } else { 2 };
        delta += delta / points;
        let mut shift = 0;
        while delta > ((BASE - T_MIN) * T_MAX) / 2 {
            delta /= BASE - T_MIN;
            shift += BASE;
        }
        shift + (BASE - T_MIN + 1) * delta / (delta + 38)
    };
    let input: Vec<u32> = label.chars().map(u32::from).collect();
    let mut out: String = label.chars().filter(char::is_ascii).collect();
    let basic = u32::try_from(out.len()).ok()?;
    if basic > 0 {
        out.push('-');
    }
    let total = u32::try_from(input.len()).ok()?;
    let (mut code, mut delta, mut bias, mut handled) = (128u32, 0u32, 72u32, basic);
    while handled < total {
        let smallest = input.iter().copied().filter(|&c| c >= code).min()?;
        delta = delta.checked_add((smallest - code).checked_mul(handled + 1)?)?;
        code = smallest;
        for &c in &input {
            if c < code {
                delta = delta.checked_add(1)?;
            }
            if c == code {
                let mut rest = delta;
                let mut step = BASE;
                loop {
                    let threshold = if step <= bias {
                        T_MIN
                    } else if step >= bias + T_MAX {
                        T_MAX
                    } else {
                        step - bias
                    };
                    if rest < threshold {
                        break;
                    }
                    out.push(digit(threshold + (rest - threshold) % (BASE - threshold))?);
                    rest = (rest - threshold) / (BASE - threshold);
                    step += BASE;
                }
                out.push(digit(rest)?);
                bias = adapt(delta, handled + 1, handled == basic);
                delta = 0;
                handled += 1;
            }
        }
        delta += 1;
        code += 1;
    }
    Some(out)
}

/// The reference's `sensitive_query_param_name`: the first query parameter with a value whose
/// decoded name is credential-like.
pub(crate) fn sensitive_query_param_name(url: &str) -> Option<String> {
    if !url.contains('?') {
        return None;
    }
    let parsed = urlsplit(py_strip(url)).ok()?;
    if (parsed.scheme != "http" && parsed.scheme != "https") || parsed.query.is_empty() {
        return None;
    }
    parsed
        .query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| {
            (
                unquote(&key.replace('+', " ")),
                unquote(&value.replace('+', " ")),
            )
        })
        .find(|(key, value)| {
            !value.is_empty()
                && SENSITIVE_QUERY_PARAM_NAMES.contains(&unquote(key).to_lowercase().as_str())
        })
        .map(|(key, _)| key)
}

/// The host's addresses as the system resolver gives them.
pub(crate) fn resolve(host: &str) -> io::Result<Vec<IpAddr>> {
    Ok((host, 0).to_socket_addrs()?.map(|a| a.ip()).collect())
}

/// The reference's `is_safe_url`: an http(s) URL whose host is not a metadata name and every
/// address `resolve` gives for it is public; a URL that does not parse or a host that does
/// not resolve is unsafe.
pub(crate) fn is_safe_url(url: &str, resolve: &dyn Fn(&str) -> io::Result<Vec<IpAddr>>) -> bool {
    let Ok(parsed) = urlsplit(url) else {
        return false;
    };
    if parsed.scheme != "http" && parsed.scheme != "https" {
        return false;
    }
    let Some(hostname) = parsed.hostname() else {
        return false;
    };
    let hostname = py_strip(&hostname).to_lowercase();
    let hostname = hostname.trim_end_matches('.');
    if hostname.is_empty() || BLOCKED_HOSTNAMES.contains(&hostname) {
        return false;
    }
    match resolve(hostname) {
        Ok(addresses) => !addresses.into_iter().any(is_blocked_ip),
        Err(_) => false,
    }
}

/// The reference's `_is_blocked_ip` with Python's `ipaddress` ranges; it covers the metadata
/// addresses and link-local range refused even when private addresses are allowed.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => blocked_v4(v4),
            None => blocked_v6(v6),
        },
    }
}

/// Python's `ipaddress` private IPv4 networks, as (address, prefix length).
const V4_PRIVATE: [([u8; 4], u32); 14] = [
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 0, 170], 31),
    ([192, 0, 2, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([240, 0, 0, 0], 4),
    ([255, 255, 255, 255], 32),
];

/// Python's `ipaddress` private IPv6 networks.
const V6_PRIVATE: [(&str, u32); 11] = [
    ("::1", 128),
    ("::", 128),
    ("::ffff:0:0", 96),
    ("64:ff9b:1::", 48),
    ("100::", 64),
    ("2001::", 23),
    ("2001:db8::", 32),
    ("2002::", 16),
    ("3fff::", 20),
    ("fc00::", 7),
    ("fe80::", 10),
];

/// IPv6 networks inside [`V6_PRIVATE`] that Python does not count as private.
const V6_PRIVATE_EXCEPTIONS: [(&str, u32); 6] = [
    ("2001:1::1", 128),
    ("2001:1::2", 128),
    ("2001:3::", 32),
    ("2001:4:112::", 48),
    ("2001:20::", 28),
    ("2001:30::", 28),
];

/// Python's `ipaddress` reserved IPv6 networks.
const V6_RESERVED: [(&str, u32); 15] = [
    ("::", 8),
    ("100::", 8),
    ("200::", 7),
    ("400::", 6),
    ("800::", 5),
    ("1000::", 4),
    ("4000::", 3),
    ("6000::", 3),
    ("8000::", 3),
    ("a000::", 3),
    ("c000::", 3),
    ("e000::", 4),
    ("f000::", 5),
    ("f800::", 6),
    ("fe00::", 9),
];

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    let within = |net: [u8; 4], prefix: u32| {
        let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
        n & mask == u32::from(Ipv4Addr::from(net)) & mask
    };
    let private = V4_PRIVATE.iter().any(|&(net, prefix)| within(net, prefix))
        && !within([192, 0, 0, 9], 32)
        && !within([192, 0, 0, 10], 32);
    let reserved = within([240, 0, 0, 0], 4);
    let cgnat = within([100, 64, 0, 0], 10);
    private
        || ip.is_loopback()
        || ip.is_link_local()
        || reserved
        || ip.is_multicast()
        || ip.is_unspecified()
        || cgnat
}

fn blocked_v6(ip: Ipv6Addr) -> bool {
    let n = u128::from(ip);
    let within = |net: &str, prefix: u32| {
        let net = net.parse::<Ipv6Addr>().map_or(0, u128::from);
        let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
        n & mask == net & mask
    };
    let private = V6_PRIVATE.iter().any(|&(net, prefix)| within(net, prefix))
        && !V6_PRIVATE_EXCEPTIONS
            .iter()
            .any(|&(net, prefix)| within(net, prefix));
    private
        || ip.is_loopback()
        || within("fe80::", 10)
        || V6_RESERVED.iter().any(|&(net, prefix)| within(net, prefix))
        || ip.is_multicast()
        || ip.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(addresses: &'static [&'static str]) -> impl Fn(&str) -> io::Result<Vec<IpAddr>> {
        move |_| Ok(addresses.iter().map(|a| a.parse().unwrap()).collect())
    }

    #[test]
    fn a_host_whose_dns_answer_is_private_or_mapped_private_is_unsafe() {
        for private in ["10.1.2.3", "192.168.0.7", "::ffff:172.16.0.1", "fd00::5"] {
            let addresses: &'static [&'static str] = Box::leak(Box::new([private]));
            assert!(
                !is_safe_url("https://intranet.example/", &answer(addresses)),
                "{private}"
            );
        }
        assert!(!is_safe_url(
            "https://mixed.example/",
            &answer(&["93.184.215.14", "10.0.0.1"])
        ));
        assert!(is_safe_url(
            "https://public.example/",
            &answer(&["93.184.215.14", "2606:2800:21f:cb07:6820:80da:af6b:8b2c"])
        ));
    }

    #[test]
    fn a_host_that_does_not_resolve_is_unsafe() {
        let fail = |_: &str| Err(io::Error::other("no such host"));
        assert!(!is_safe_url("https://nowhere.example/", &fail));
    }
}
