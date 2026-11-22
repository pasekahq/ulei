//! Typed reads of the `ulei.toml` values the core's binaries take from
//! `ulei_contracts::config`.

use ulei_contracts::config::{Config, Value};

/// The non-empty string of `key`; an error is one line naming the key.
///
/// # Errors
/// The key is missing, empty or not a string.
pub fn string(config: &Config, key: &str) -> Result<String, String> {
    match config.get(key).map(|e| &e.value) {
        Some(Value::String(s)) if s.is_empty() => Err(format!("{key} is empty")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing {key}")),
    }
}
