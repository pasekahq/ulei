//! Discovery of the connected connectors: the names of `connectors.enabled` whose directory in
//! `connectors.dir` holds a valid C11 `Manifest` as `connector.json`.

use std::fs;
use std::path::{Path, PathBuf};

use ulei_contracts::CONTRACTS;
use ulei_contracts::generated::c11::Manifest;

use crate::c6::is_route_alias;

/// The manifest's file name in a connector's directory.
const MANIFEST: &str = "connector.json";
/// Most `source.addresses` of a manifest.
const MAX_ADDRESSES: usize = 8;

/// A connector with a valid manifest.
#[derive(Debug, Clone)]
pub struct Connector {
    /// Its name: its directory, its `[connectors.<name>]` and the C10 `source` of its items.
    pub name: String,
    /// Its directory in `connectors.dir`.
    pub dir: PathBuf,
    /// Its `connector.json`.
    pub manifest: Manifest,
}

/// What discovery found for the enabled names, each name once, in `connectors.enabled` order.
#[derive(Debug, Default)]
pub struct Discovery {
    /// The enabled names with a valid manifest.
    pub connectors: Vec<Connector>,
    /// One line per enabled name that is not scheduled: the name and why.
    pub refused: Vec<(String, String)>,
}

/// The names of a `connectors.enabled` value: split at commas, trimmed, empty ones and repeats
/// dropped.
pub fn enabled(list: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// The connectors of `enabled` among the subdirectories of `dir` that hold a manifest; `.git`
/// is never one, and nothing runs git.
///
/// # Errors
/// `dir` cannot be listed; the text names it.
pub fn discover(dir: &Path, enabled: &[String]) -> Result<Discovery, String> {
    let listing = |e: std::io::Error| format!("connectors.dir {}: {e}", dir.display());
    let mut present = Vec::new();
    for entry in fs::read_dir(dir).map_err(listing)? {
        let entry = entry.map_err(listing)?;
        // A name that is not UTF-8 cannot be a C11 `Name`.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name != ".git" && entry.path().join(MANIFEST).is_file() {
            present.push(name);
        }
    }
    let mut discovery = Discovery::default();
    for name in enabled {
        match load(dir, name, &present) {
            Ok(connector) => discovery.connectors.push(connector),
            Err(reason) => discovery.refused.push((name.clone(), reason)),
        }
    }
    Ok(discovery)
}

/// The connector `name` of `dir` when its manifest is valid; the error is why not.
fn load(dir: &Path, name: &str, present: &[String]) -> Result<Connector, String> {
    if !is_name(name) {
        return Err(
            "not a connector name (^[a-z][a-z0-9_-]{0,63}$, not dialogue) in connectors.enabled"
                .to_owned(),
        );
    }
    let path = dir.join(name);
    if !present.iter().any(|p| p == name) {
        return Err(format!("no {MANIFEST} in {}", path.display()));
    }
    let bytes = fs::read(path.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{MANIFEST} is not a C11 Manifest: {e}", e = json_fault(&e)))?;
    check(&manifest, name)?;
    Ok(Connector {
        name: name.to_owned(),
        dir: path,
        manifest,
    })
}

/// What the generated [`Manifest`] does not encode: the patterns and ranges of C11, the name
/// equal to the directory and the contract's major equal to this runner's C11.
fn check(manifest: &Manifest, name: &str) -> Result<(), String> {
    if manifest.name != name {
        return Err(format!(
            "manifest name {:?} is not its directory name",
            manifest.name
        ));
    }
    if !is_semver(&manifest.version) {
        return Err("version is not a semantic version".to_owned());
    }
    let ours = c11_version()?;
    if !is_semver(&manifest.contract) || major(&manifest.contract) != major(ours) {
        return Err(format!(
            "contract {:?} is not C11 {}.x",
            manifest.contract,
            major(ours)
        ));
    }
    let program = manifest.command.first().ok_or("command is empty")?;
    let under_usr = program
        .strip_prefix("/usr/")
        .is_some_and(|rest| !rest.is_empty());
    if program.is_empty()
        || program.split('/').any(|segment| segment == "..")
        || (program.starts_with('/') && !under_usr)
    {
        return Err(format!(
            "command program {program:?} is neither a path in the connector's directory without .. nor under /usr"
        ));
    }
    if manifest.command.iter().any(String::is_empty) {
        return Err("command has an empty argument".to_owned());
    }
    if !(60..=604_800).contains(&manifest.schedule.every) {
        return Err(format!(
            "schedule.every {} is outside 60..604800",
            manifest.schedule.every
        ));
    }
    if let Some(timeout) = manifest.timeout.filter(|t| !(1..=3600).contains(t)) {
        return Err(format!("timeout {timeout} is outside 1..3600"));
    }
    if manifest.source.addresses.len() > MAX_ADDRESSES {
        return Err(format!(
            "source.addresses has more than {MAX_ADDRESSES} entries"
        ));
    }
    for address in &manifest.source.addresses {
        host_port(address).map_err(|e| format!("source.addresses: {e}"))?;
    }
    if let Some(key) = manifest.key.as_deref().filter(|k| !is_route_alias(k)) {
        return Err(format!("key {key:?} is not a key name"));
    }
    Ok(())
}

/// Whether `text` is a C11 `Name`; it shares its pattern with C6 `RouteAlias`.
pub fn is_name(text: &str) -> bool {
    is_route_alias(text) && text != "dialogue"
}

/// The version of C11 this runner speaks.
///
/// # Errors
/// The contracts index has no C11.
pub(crate) fn c11_version() -> Result<&'static str, String> {
    CONTRACTS
        .iter()
        .find(|c| c.id == "C11")
        .map(|c| c.version)
        .ok_or_else(|| "the contracts index has no C11".to_owned())
}

/// The class and column of a JSON error, never the input around it.
pub(crate) fn json_fault(e: &serde_json::Error) -> String {
    let class = match e.classify() {
        serde_json::error::Category::Io => "read error",
        serde_json::error::Category::Syntax => "JSON syntax error",
        serde_json::error::Category::Data => "wrong field or value",
        serde_json::error::Category::Eof => "JSON ends early",
    };
    format!("{class} at line {} column {}", e.line(), e.column())
}

/// Checks C11 `HostPort` with a port in 1..65535; the error names the entry.
///
/// # Errors
/// `text` is not `host:port`, or its port is outside 1..65535.
pub(crate) fn host_port(text: &str) -> Result<(), String> {
    let malformed = || format!("{text:?} is not host:port");
    let (host, port) = text.rsplit_once(':').ok_or_else(malformed)?;
    let host_ok = if let Some(ip) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        (2..=45).contains(&ip.len())
            && ip
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
    } else {
        let b = host.as_bytes();
        (1..=253).contains(&b.len())
            && b.first().is_some_and(u8::is_ascii_alphanumeric)
            && b.last().is_some_and(u8::is_ascii_alphanumeric)
            && b.iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
    };
    if !host_ok
        || port.is_empty()
        || port.starts_with('0')
        || !port.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(malformed());
    }
    match port.parse::<u32>() {
        Ok(n) if (1..=65_535).contains(&n) => Ok(()),
        _ => Err(format!("port {port} of {text:?} is outside 1..65535")),
    }
}

/// Whether `text` matches C11 `Semver`.
fn is_semver(text: &str) -> bool {
    let ident = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    };
    let (rest, build) = match text.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (text, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers.iter().all(|n| {
            !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && (n.len() == 1 || !n.starts_with('0'))
        })
        && pre.is_none_or(ident)
        && build.is_none_or(ident)
}

/// The `MAJOR` of a semantic version.
fn major(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}
