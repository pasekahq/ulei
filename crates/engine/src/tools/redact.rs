// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Secret redaction of text the model reads: vendor-prefixed tokens, secret assignments, JSON
//! and YAML fields, auth headers, private keys, connection strings, JWTs, URL credentials, form
//! bodies and phone numbers are masked. Patterns compile on first use.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex_lite::{Captures, Regex};

use super::is_py_space;

/// Which passes [`Redactor::sensitive_text`] runs and how it masks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the four independent keyword flags of the reference's redaction call"
)]
pub struct Flags {
    /// Redact even when the switch is off.
    pub force: bool,
    /// Source code: skip the assignment, JSON-field and YAML passes, keep `{...}` passwords of
    /// connection-string templates.
    pub code_file: bool,
    /// File content: implies `code_file`, and prefixed tokens become the non-reusable
    /// `«redacted:<prefix>…»` instead of a head and tail mask.
    pub file_read: bool,
    /// Also mask credential-named URL parameters and URL userinfo.
    pub redact_url_credentials: bool,
}

/// The `security.redact_secrets` switch; a forced call redacts whatever it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Redactor {
    /// Unforced calls redact.
    pub enabled: bool,
}

impl Redactor {
    /// `text` with every secret shape masked; unchanged when the switch is off and the call is
    /// not forced.
    pub fn sensitive_text(self, text: &str, flags: Flags) -> Cow<'_, str> {
        let mut text = Cow::Borrowed(text);
        if text.is_empty() || !(flags.force || self.enabled) {
            return text;
        }
        let code_file = flags.code_file || flags.file_read;
        let p = patterns();

        if has_known_prefix(&text) {
            let mask = if flags.file_read {
                mask_nonreusable
            } else {
                mask_token
            };
            update(&mut text, |t| mask_control_split_tokens(t, mask));
            update(&mut text, |t| replace_prefixed(t, mask));
        }

        if !code_file {
            redact_assignments(&mut text, p);
        }

        if text.contains("uthorization") || text.contains("UTHORIZATION") {
            update(&mut text, |t| {
                p.auth_header.replace_all(t, |c: &Captures<'_>| {
                    let scheme = c.get(2).map_or("", |m| m.as_str());
                    format!("{}{scheme}{}", &c[1], mask_token(&c[3]))
                })
            });
        }
        if text.contains(':') && maybe_secret_header(&text) {
            update(&mut text, |t| {
                p.secret_header.replace_all(t, |c: &Captures<'_>| {
                    format!("{}{}", &c[1], mask_token(&c[2]))
                })
            });
        }
        if text.contains(':') && maybe_telegram(&text) {
            update(&mut text, |t| {
                p.telegram.replace_all(t, |c: &Captures<'_>| {
                    let bot = c.get(1).map_or("", |m| m.as_str());
                    format!("{bot}{}:***", &c[2])
                })
            });
        }
        if text.contains("BEGIN") && text.contains("-----") {
            update(&mut text, |t| {
                p.private_key.replace_all(t, "[REDACTED PRIVATE KEY]")
            });
        }
        if text.contains("://") {
            update(&mut text, |t| {
                p.db_connstr.replace_all(t, |c: &Captures<'_>| {
                    let password = &c[2];
                    if code_file && password.starts_with('{') && password.ends_with('}') {
                        return c[0].to_owned();
                    }
                    format!("{}***{}", &c[1], &c[3])
                })
            });
            update(&mut text, |t| {
                p.url_bare_token.replace_all(t, |c: &Captures<'_>| {
                    format!("{}{}{}", &c[1], mask_token(&c[2]), &c[3])
                })
            });
        }
        if text.contains("eyJ") {
            update(&mut text, |t| {
                p.jwt.replace_all(t, |c: &Captures<'_>| mask_token(&c[0]))
            });
        }
        if flags.redact_url_credentials {
            redact_url_credentials(&mut text, p);
        }
        if text.contains('&') && text.contains('=') {
            update(&mut text, |t| redact_form_body(t, &p.form_body));
        }
        if text.contains('+') {
            update(&mut text, |t| replace_phones(t, &p.phone));
        }
        text
    }

    /// Command output redacted for `command`: an environment dump or a read of a `.env` file
    /// runs the assignment passes, any other command is treated as source code.
    pub fn terminal_output<'a>(self, output: &'a str, command: &str) -> Cow<'a, str> {
        let code_file = !(is_env_dump_command(command) || command_reads_env_file(command));
        self.sensitive_text(
            output,
            Flags {
                code_file,
                ..Flags::default()
            },
        )
    }
}

