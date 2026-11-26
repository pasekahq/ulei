//! Detectors of secret values in text. The edge refuses a candidate that [`looks_secret`], the
//! store refuses a secret-pointer fact whose text [`may_carry_secret`] (ADR-0046 section 2,
//! ADR-0044 section 5) and the places bed a text that [`may_carry_credential`] (ADR-0080
//! section 1); all three run the same detectors.

/// Whether `text` looks like it carries a secret value: a payment card number (Luhn), an IBAN
/// (mod 97), a national id (by checksum, by shape, or after its key), a phone number, a value
/// after a secret key, a well-known API key prefix or a long random token. Errs towards yes.
pub fn looks_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    digit_runs(text, " -", 1)
        .iter()
        .any(|r| is_card_number(r) || is_checked_id(r))
        || has_iban(text)
        || has_phone(text)
        || has_assignment(&lower)
        || has_keyed_id(&lower)
        || text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| "\"'`()[]{}<>,;".contains(c));
            has_key_prefix(w) || is_random_token(w)
        })
        || text.contains("-----BEGIN")
}

/// Whether the text of a secret-pointer fact may carry the value: anything [`looks_secret`]
/// finds, four or more digits in a row written in digits or in words, or a token (see
/// [`has_token`]). The check is a guard, not a classifier.
pub(crate) fn may_carry_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    looks_secret(text)
        || digit_runs(text, "", 0)
            .iter()
            .any(|r| r.digits().len() >= 4)
        || (0..words.len()).any(|i| spelled_digits(&words[i..]) >= 4)
        || has_token(text)
}

/// Whether a text of the places bed may carry a credential: a payment card number not written
/// after `+`, an IBAN, a national id after its key, a value after a secret key, a private key
/// block, or a word that is a well-known API key prefix, a long random token or a [`has_token`]
/// run; in a URL only the pieces between `/`, `?`, `&`, `#`, `=`, `;`, `!` and `:` are checked
/// for a key prefix or a long random token. Phone numbers, postcodes and plain URLs pass.
pub(crate) fn may_carry_credential(text: &str) -> bool {
    let lower = text.to_lowercase();
    digit_runs(text, " -", 1)
        .iter()
        .any(|r| !r.before.ends_with('+') && is_card_number(r))
        || has_iban(text)
        || has_assignment(&lower)
        || has_keyed_id(&lower)
        || text.contains("-----BEGIN")
        || text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| "\"'`()[]{}<>,;".contains(c));
            match w.split_once("://") {
                Some((_, rest)) => rest
                    .split(['/', '?', '&', '#', '=', ';', '!', ':'])
                    .any(|p| has_key_prefix(p) || is_random_token(p)),
                None => has_key_prefix(w) || is_random_token(w) || has_token(w),
            }
        })
}

/// Whether `s` holds a token: a run of 16 or more letters and digits mixing both.
pub(crate) fn has_token(s: &str) -> bool {
    s.split(|c: char| !c.is_alphanumeric()).any(|w| {
        w.chars().count() >= 16
            && w.chars().any(|c| c.is_ascii_digit())
            && w.chars().any(char::is_alphabetic)
    })
}

/// Groups of ASCII digits joined by separators.
struct Run<'a> {
    /// The text before the first digit.
    before: &'a str,
    /// Each group with the separator in front of it, empty for the first.
    groups: Vec<(&'a str, &'a str)>,
}

impl Run<'_> {
    fn digits(&self) -> Vec<u8> {
        self.groups
            .iter()
            .flat_map(|(_, g)| g.bytes())
            .map(|b| b - b'0')
            .collect()
    }

    fn lens(&self) -> Vec<usize> {
        self.groups.iter().map(|(_, g)| g.len()).collect()
    }
}

/// The runs of `text` in which groups are joined by at most `max_sep` characters of `seps`
/// (ASCII); "+7 (912) 345-67-89" is one run with `" -()"` and 2.
fn digit_runs<'a>(text: &'a str, seps: &str, max_sep: usize) -> Vec<Run<'a>> {
    let b = text.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut groups = Vec::new();
        let mut sep = i;
        loop {
            let g = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            groups.push((&text[sep..g], &text[g..i]));
            let mut j = i;
            while j < b.len() && j - i < max_sep && seps.as_bytes().contains(&b[j]) {
                j += 1;
            }
            if j == i || j == b.len() || !b[j].is_ascii_digit() {
                break;
            }
            sep = i;
            i = j;
        }
        runs.push(Run {
            before: &text[..start],
            groups,
        });
    }
    runs
}

