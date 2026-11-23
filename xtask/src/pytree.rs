//! The Python tree `hive-py/` (ADR-0068): its `module.toml` and its runtime lock.

use std::path::Path;

use crate::util::{self, Errors};

/// Directory of the Python tree, relative to the repository root.
pub const DIR: &str = "hive-py";

/// Packages admitted to `requirements.lock` by an ADR naming `hive-py` and `dep:py:<name>`.
pub const ADMITTED: &[&str] = &["cronsim"];

pub struct PyTree {
    pub role: String,
    pub contracts: Vec<String>,
    /// `(name, version)` of each pin of `requirements.lock`, names normalised.
    pub packages: Vec<(String, String)>,
}

/// Reads `module.toml` and `requirements.lock` of the tree; every malformed entry is an error.
pub fn load(root: &Path) -> Result<PyTree, Errors> {
    let mut errors = Errors::new();
    let module = format!("{DIR}/module.toml");
    let (role, contracts) = match util::read(&root.join(&module)).and_then(|t| {
        t.parse::<toml::Table>()
            .map_err(|e| format!("{module}: {e}"))
    }) {
        Ok(table) => {
            let role = table.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role.trim().is_empty() {
                errors.push(format!("{module}: role is missing"));
            }
            let contracts = match table.get("contracts").and_then(|v| v.as_array()) {
                Some(a) => a
                    .iter()
                    .filter_map(|v| {
                        let id = v.as_str();
                        if id.is_none() {
                            errors.push(format!("{module}: contract id {v} is not a string"));
                        }
                        id.map(str::to_string)
                    })
                    .collect(),
                None => {
                    errors.push(format!("{module}: contracts = [...] is missing"));
                    Vec::new()
                }
            };
            (role.to_string(), contracts)
        }
        Err(e) => {
            errors.push(e);
            (String::new(), Vec::new())
        }
    };
    let lock = format!("{DIR}/requirements.lock");
    let packages = match util::read(&root.join(&lock)) {
        Ok(text) => parse_lock(&lock, &text, &mut errors),
        Err(e) => {
            errors.push(e);
            Vec::new()
        }
    };
    if errors.is_empty() {
        Ok(PyTree {
            role,
            contracts,
            packages,
        })
    } else {
        Err(errors)
    }
}

/// The line without its comment: a `#` at the start or after whitespace, as pip reads it.
fn strip_comment(line: &str) -> &str {
    let mut prev_space = true;
    for (i, c) in line.char_indices() {
        if c == '#' && prev_space {
            return &line[..i];
        }
        prev_space = c.is_whitespace();
    }
    line
}

/// `name==version` of every requirement line; comments and blank lines carry none.
fn parse_lock(rel: &str, text: &str, errors: &mut Errors) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let req = strip_comment(line).trim();
        if req.is_empty() {
            continue;
        }
        let pin = req.split_whitespace().next().unwrap_or("");
        match pin.split_once("==") {
            Some((name, version)) if !name.is_empty() && !version.is_empty() => out.push((
                name.to_ascii_lowercase().replace(['_', '.'], "-"),
                version.to_string(),
            )),
            _ => errors.push(format!(
                "{rel}:{}: `{pin}` is not an exact `name==version` pin",
                n + 1
            )),
        }
    }
    out
}