/// The assignment, JSON-field and YAML passes, which source code skips.
fn redact_assignments(text: &mut Cow<'_, str>, p: &Patterns) {
    if text.contains('=') {
        update(text, |t| p.env_assign.replace_all(t, redact_quoted));
        if !text.contains("://") {
            update(text, |t| p.env_assign_lower.replace_all(t, redact_quoted));
        }
        if !text.contains("://") && p.cfg_secret_word.is_match(text) {
            update(text, |t| replace_cfg(t, &p.cfg_dotted));
            update(text, |t| replace_cfg(t, &p.cfg_anchored));
        }
    }
    if text.contains(':') && text.contains('"') {
        update(text, |t| {
            p.json_field.replace_all(t, |c: &Captures<'_>| {
                if env_lookup(&c[2]) {
                    return c[0].to_owned();
                }
                format!("{}: \"{}\"", &c[1], mask_token(&c[2]))
            })
        });
    }
    if text.contains(':') && !text.contains("://") {
        update(text, |t| {
            p.yaml_assign.replace_all(t, |c: &Captures<'_>| {
                if env_lookup(&c[3]) || !key_has_secret_keyword(&c[1]) {
                    return c[0].to_owned();
                }
                format!("{}{}{}", &c[1], &c[2], mask_token(&c[3]))
            })
        });
    }
}

/// Credential-named URL parameters and the password, or the whole, of URL userinfo masked.
fn redact_url_credentials(text: &mut Cow<'_, str>, p: &Patterns) {
    update(text, |t| {
        p.strict_param.replace_all(t, |c: &Captures<'_>| {
            if SENSITIVE_QUERY_PARAMS.contains(&canonical_param_name(&c[2]).as_str()) {
                format!("{}{}=***", &c[1], &c[2])
            } else {
                c[0].to_owned()
            }
        })
    });
    update(text, |t| {
        p.strict_userinfo
            .replace_all(t, |c: &Captures<'_>| match c[2].split_once(':') {
                Some((user, _)) => format!("{}{user}:***@", &c[1]),
                None => format!("{}***@", &c[1]),
            })
    });
}

/// Whether `text` holds a literal every secret header name contains, in any case.
fn maybe_secret_header(text: &str) -> bool {
    ["api-key", "apikey", "-token"].iter().any(|name| {
        text.as_bytes()
            .windows(name.len())
            .any(|w| w.eq_ignore_ascii_case(name.as_bytes()))
    })
}

/// Whether `text` holds eight digits followed by `:`, which every bot token has.
fn maybe_telegram(text: &str) -> bool {
    let bytes = text.as_bytes();
    (8..bytes.len()).any(|i| bytes[i] == b':' && bytes[i - 8..i].iter().all(u8::is_ascii_digit))
}

/// Replaces `text` with the result of `pass` when the pass changed it.
fn update(text: &mut Cow<'_, str>, pass: impl FnOnce(&str) -> Cow<'_, str>) {
    let changed = match pass(text) {
        Cow::Owned(changed) => Some(changed),
        Cow::Borrowed(_) => None,
    };
    if let Some(changed) = changed {
        *text = Cow::Owned(changed);
    }
}

/// Characters of Python's `\s` in a character class.
const SPACE: &str = r"\t\n\x0B\x0C\r\x1C-\x1F \x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}";

const ENV_NAMES: &str = "(?:API_?KEY|KEY|TOKEN|SECRET|PASSWORD|PASSWD|PASS|PW|CREDENTIAL|AUTH)";
const ENV_NAMES_LOWER: &str = "(?:key|pass|pw|token|secret|password|passwd|credential|auth)";
const CFG_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential|auth)";
const YAML_NAMES: &str = r"(?:api[ _.\-]?key|token|secret|passwd|password|credential)";
const JSON_NAMES: &str = "(?:api_?[Kk]ey|token|secret|password|access_token|refresh_token|\
                          auth_token|bearer|secret_value|raw_secret|secret_input|key_material)";