fn is_card_number(r: &Run<'_>) -> bool {
    let d = r.digits();
    (13..=19).contains(&d.len()) && luhn(&d)
}

fn luhn(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &v)| {
            let v = u32::from(v);
            if i % 2 == 1 {
                let x = v * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                v
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// A Russian SNILS or personal INN passing its checksum, or a US SSN written `AAA-GG-SSSS`
/// outside the never-issued ranges.
fn is_checked_id(r: &Run<'_>) -> bool {
    let d = r.digits();
    match d.len() {
        11 => snils_ok(&d),
        12 => inn_ok(&d),
        9 => is_ssn(r),
        _ => false,
    }
}

fn snils_ok(d: &[u8]) -> bool {
    let sum: u32 = d[..9]
        .iter()
        .zip((1..=9).rev())
        .map(|(&v, w)| u32::from(v) * w)
        .sum();
    sum % 101 % 100 == u32::from(d[9]) * 10 + u32::from(d[10])
}

fn inn_ok(d: &[u8]) -> bool {
    const WEIGHTS: [u32; 11] = [3, 7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
    let check = |n: usize| {
        let sum: u32 = WEIGHTS[11 - n..]
            .iter()
            .zip(d)
            .map(|(w, &v)| w * u32::from(v))
            .sum();
        sum % 11 % 10 == u32::from(d[n])
    };
    check(10) && check(11)
}

fn is_ssn(r: &Run<'_>) -> bool {
    let [("", area), ("-", group), ("-", serial)] = r.groups[..] else {
        return false;
    };
    r.lens() == [3, 2, 4]
        && !matches!(area, "000" | "666")
        && !area.starts_with('9')
        && group != "00"
        && serial != "0000"
}

/// A phone number of 10 to 15 digits: after `+`, after the Russian trunk prefix `8` (eleven
/// digits), with an area code in brackets, or in dashed groups (`912-345-67-89`).
fn has_phone(text: &str) -> bool {
    digit_runs(text, " -()", 2).iter().any(|r| {
        let lens = r.lens();
        let n: usize = lens.iter().sum();
        let bracket =
            r.before.ends_with('(') || r.groups.iter().any(|(s, _)| s.contains(['(', ')']));
        let dashed = r.groups[1..].iter().all(|(s, _)| *s == "-")
            && matches!(lens[..], [3, 3, 4] | [3, 3, 2, 2]);
        let trunk = n == 11 && r.groups[0].1.starts_with('8');
        (10..=15).contains(&n) && (r.before.ends_with('+') || bracket || dashed || trunk)
    })
}

fn has_iban(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| {
        if !chars[i].is_ascii_alphabetic() || (i > 0 && chars[i - 1].is_alphanumeric()) {
            return false;
        }
        // An IBAN is often written in groups of four separated by single spaces.
        let mut s = Vec::with_capacity(34);
        let mut prev_space = false;
        for &c in &chars[i..] {
            if c.is_ascii_alphanumeric() && s.len() < 34 {
                s.push(c.to_ascii_uppercase());
                prev_space = false;
            } else if c == ' ' && !prev_space {
                prev_space = true;
            } else {
                break;
            }
        }
        s.len() >= 15
            && s[1].is_ascii_alphabetic()
            && s[2].is_ascii_digit()
            && s[3].is_ascii_digit()
            && (15..=s.len()).any(|n| iban_mod97(&s[..n]))
    })
}

fn iban_mod97(s: &[char]) -> bool {
    let mut rem: u32 = 0;
    for &c in s[4..].iter().chain(&s[..4]) {
        let Some(v) = c.to_digit(36) else {
            return false;
        };
        rem = if v < 10 {
            (rem * 10 + v) % 97
        } else {
            (rem * 100 + v) % 97
        };
    }
    rem == 1
}

/// Words naming a secret. A Cyrillic key is a stem and may take up to two more letters of an
/// ending (пароль, пароля, паролем); a Latin key may take a plural `s`.
const SECRET_KEYS: [&str; 34] = [
    "password",
    "passwort",
    "passwd",
    "passcode",
    "passphrase",
    "pass",
    "pwd",
    "pw",
    "seed phrase",
    "recovery phrase",
    "code",
    "pin",
    "pin code",
    "pin-code",
    "door code",
    "gate code",
    "access code",
    "entry code",
    "lock code",
    "alarm code",
    "security code",
    "cvv",
    "cvc",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "api key",
    "secret",
    "парол",
    "пин",
    "пин-код",
    "код",
    "токен",
];

/// Words between a key and its value ("password for the cottage: ...", "код подъезда 4821").
const MAX_GAP_WORDS: usize = 3;

/// Keys whose value may be as short as three digits ("CVV 123", "PIN: 482", "пароль 512").
const SHORT_DIGIT_KEYS: [&str; 8] = [
    "cvv",
    "cvc",
    "pin",
    "code",
    "password",
    "пин",
    "код",
    "парол",
];

/// Words before `code` that make it a public number, not a secret ("zip code 94105").
const PUBLIC_CODES: [&str; 7] = ["zip", "postal", "post", "area", "country", "почтовый", "qr"];

/// Words after which a number is a date or an amount, not a value ("changed in 2030").
const NOT_A_VALUE_AFTER: [&str; 14] = [
    "in",
    "since",
    "until",
    "till",
    "by",
    "from",
    "before",
    "after",
    "on",
    "for",
    "в",
    "с",
    "до",
    "после",
];

/// Keys of national ids with the digit counts of their numbers: a Russian passport (series
/// and number, or a travel passport), SNILS, INN and a US social security number.
const ID_KEYS: [(&str, &[usize]); 7] = [
    ("паспорт", &[9, 10]),
    ("passport", &[9, 10]),
    ("снилс", &[11]),
    ("snils", &[11]),
    ("инн", &[10, 12]),
    ("ssn", &[9]),
    ("social security", &[9]),
];

/// Words that may stand between the parts of a document number ("серия 45 12 номер 345678").
const ID_LABELS: [&str; 6] = ["сери", "номер", "series", "number", "no", "nr"];

/// Whether `ending` may follow `key` in one word: up to two letters of a case ending after a
/// Cyrillic stem, a plural `s` after a Latin word.
fn ending_ok(key: &str, ending: &str) -> bool {
    if key.chars().any(|c| ('а'..='я').contains(&c)) {
        ending.chars().count() <= 2
    } else {
        ending.is_empty() || ending == "s"
    }
}

/// Whether the word `w` is one of `keys` (see [`ending_ok`]).
fn is_word_of(w: &str, keys: &[&str]) -> bool {
    keys.iter()
        .any(|k| w.strip_prefix(k).is_some_and(|e| ending_ok(k, e)))
}

/// Each place where `key` stands as a word in `lower`: the text before it and the text after
/// its ending.
fn after_key<'a>(lower: &'a str, key: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
    lower.match_indices(key).filter_map(move |(i, _)| {
        let before = &lower[..i];
        if before
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            return None;
        }
        let rest = &lower[i + key.len()..];
        let end = rest
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(rest.len());
        ending_ok(key, &rest[..end]).then_some((before, &rest[end..]))
    })
}

