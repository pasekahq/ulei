//! The strict TOML subset `ulei.toml` is written in: section headers, `key = "string"`,
//! `key = integer`, `key = true` or `false`, comments and blank lines. It accepts exactly what
//! the engine's reader accepts, so both read the one file.

/// A value of the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
}

/// The values of a file by dotted key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Values(Vec<(String, Value)>);

impl Values {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// The values of `text`, or the first offending line number and why.
pub(crate) fn parse(text: &str) -> Result<Values, (usize, &'static str)> {
    let mut values: Vec<(String, Value)> = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    let mut section = String::new();
    for (n, line) in (1..).zip(text.lines()) {
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let (name, rest) = header
                .split_once(']')
                .ok_or((n, "unterminated section header"))?;
            let name = name.trim_matches([' ', '\t']);
            if !name.split('.').all(is_bare_key) {
                return Err((n, "unsupported section header"));
            }
            trailing(rest).ok_or((n, "unexpected text after the section header"))?;
            if sections.iter().any(|s| s == name) {
                return Err((n, "duplicate section"));
            }
            sections.push(name.to_owned());
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').ok_or((n, "expected key = value"))?;
        let key = key.trim_matches([' ', '\t']);
        if !is_bare_key(key) {
            return Err((n, "unsupported key"));
        }
        let value = parse_value(value.trim_matches([' ', '\t'])).map_err(|reason| (n, reason))?;
        let key = if section.is_empty() {
            key.to_owned()
        } else {
            format!("{section}.{key}")
        };
        if values.iter().any(|(k, _)| *k == key) {
            return Err((n, "duplicate key"));
        }
        values.push((key, value));
    }
    Ok(Values(values))
}

/// A basic string without escapes, a decimal integer or a boolean, then at most a comment.
fn parse_value(text: &str) -> Result<Value, &'static str> {
    if let Some(body) = text.strip_prefix('"') {
        let (string, rest) = body.split_once('"').ok_or("unterminated string")?;
        if string.contains('\\') {
            return Err("escapes in strings are not supported");
        }
        if string.chars().any(char::is_control) {
            return Err("control character in a string");
        }
        trailing(rest).ok_or("unexpected text after the value")?;
        return Ok(Value::String(string.to_owned()));
    }
    let end = text.find([' ', '\t', '#']).unwrap_or(text.len());
    let (number, rest) = text.split_at(end);
    if let Some(value) = match number {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    } {
        trailing(rest).ok_or("unexpected text after the value")?;
        return Ok(Value::Bool(value));
    }
    let digits = number.strip_prefix(['+', '-']).unwrap_or(number);
    let decimal = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !decimal {
        return Err("unsupported value");
    }
    trailing(rest).ok_or("unexpected text after the value")?;
    number
        .parse()
        .map(Value::Integer)
        .map_err(|_| "integer out of range")
}

/// `Some` when `rest` is blank or a comment.
fn trailing(rest: &str) -> Option<()> {
    let rest = rest.trim_matches([' ', '\t']);
    (rest.is_empty() || rest.starts_with('#')).then_some(())
}

fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