const SECRET_HEADER_NAMES: &str =
    "(?:x-api-key|x-goog-api-key|api-key|apikey|x-api-token|x-auth-token|x-access-token)";

/// Query parameter names whose values are credentials, lowercase.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "api_key",
    "apikey",
    "client_secret",
    "password",
    "auth",
    "jwt",
    "session",
    "secret",
    "key",
    "code",
    "signature",
    "x-amz-signature",
];

struct Patterns {
    env_assign: Regex,
    env_assign_lower: Regex,
    cfg_secret_word: Regex,
    /// Key and `=` of a dotted config key; the value is matched by [`cfg_value`].
    cfg_dotted: Regex,
    /// Key and `=` of a bare config key at the start of a line.
    cfg_anchored: Regex,
    key_keyword: Regex,
    json_field: Regex,
    yaml_assign: Regex,
    auth_header: Regex,
    secret_header: Regex,
    telegram: Regex,
    private_key: Regex,
    db_connstr: Regex,
    url_bare_token: Regex,
    jwt: Regex,
    /// A phone number; [`replace_phones`] rejects one followed by a letter or digit.
    phone: Regex,
    form_body: Regex,
    strict_param: Regex,
    strict_userinfo: Regex,
}

/// The compiled patterns, built by the first call that needs them.
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(Patterns::compile)
}