fn has_assignment(lower: &str) -> bool {
    SECRET_KEYS.iter().any(|k| {
        let short = SHORT_DIGIT_KEYS.iter().any(|s| k.ends_with(s));
        after_key(lower, k).any(|(before, rest)| {
            let public = short
                && before
                    .split_whitespace()
                    .next_back()
                    .is_some_and(|w| PUBLIC_CODES.contains(&w));
            !public && value_follows(rest, short)
        })
    })
}

/// Whether the text after a secret key holds a value within [`MAX_GAP_WORDS`] words: one
/// after `:`, `=`, a dash or `is`/`это`, a PIN-like token (four or more characters with a
/// digit, or three digits after a `short` key) or a number spelled out in words, unless a
/// date preposition stands before it. A flat, house, year or price word before the number
/// does not make it public: the key governs it ("пароль от дома 4821"). A `vault:` pointer
/// is no value.
fn value_follows(rest: &str, short: bool) -> bool {
    let is_dash = |w: &str| !w.is_empty() && w.chars().all(|c| "-–—".contains(c));
    let is_copula = |w: &str| w == "is" || w == "это";
    // Punctuation between the key and its value is not a word ("password, by the way, is").
    let words: Vec<&str> = rest
        .split_whitespace()
        .filter(|w| is_dash(w) || w.contains([':', '=']) || w.chars().any(char::is_alphanumeric))
        .collect();
    let clean = |w: &str| {
        w.trim_matches(|c: char| "\"'`.,;:=()".contains(c))
            .to_owned()
    };
    let digits = |w: &str| w.chars().all(|c| c.is_ascii_digit());
    let value = |w: &str| {
        !w.starts_with("vault:") && (w.chars().count() >= 4 || short && w.len() >= 3 && digits(w))
    };
    let pin_like = |w: &str| value(w) && w.chars().any(|c| c.is_ascii_digit());
    let spelled = |from: &[&str]| spelled_digits(from) >= if short { 3 } else { 4 };
    let next = |n: usize| words.get(n + 1).map(|w| clean(w)).unwrap_or_default();
    for (n, w) in words.iter().enumerate().take(MAX_GAP_WORDS + 1) {
        if w.contains("://") || clean(w).starts_with("vault:") {
            return false;
        }
        if is_dash(w) {
            let m = if is_copula(&next(n)) { n + 1 } else { n };
            return value(&next(m)) || spelled(&words[m + 1..]);
        }
        if let Some(at) = w.find([':', '=']) {
            let tail = clean(&w[at + 1..]);
            if tail.is_empty() {
                return value(&next(n)) || spelled(&words[n + 1..]);
            }
            let mut from = vec![tail.as_str()];
            from.extend(&words[n + 1..]);
            return value(&tail) || spelled(&from);
        }
        if is_copula(w) {
            // After "is" only a word with a digit or a symbol counts: "is weak" is prose.
            let v = next(n);
            return (value(&v) && v.chars().any(|c| !c.is_alphabetic()))
                || spelled(&words[n + 1..]);
        }
        let dated = n
            .checked_sub(1)
            .is_some_and(|p| NOT_A_VALUE_AFTER.contains(&clean(words[p]).as_str()));
        if !dated && (pin_like(&clean(w)) || spelled(&words[n..])) {
            return true;
        }
        if w.ends_with(['.', '!', '?']) {
            return false;
        }
    }
    false
}