impl Patterns {
    fn compile() -> Self {
        let s = SPACE;
        // A quoted value keeps its quotes outside the match only when it closes in the same
        // run of non-space characters, as the back-reference of the reference pattern does.
        let value = format!(r#"(?:'([^{s}]+)'|"([^{s}]+)"|([^{s}]+))"#);
        Self {
            env_assign: re(&format!(
                "([A-Z0-9_]{{0,50}}{ENV_NAMES}[A-Z0-9_]{{0,50}})[{s}]*=[{s}]*{value}"
            )),
            env_assign_lower: re(&format!(
                "(?i)([a-z0-9_]+_{ENV_NAMES_LOWER})[{s}]*=[{s}]*{value}"
            )),
            cfg_secret_word: re(&format!("(?i){CFG_NAMES}")),
            cfg_dotted: re(&format!(
                r"(?i)([A-Za-z0-9_\-]+\.[A-Za-z0-9_.\-]*{CFG_NAMES}[A-Za-z0-9_.\-]*|[A-Za-z0-9_.\-]*{CFG_NAMES}[A-Za-z0-9_.\-]*\.[A-Za-z0-9_.\-]+)="
            )),
            cfg_anchored: re(&format!(
                r"(?im)(^[ \t]*(?:export[ \t]+)?[A-Za-z0-9_\-]*{CFG_NAMES}[A-Za-z0-9_\-]*)="
            )),
            key_keyword: re(
                r"(?i)(?:api|auth|access|refresh|session|secret)[ _.\\\-]?(?:key|token)|token|secret|passwd|password|pass|pw|credential|auth|key",
            ),
            json_field: re(&format!(r#"(?i)("{JSON_NAMES}")[{s}]*:[{s}]*"([^"]+)""#)),
            yaml_assign: re(&format!(
                r#"(?im)(^[ \t]*[A-Za-z0-9_.\-]*{YAML_NAMES}[A-Za-z0-9_.\-]*)(:[ \t]*)([^{s}&'"][^{s}&]*)"#
            )),
            auth_header: re(&format!(
                r#"(?i)((?:Proxy-)?Authorization:[{s}]*)([A-Za-z][A-Za-z0-9_.+\-]*[{s}]+)?([^{s}"']+)"#
            )),
            secret_header: re(&format!(
                "(?i)({SECRET_HEADER_NAMES}[{s}]*:[{s}]*)([^{s}]+)"
            )),
            telegram: re("(bot)?([0-9]{8,}):([-A-Za-z0-9_]{30,})"),
            private_key: re(
                "(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----",
            ),
            db_connstr: re(&format!(
                r"(?i)((?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp)://[^:{s}]+:)([^@{s}]+)(@)"
            )),
            url_bare_token: re(&format!(
                "(?i)((?:https?|wss?|git|ssh|ftp|ftps|sftp)://)([^{s}:@/]{{8,}})(@[^{s}]+)"
            )),
            jwt: re(r"eyJ[A-Za-z0-9_\-]{10,}(?:\.[A-Za-z0-9_=\-]{4,}){0,2}"),
            phone: re(r"\+[1-9][0-9]{6,14}"),
            form_body: re(&format!(
                r"^[A-Za-z_][A-Za-z0-9_.\-]*=[^&{s}]*(?:&[A-Za-z_][A-Za-z0-9_.\-]*=[^&{s}]*)+$"
            )),
            strict_param: re(&format!(
                r#"([?#&;])([A-Za-z0-9_.~+%\-]+)=([^#&;{s}"'<>]*)"#
            )),
            strict_userinfo: re(&format!("(//)([^/{s}?#@]+)@")),
        }
    }
}

/// A constant pattern; one that does not compile is a defect every redaction test hits.
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("redaction pattern {pattern}: {e}"))
}

/// An assignment match of the [`Patterns::env_assign`] shape: key, then the value in group 2
/// (single-quoted), 3 (double-quoted) or 4 (bare).
fn redact_quoted(c: &Captures<'_>) -> String {
    let (quote, value) = match (c.get(2), c.get(3), c.get(4)) {
        (Some(v), _, _) => ("'", v.as_str()),
        (_, Some(v), _) => ("\"", v.as_str()),
        (_, _, Some(v)) => ("", v.as_str()),
        (None, None, None) => return c[0].to_owned(),
    };
    redact_assignment(&c[1], quote, value).unwrap_or_else(|| c[0].to_owned())
}

/// `name=<quote><mask><quote>`, or `None` for a programmatic lookup or a key without a secret
/// word at a word boundary.
fn redact_assignment(name: &str, quote: &str, value: &str) -> Option<String> {
    if env_lookup(value) || !key_has_secret_keyword(name) {
        return None;
    }
    Some(format!("{name}={quote}{}{quote}", mask_token(value)))
}

/// A value naming an environment lookup in code rather than holding a secret.
fn env_lookup(value: &str) -> bool {
    ["os.getenv", "os.environ", "process.env", "$ENV{"]
        .iter()
        .any(|p| value.starts_with(p))
}

/// Config assignments whose key and `=` `key` matches, each value taken by [`cfg_value`].
fn replace_cfg<'a>(text: &'a str, key: &Regex) -> Cow<'a, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut from = 0;
    while let Some(c) = key.captures_at(text, from) {
        let Some(whole) = c.get(0) else { break };
        let Some((quote, value, end)) = cfg_value(text, whole.end()) else {
            from = whole.end();
            continue;
        };
        if let Some(masked) = redact_assignment(&c[1], quote, value) {
            out.push_str(&text[copied..whole.start()]);
            out.push_str(&masked);
            copied = end;
        }
        from = end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// The value of a config assignment at `at`: the run up to whitespace or `&`, its quotes kept
/// apart when it is quoted on both ends; the quote, the value and the end of the run.
fn cfg_value(text: &str, at: usize) -> Option<(&'static str, &str, usize)> {
    let rest = &text[at..];
    let len = rest
        .find(|c| is_py_space(c) || c == '&')
        .unwrap_or(rest.len());
    let run = &rest[..len];
    if run.is_empty() {
        return None;
    }
    for quote in ["'", "\""] {
        if run.len() >= 3 && run.starts_with(quote) && run.ends_with(quote) {
            return Some((quote, &run[1..len - 1], at + len));
        }
    }
    Some(("", run, at + len))
}

/// Whether a secret word of `key` sits at a word boundary: at an edge, next to a non-letter or
/// at a camel-case transition, a plural `s` included.
fn key_has_secret_keyword(key: &str) -> bool {
    let chars: Vec<char> = key.chars().collect();
    patterns().key_keyword.find_iter(key).any(|m| {
        let start = key[..m.start()].chars().count();
        let end = start + m.as_str().chars().count();
        word_start(&chars, start) && word_end(&chars, end, true)
    })
}

fn word_start(s: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (prev, cur) = (s[i - 1], s[i]);
    !prev.is_alphabetic()
        || (cur.is_uppercase() && prev.is_lowercase())
        || (cur.is_uppercase()
            && prev.is_uppercase()
            && s.get(i + 1).is_some_and(|c| c.is_lowercase()))
}

fn word_end(s: &[char], j: usize, allow_plural: bool) -> bool {
    let Some(&cur) = s.get(j) else { return true };
    if !cur.is_alphabetic() || (cur.is_uppercase() && s[j - 1].is_lowercase()) {
        return true;
    }
    allow_plural && matches!(cur, 's' | 'S') && word_end(s, j + 1, false)
}

/// The text with a form body `k=v&k=v` (one line, nothing else) stripped and its sensitive
/// values masked.
fn redact_form_body<'a>(text: &'a str, form: &Regex) -> Cow<'a, str> {
    if text.contains('\n') || !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let body = text.trim_matches(is_py_space);
    if !form.is_match(body) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(redact_query_string(body))
}

/// `k=v&k=v` with the value of each sensitive key, compared in lowercase, set to `***`.
fn redact_query_string(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if SENSITIVE_QUERY_PARAMS.contains(&key.to_lowercase().as_str()) => {
                format!("{key}=***")
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A URL parameter name percent- and plus-decoded up to three times, case-folded, `-` as `_`.
fn canonical_param_name(name: &str) -> String {
    let mut decoded = name.to_owned();
    for _ in 0..3 {
        let next = unquote_plus(&decoded);
        if next == decoded {
            break;
        }
        decoded = next;
    }
    casefold(&decoded).replace('-', "_")
}

/// `+` as a space and `%XX` escapes decoded, invalid UTF-8 replaced.
fn unquote_plus(text: &str) -> String {
    let text = text.replace('+', " ");
    if !text.contains('%') {
        return text;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.extend(u8::try_from(hi * 16 + lo));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Python's `str.casefold` where it can yield ASCII, lowercase elsewhere.
fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{df}' | '\u{1e9e}' => out.push_str("ss"),
            '\u{17f}' => out.push('s'),
            '\u{fb00}' => out.push_str("ff"),
            '\u{fb01}' => out.push_str("fi"),
            '\u{fb02}' => out.push_str("fl"),
            '\u{fb03}' => out.push_str("ffi"),
            '\u{fb04}' => out.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => out.push_str("st"),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// E.164 numbers keep their first and last digits: two of each up to eight characters, four
/// of each above.
fn replace_phones<'a>(text: &'a str, phone: &Regex) -> Cow<'a, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut from = 0;
    while let Some(m) = phone.find_at(text, from) {
        from = m.end();
        if text[m.end()..]
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        {
            continue;
        }
        let number = m.as_str();
        let keep = if number.len() <= 8 { 2 } else { 4 };
        out.push_str(&text[copied..m.start()]);
        out.push_str(&number[..keep]);
        out.push_str("****");
        out.push_str(&number[number.len() - keep..]);
        copied = m.end();
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// `token` masked for display: values under 18 characters fully, longer ones keep their first
/// six and last four characters.
fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return "***".into();
    }
    mask_secret(token, 6, 4, 18)
}

/// `value` without control characters, as `***` below `floor` characters, else as its first
/// `head` and last `tail` characters around `...`.
fn mask_secret(value: &str, head: usize, tail: usize, floor: usize) -> String {
    let value: Vec<char> = value.chars().filter(|&c| !display_control(c)).collect();
    if value.is_empty() {
        return String::new();
    }
    if value.len() < floor {
        return "***".into();
    }
    let head: String = value[..head].iter().collect();
    let tail: String = value[value.len() - tail..].iter().collect();
    format!("{head}...{tail}")
}

fn display_control(c: char) -> bool {
    matches!(c,
        '\0'..='\x1f' | '\x7f'..='\u{9f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}')
}

/// A prefixed token as `«redacted:<prefix>…»`, which no tool accepts back as a key.
fn mask_nonreusable(token: &str) -> String {
    match PREFIXES.iter().find(|p| token.starts_with(p.label)) {
        Some(p) if !token.is_empty() => format!("«redacted:{}…»", p.label),
        _ => "«redacted-secret»".into(),
    }
}

/// A vendor token shape: `label` is the literal every match contains, `parts` the sequence a
/// match runs through.
struct Prefix {
    label: &'static str,
    parts: &'static [Part],
}

enum Part {
    Lit(&'static str),
    /// At least `.1` and at most `.2` bytes of a class.
    Run(fn(u8) -> bool, usize, usize),
}

/// `label` followed by `min` or more bytes of `class`.
macro_rules! tok {
    ($label:literal, $class:expr, $min:literal) => {
        Prefix {
            label: $label,
            parts: &[Part::Lit($label), Part::Run($class, $min, usize::MAX)],
        }
    };
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn alnum_us(b: u8) -> bool {
    alnum(b) || b == b'_'
}
fn alnum_dash(b: u8) -> bool {
    alnum(b) || b == b'-'
}
/// Also the bytes a token may not touch on either side.
fn alnum_us_dash(b: u8) -> bool {
    alnum(b) || b == b'_' || b == b'-'
}
fn alnum_us_dot_dash(b: u8) -> bool {
    alnum_us_dash(b) || b == b'.'
}
fn alnum_us_eq_dash(b: u8) -> bool {
    alnum_us_dash(b) || b == b'='
}
fn upper_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}
fn digit(b: u8) -> bool {
    b.is_ascii_digit()
}
fn slack_kind(b: u8) -> bool {
    b"baprs".contains(&b)
}

/// Token shapes in the order the reference's alternation tries them.
const PREFIXES: &[Prefix] = &[
    tok!("sk-", alnum_us_dash, 10),
    tok!("ghp_", alnum, 10),
    tok!("github_pat_", alnum_us, 10),
    tok!("gho_", alnum, 10),
    tok!("ghu_", alnum, 10),
    tok!("ghs_", alnum, 10),
    tok!("ghr_", alnum, 10),
    Prefix {
        label: "xapp-",
        parts: &[
            Part::Lit("xapp-"),
            Part::Run(digit, 1, usize::MAX),
            Part::Lit("-"),
            Part::Run(alnum_dash, 10, usize::MAX),
        ],
    },
    Prefix {
        label: "xox",
        parts: &[
            Part::Lit("xox"),
            Part::Run(slack_kind, 1, 1),
            Part::Lit("-"),
            Part::Run(alnum_dash, 10, usize::MAX),
        ],
    },
    tok!("AIza", alnum_us_dash, 30),
    tok!("pplx-", alnum, 10),
    tok!("fal_", alnum_us_dash, 10),
    tok!("fc-", alnum, 10),
    tok!("bb_live_", alnum_us_dash, 10),
    tok!("gAAAA", alnum_us_eq_dash, 20),
    Prefix {
        label: "AKIA",
        parts: &[Part::Lit("AKIA"), Part::Run(upper_digit, 16, 16)],
    },
    tok!("sk_live_", alnum, 10),
    tok!("sk_test_", alnum, 10),
    tok!("rk_live_", alnum, 10),
    Prefix {
        label: "SG",
        parts: &[Part::Lit("SG."), Part::Run(alnum_us_dash, 10, usize::MAX)],
    },
    tok!("hf_", alnum, 10),
    tok!("r8_", alnum, 10),
    tok!("npm_", alnum, 10),
    tok!("pypi-", alnum_us_dash, 10),
    tok!("dop_v1_", alnum, 10),
    tok!("doo_v1_", alnum, 10),
    tok!("am_", alnum_us_dash, 10),
    tok!("sk_", alnum_us, 10),
    tok!("tvly-", alnum, 10),
    tok!("exa_", alnum, 10),
    tok!("gsk_", alnum, 10),
    tok!("syt_", alnum, 10),
    tok!("retaindb_", alnum, 10),
    tok!("hsk-", alnum, 10),
    tok!("mem0_", alnum, 10),
    tok!("brv_", alnum, 10),
    tok!("xai-", alnum, 30),
    tok!("ntn_", alnum, 10),
    tok!("fw-", alnum, 30),
    tok!("fw_", alnum, 30),
    tok!("fpk_", alnum, 30),
    tok!("glpat-", alnum_us_dash, 10),
    tok!("gloas-", alnum_us_dash, 10),
    tok!("gldt-", alnum_us_dash, 10),
    tok!("glrt-", alnum_us_dot_dash, 10),
    tok!("glrtr-", alnum_us_dot_dash, 10),
    tok!("glcbt-", alnum_us_dash, 10),
    tok!("glptt-", alnum_us_dash, 10),
    tok!("glft-", alnum_us_dash, 10),
    tok!("glimt-", alnum_us_dash, 10),
    tok!("glagent-", alnum_us_dash, 10),
    tok!("glsoat-", alnum_us_dash, 10),
    tok!("glffct-", alnum_us_dash, 10),
    tok!("glwt-", alnum_us_dash, 10),
    tok!("GR1348941", alnum_us_dash, 10),
    tok!("pk-lf-", alnum_dash, 8),
];

/// Whether `text` holds the literal of any token shape; no shape matches without one.
fn has_known_prefix(text: &str) -> bool {
    PREFIXES.iter().any(|p| text.contains(p.label))
}

/// The first token at or after `from`: it starts and ends next to a byte outside
/// [`alnum_us_dash`] or at an edge; the start and end offsets.
fn find_token(text: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..text.len())
        .filter(|&at| starts_label(text[at]) && (at == 0 || !alnum_us_dash(text[at - 1])))
        .find_map(|at| {
            PREFIXES
                .iter()
                .find_map(|p| match_parts(text, at, p.parts))
                .map(|end| (at, end))
        })
}

/// The end of `parts` matched at `at`, runs giving back bytes from the longest, as the
/// reference's backtracking does.
fn match_parts(text: &[u8], at: usize, parts: &[Part]) -> Option<usize> {
    let Some((first, rest)) = parts.split_first() else {
        return (!text.get(at).copied().is_some_and(alnum_us_dash)).then_some(at);
    };
    match first {
        Part::Lit(lit) => text[at..]
            .starts_with(lit.as_bytes())
            .then(|| match_parts(text, at + lit.len(), rest))
            .flatten(),
        Part::Run(class, min, max) => {
            let run = text[at..]
                .iter()
                .take(*max)
                .take_while(|&&b| class(b))
                .count();
            (*min..=run)
                .rev()
                .find_map(|n| match_parts(text, at + n, rest))
        }
    }
}

/// Whether some token label starts with `b`.
fn starts_label(b: u8) -> bool {
    PREFIXES.iter().any(|p| p.label.as_bytes()[0] == b)
}

/// Every token in `text` replaced by `mask` of it.
fn replace_prefixed(text: &str, mask: fn(&str) -> String) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    while let Some((start, end)) = find_token(text.as_bytes(), copied) {
        out.push_str(&text[copied..start]);
        out.push_str(&mask(&text[start..end]));
        copied = end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// Control and zero-width characters, which can split a token body.
fn split_control(c: char) -> bool {
    matches!(c,
        '\0'..='\x1f' | '\x7f' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202f}' | '\u{2060}'
        | '\u{feff}')
}

/// Tokens whose body is split by [`split_control`] characters, found with those characters
/// removed and masked over their span in `text`. A span crossing a line break where a part
/// already matches alone, crossing a byte that is no token byte, or running into `=` is left.
fn mask_control_split_tokens(text: &str, mask: fn(&str) -> String) -> Cow<'_, str> {
    if !text.chars().any(split_control) {
        return Cow::Borrowed(text);
    }
    let mut stripped = String::with_capacity(text.len());
    // For each removed character: the offset in `stripped` it was removed at and the bytes
    // removed up to and including it.
    let mut removed: Vec<(usize, usize)> = Vec::new();
    for c in text.chars() {
        if split_control(c) {
            let before = removed.last().map_or(0, |&(_, n)| n);
            removed.push((stripped.len(), before + c.len_utf8()));
        } else {
            stripped.push(c);
        }
    }
    let origin = |at: usize| {
        let i = removed.partition_point(|&(r, _)| r <= at);
        at + i.checked_sub(1).map_or(0, |i| removed[i].1)
    };
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some((start, end)) = find_token(stripped.as_bytes(), from) {
        from = end;
        let (left, right) = (origin(start), origin(end - 1) + 1);
        let span = &text[left..right];
        if span.contains(['\n', '\r']) && find_token(span.as_bytes(), 0).is_some() {
            continue;
        }
        let token_bytes = span
            .chars()
            .all(|c| split_control(c) || (c.is_ascii() && alnum_us_dot_dash(c as u8)));
        if token_bytes && text.as_bytes().get(right) != Some(&b'=') {
            spans.push((left, right, mask(&stripped[start..end])));
        }
    }
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (left, right, masked) in spans {
        out.push_str(&text[copied..left]);
        out.push_str(&masked);
        copied = right;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// Shell words of an environment dump.
const ENV_DUMP_COMMANDS: &[&str] = &["env", "printenv", "set", "export", "declare"];
/// Commands that print the files they name.
const FILE_READ_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "type", "bat", "less", "more", "nl", "zcat", "tac", "view", "batcat",
];
/// Basenames of files holding secrets only, lowercase.
const ENV_FILE_BASENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.staging",
    ".envrc",
];

/// The pieces of a command between `|`, `;` and `&`, stripped, empty ones left out.
fn segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(['|', ';', '&'])
        .map(|seg| seg.trim_matches(is_py_space))
        .filter(|seg| !seg.is_empty())
}