fn has_keyed_id(lower: &str) -> bool {
    ID_KEYS
        .iter()
        .any(|(k, sizes)| after_key(lower, k).any(|(_, rest)| sizes.contains(&id_digits(rest))))
}

/// The digits of the number that follows within [`MAX_GAP_WORDS`] words; labels and
/// punctuation are not counted as words.
fn id_digits(rest: &str) -> usize {
    let mut digits = 0;
    let mut gap = 0;
    for w in rest.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            digits += w.bytes().filter(u8::is_ascii_digit).count();
        } else if !w.is_empty() && !is_word_of(w, &ID_LABELS) {
            if digits > 0 || gap == MAX_GAP_WORDS {
                break;
            }
            gap += 1;
        }
    }
    digits
}

/// Number words by the digits they write: units, teens and tens, Russian hundreds.
const UNIT_WORDS: [&str; 24] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ноль",
    "нуль",
    "один",
    "одна",
    "одно",
    "два",
    "две",
    "три",
    "четыре",
    "пять",
    "шесть",
    "семь",
    "восемь",
    "девять",
];
const TEEN_WORDS: [&str; 20] = [
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "десять",
    "одиннадцать",
    "двенадцать",
    "тринадцать",
    "четырнадцать",
    "пятнадцать",
    "шестнадцать",
    "семнадцать",
    "восемнадцать",
    "девятнадцать",
];
const TEN_WORDS: [&str; 16] = [
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "двадцать",
    "тридцать",
    "сорок",
    "пятьдесят",
    "шестьдесят",
    "семьдесят",
    "восемьдесят",
    "девяносто",
];
const HUNDRED_WORDS: [&str; 9] = [
    "сто",
    "двести",
    "триста",
    "четыреста",
    "пятьсот",
    "шестьсот",
    "семьсот",
    "восемьсот",
    "девятьсот",
];