/// Whether a piece of `command` starts with an environment dump word.
fn is_env_dump_command(command: &str) -> bool {
    segments(command).any(|seg| {
        let first = match shell_words(seg) {
            Some(words) => words.into_iter().next(),
            None => seg
                .split(is_py_space)
                .find(|w| !w.is_empty())
                .map(str::to_owned),
        };
        first.is_some_and(|w| ENV_DUMP_COMMANDS.contains(&w.as_str()))
    })
}

/// Whether a piece of `command` prints a `.env`-style file with a file-reading command.
fn command_reads_env_file(command: &str) -> bool {
    segments(command).any(|seg| {
        let mut words = seg.split(is_py_space).filter(|w| !w.is_empty());
        if !words
            .next()
            .is_some_and(|w| FILE_READ_COMMANDS.contains(&w))
        {
            return false;
        }
        words.filter(|arg| !arg.starts_with('-')).any(|arg| {
            let arg = arg.trim_matches(['"', '\'']);
            let base = arg.rsplit('/').next().unwrap_or(arg);
            let base = base.rsplit('\\').next().unwrap_or(base);
            ENV_FILE_BASENAMES.contains(&base.to_lowercase().as_str())
        })
    })
}

/// The words of `text` split as a POSIX shell does, quotes and backslashes removed; `None` on
/// an unclosed quote or a trailing backslash.
fn shell_words(text: &str) -> Option<Vec<String>> {
    const BLANK: [char; 4] = [' ', '\t', '\r', '\n'];
    let mut words = Vec::new();
    let mut chars = text.chars();
    loop {
        let mut c = loop {
            match chars.next() {
                None => return Some(words),
                Some(c) if BLANK.contains(&c) => {}
                Some(c) => break c,
            }
        };
        let mut word = String::new();
        loop {
            match c {
                '\\' => word.push(chars.next()?),
                '\'' => loop {
                    match chars.next()? {
                        '\'' => break,
                        q => word.push(q),
                    }
                },
                '"' => loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let escaped = chars.next()?;
                            if escaped != '"' && escaped != '\\' {
                                word.push('\\');
                            }
                            word.push(escaped);
                        }
                        q => word.push(q),
                    }
                },
                c if BLANK.contains(&c) => break,
                c => word.push(c),
            }
            let Some(next) = chars.next() else {
                words.push(word);
                return Some(words);
            };
            c = next;
        }
        words.push(word);
    }
}