/// How many digits the number words at the start of lowercased `words` write: "one two three
/// four" and "сорок два семнадцать" write four, "twenty one" and "две" fewer. Words may be
/// joined by hyphens or commas.
fn spelled_digits(words: &[&str]) -> usize {
    let mut total = 0;
    // The places a smaller word still fills: 2 after a hundred, 1 after a ten.
    let mut open = 0;
    let mut lone_unit = false;
    for part in words.iter().flat_map(|w| w.split(['-', ','])) {
        let part = part.trim_matches(|c: char| !c.is_alphanumeric());
        if part.is_empty() {
            continue;
        }
        let was_lone_unit = std::mem::replace(&mut lone_unit, false);
        if UNIT_WORDS.contains(&part) {
            lone_unit = open == 0;
            total += usize::from(lone_unit);
            open = 0;
        } else if TEEN_WORDS.contains(&part) || TEN_WORDS.contains(&part) {
            if open < 2 {
                total += 2;
            }
            open = usize::from(TEN_WORDS.contains(&part));
        } else if part == "hundred" {
            // "one hundred" turns the lone unit before it into three digits.
            total += if was_lone_unit { 2 } else { 3 };
            open = 2;
        } else if HUNDRED_WORDS.contains(&part) {
            total += 3;
            open = 2;
        } else {
            break;
        }
    }
    total
}

fn has_key_prefix(w: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "AIza",
    ];
    let long = w.len() >= 20;
    (long && PREFIXES.iter().any(|p| w.starts_with(p)))
        || (w.len() == 20
            && w.starts_with("AKIA")
            && w[4..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
}

fn is_random_token(w: &str) -> bool {
    if w.len() < 32 || w.contains("://") {
        return false;
    }
    let charset = w
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_+/=".contains(&b));
    let letters = w.bytes().filter(u8::is_ascii_alphabetic).count();
    let digits = w.bytes().filter(u8::is_ascii_digit).count();
    charset && letters > 0 && digits > 0 && digits * 10 >= w.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secret_shapes() {
        assert!(looks_secret("card 4111 1111 1111 1111 for groceries"));
        assert!(
            !looks_secret("card 4111 1111 1111 1112 for groceries"),
            "Luhn fails"
        );
        assert!(looks_secret("salary goes to GB82 WEST 1234 5698 7654 32"));
        assert!(looks_secret("wifi password: hunter2x"));
        assert!(looks_secret("пароль = qwerty12"));
        assert!(looks_secret("my password is hunter2"));
        assert!(looks_secret("door code: 4821#"));
        for leak in [
            "Ada's wifi password for the cottage: Sunflower77",
            "Ada's wifi password for the cottage: Sunflower",
            "пароль от почты: qwerty12",
            "Пароль от домофона = 4821",
            "PIN 4821",
            "PIN 4821.",
            "door code 4821",
            "the pin-code is 4821",
            "пин-код 4821",
            "код 4821",
            "Passwords: hunter2x",
            "password:hunter2",
            "код подъезда 4821",
            "the code is 4821",
            "wifi password - Sunflower77",
            "wifi password — Sunflower77",
            "my password, by the way, is hunter2",
            "pass: hunter2x",
            "pw hunter2x",
            "CVV 123",
            "PIN: 482",
            "door code: 123",
            "seed phrase: apple banana cherry",
            "recovery phrase: apple banana cherry",
            "Passwort: hunter2x",
            "password: vaultkeeper77",
        ] {
            assert!(looks_secret(leak), "{leak}");
        }
        // Key-shaped samples are assembled at run time so secret scanners of the repository
        // do not flag the test itself.
        let gh = ["ghp", "_abcdefghijklmnopqrstuvwxyz0123"].concat();
        assert!(looks_secret(&format!("key {gh}")));
        let aws = ["AK", "IA", &"Q".repeat(16)].concat();
        assert!(looks_secret(&aws));
        assert!(looks_secret("x 9f8e7d6c5b4a39281706f5e4d3c2b1a0aa11bb22"));
        for ok in [
            "The passport is in the vault.",
            "The password is stored in the vault.",
            "Ada Example lives at 7 Sample Lane.",
            "Dentist appointment on 2030-04-01 at 10:00, phone +1 555 0100.",
            "Read https://example.invalid/articles/2030/garden-memory-and-retrieval",
            "The pin code of the bike lock is in the vault",
            "The token is valid until spring.",
            "The zip code of the office is in the letter.",
            "Trudy pinned 2030 as the year of the move.",
            "Налоговый кодекс 2031 года лежит на полке.",
            "The password of the bank is in the vault.",
            "The door code changed in 2030.",
            "PIN of the bike lock: vault:bike/lock",
            "PIN vault:bike/lock",
            "Ada's postal code is 10115.",
            "The zip code 94105 is on the letter.",
            "Ada bought a passport cover in 2030.",
            "The code review is on Friday.",
            "Check the token docs at https://example.invalid/a:b",
        ] {
            assert!(!looks_secret(ok), "{ok}");
        }
    }

    #[test]
    fn spelled_out_numbers_after_a_key() {
        for leak in [
            "PIN one two three four",
            "the door code is four two one",
            "wifi password: seven seven nine nine",
            "password - one-two-three-four",
            "пароль один два три четыре",
            "код от сейфа: ноль семь ноль семь",
            "пин-код это девять восемь семь",
            "код подъезда сорок два семнадцать",
            "the alarm code is twenty twenty",
        ] {
            assert!(looks_secret(leak), "{leak}");
        }
        for ok in [
            "Ada has two cats.",
            "У Ады две кошки и три собаки.",
            "Bo has three kids.",
            "Встреча в четыре часа.",
            "The PIN has four digits.",
            "The door code is three letters long.",
            "Код сменили в четыре часа.",
            "The gate code changed in twenty thirty.",
            "one two three four",
        ] {
            assert!(!looks_secret(ok), "{ok}");
        }
    }

    #[test]
    fn short_pins_after_a_key() {
        for leak in [
            "PIN 482",
            "pin: 482",
            "пин 4821",
            "код = 512",
            "code - 512",
            "пароль 512",
            "password: 512",
            "пароль это 512",
            "пароль — это 512",
            "my password is 512",
            "CVC 512",
            "код домофона 123456",
            "пароль от сейфа 512",
            // A public number word between the key and the number does not lift the key.
            "Пароль от дома 4821",
            "Код от квартиры 4821",
            "PIN for the house 4821",
            "door code of flat 12: 4821",
            "Код домофона, квартира 123.",
            "Код подъезда на этаже 512.",
            "The door code of apartment 123 is in the vault.",
            "The door code, price 450, is in the vault.",
        ] {
            assert!(looks_secret(leak), "{leak}");
        }
        for ok in [
            "квартира 12",
            "дом 5",
            "индекс 101000",
            "этаж 3",
            "price 4821",
            "Ada lives in flat 42, floor 7, house 154.",
            "Квартира 123, этаж 5, год постройки 2030, цена 4500000.",
            "Wifi password on floor 12 is in the vault.",
        ] {
            assert!(!looks_secret(ok), "{ok}");
        }
    }

    /// The check digits of a SNILS whose first nine digits are `base`.
    fn snils(base: [u32; 9]) -> String {
        let sum: u32 = base
            .iter()
            .zip([9, 8, 7, 6, 5, 4, 3, 2, 1])
            .map(|(d, w)| d * w)
            .sum();
        let check = match sum {
            0..100 => sum,
            100 | 101 => 0,
            _ => sum % 101 % 100,
        };
        let d: String = base.iter().map(u32::to_string).collect();
        format!("{}-{}-{} {check:02}", &d[..3], &d[3..6], &d[6..])
    }

    /// A personal INN whose first ten digits are `base`, with its two check digits.
    fn inn(base: [u32; 10]) -> String {
        let w11 = [7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        let c11 = base.iter().zip(w11).map(|(d, w)| d * w).sum::<u32>() % 11 % 10;
        let w12 = [3, 7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        let c12 = base
            .iter()
            .chain([&c11])
            .zip(w12)
            .map(|(d, w)| d * w)
            .sum::<u32>()
            % 11
            % 10;
        let d: String = base.iter().map(u32::to_string).collect();
        format!("{d}{c11}{c12}")
    }

    /// `id` with its last digit changed, so its checksum fails.
    fn broken(id: &str) -> String {
        let (head, last) = id.split_at(id.len() - 1);
        let last = (last.as_bytes()[0] - b'0' + 1) % 10;
        format!("{head}{last}")
    }

    #[test]
    fn national_ids() {
        // Numbers below 001-001-998 are never issued as SNILS; region 00 has no INN.
        let snils = snils([0, 0, 0, 0, 0, 0, 7, 7, 7]);
        let inn = inn([0, 0, 0, 0, 1, 2, 3, 4, 5, 6]);
        for leak in [
            format!("Ada's insurance number {snils}"),
            format!("{inn} is on the tax form"),
            "Bo's SSN is 123-45-6789".to_owned(),
            "card says 123-45-6789".to_owned(),
            "паспорт 00 00 123456".to_owned(),
            "Паспорт: серия 00 00 номер 123456".to_owned(),
            "passport number 0000 123456".to_owned(),
            format!("СНИЛС {}", broken(&snils)),
            format!("ИНН {}", broken(&inn)),
            "SSN 123 45 6789".to_owned(),
        ] {
            assert!(looks_secret(&leak), "{leak}");
        }
        for ok in [
            format!("Ada's insurance number {}", broken(&snils)),
            format!("{} is on the tax form", broken(&inn)),
            "card says 000-12-3456".to_owned(),
            "card says 666-12-3456".to_owned(),
            "card says 912-34-5678".to_owned(),
            "card says 123-00-4567".to_owned(),
            "card says 123-45-0000".to_owned(),
            "Ada's passport expires in 2030.".to_owned(),
            "паспорт выдан 12.05.2030".to_owned(),
            "Ada's passport photo is from 2030-04-01.".to_owned(),
        ] {
            assert!(!looks_secret(&ok), "{ok}");
        }
    }

    #[test]
    fn phone_numbers() {
        for leak in [
            "Ada's phone +7 (000) 123-45-67",
            "call 8 (000) 123-45-67",
            "call 8 000 123 45 67",
            "Bo: +1 (555) 010-0199",
            "Bo: +1 555 010 0199",
            "office +49 000 1234567",
            "office +49-000-123-4567",
            "mobile (000) 123-45-67",
            "mobile 000-123-45-67",
            "mobile +70001234567",
        ] {
            assert!(looks_secret(leak), "{leak}");
        }
        for ok in [
            "Dentist appointment on 2030-04-01 at 10:00, phone +1 555 0100.",
            "Moved on 01.04.2030 at 10:30.",
            "The flat costs 12 500 000 rubles.",
            "Postal code 101000, house 15, flat 42.",
            "From 2030-04-01 to 2030-05-01.",
            "The trip was 2030-2031.",
            "The meeting is 10:00-12:00 in room 512.",
        ] {
            assert!(!looks_secret(ok), "{ok}");
        }
    }

    #[test]
    fn secret_pointer_heuristic() {
        assert!(may_carry_secret("passport number 4512 334455"));
        assert!(may_carry_secret("api key sk9fj2k3l4m5n6b7v8"));
        let token = "ab12".repeat(5);
        assert!(may_carry_secret(&format!("token={token}")));
        assert!(may_carry_secret("Bank PIN one two three four exists."));
        assert!(may_carry_secret("The value is seven seven nine nine."));
        assert!(may_carry_secret("The bank PIN is 482."));
        assert!(may_carry_secret(
            "Ada's phone +7 (000) 123-45-67 is in the vault."
        ));
        assert!(!may_carry_secret("The passport is in the vault."));
        assert!(!may_carry_secret("Ada has two bank cards with PINs."));
        assert!(!has_token("chat:-100123456/msg:789"));
        assert!(has_token(&format!("https://x.example/?token={token}")));
    }
}
