//! `ulei_contracts::config` on synthetic files and variables.

#![allow(missing_docs, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ulei_contracts::config::{CONFIG_ENV, Config, Entry, Error, Fault, Value};
use ulei_contracts::generated::c9::{KEYS, Kind};

/// A directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ulei-config-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, text: &str) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let pairs: Vec<(String, OsString)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

fn found(flag: Option<&str>, pairs: &[(&str, &str)]) -> Result<PathBuf, String> {
    Config::find(flag.map(Path::new), vars(pairs)).map_err(|e| e.to_string())
}

/// The one error line of `text` as a config file.
fn refusal(text: &str) -> (String, PathBuf) {
    let dir = Scratch::new();
    let path = dir.file(text);
    let err = Config::load(&path).expect_err("refused");
    (err.to_string(), path)
}

fn line_error(text: &str, line: usize, what: &str) {
    let (got, path) = refusal(text);
    assert_eq!(
        got,
        format!("config {} line {line}: {what}", path.display()),
        "{text:?}"
    );
}

#[test]
fn the_flag_wins_over_every_variable() {
    let all = [
        (CONFIG_ENV, "/synthetic/env.toml"),
        ("XDG_CONFIG_HOME", "/synthetic/xdg"),
        ("HOME", "/synthetic/user"),
    ];
    assert_eq!(
        found(Some("/synthetic/flag.toml"), &all),
        Ok(PathBuf::from("/synthetic/flag.toml"))
    );
}

#[test]
fn ulei_config_comes_next() {
    let all = [
        (CONFIG_ENV, "/synthetic/env.toml"),
        ("XDG_CONFIG_HOME", "/synthetic/xdg"),
        ("HOME", "/synthetic/user"),
    ];
    assert_eq!(found(None, &all), Ok(PathBuf::from("/synthetic/env.toml")));
}

#[test]
fn an_absolute_xdg_config_home_comes_next() {
    let vars = [
        (CONFIG_ENV, ""),
        ("XDG_CONFIG_HOME", "/synthetic/xdg"),
        ("HOME", "/synthetic/user"),
    ];
    assert_eq!(
        found(None, &vars),
        Ok(PathBuf::from("/synthetic/xdg/ulei/ulei.toml"))
    );
}

#[test]
fn home_comes_last_and_a_relative_or_empty_xdg_config_home_is_skipped() {
    for xdg in ["relative/xdg", ""] {
        let vars = [("XDG_CONFIG_HOME", xdg), ("HOME", "/synthetic/user")];
        assert_eq!(
            found(None, &vars),
            Ok(PathBuf::from("/synthetic/user/.config/ulei/ulei.toml")),
            "{xdg:?}"
        );
    }
}

#[test]
fn no_step_is_an_error() {
    let err = found(None, &[("HOME", ""), ("XDG_CONFIG_HOME", "relative")]).unwrap_err();
    assert_eq!(
        err,
        "config: no --config and none of ULEI_CONFIG, XDG_CONFIG_HOME or HOME is set"
    );
}

#[test]
fn read_decodes_the_file_the_flag_names() {
    let dir = Scratch::new();
    let path = dir.file("# synthetic\n\n[qb]\nsocket = \"/synthetic/qb.sock\" # gateway\n");
    let config = Config::read(Some(&path)).unwrap();
    assert_eq!(config.path, path);
    assert_eq!(
        config.entries,
        [Entry {
            key: "qb.socket".into(),
            line: 4,
            value: Value::String("/synthetic/qb.sock".into()),
        }]
    );
}

#[test]
fn values_come_by_dotted_key_with_their_lines() {
    let dir = Scratch::new();
    let path = dir.file(
        "[inbox]\nroot = \"/synthetic/inbox\"\ndeliver = true\n\n[engine.routes.main]\nmax_tokens = -4096\n",
    );
    let config = Config::load(&path).unwrap();
    let got: Vec<(&str, usize, &Value)> = config
        .entries
        .iter()
        .map(|e| (e.key.as_str(), e.line, &e.value))
        .collect();
    assert_eq!(
        got,
        [
            ("inbox.root", 2, &Value::String("/synthetic/inbox".into())),
            ("inbox.deliver", 3, &Value::Bool(true)),
            ("engine.routes.main.max_tokens", 6, &Value::Integer(-4096)),
        ]
    );
    assert_eq!(config.get("inbox.deliver").map(|e| e.line), Some(3));
    assert_eq!(config.get("inbox.socket"), None);
}

#[test]
fn every_section_header_is_kept_with_its_line_even_without_keys() {
    let dir = Scratch::new();
    let path = dir.file("[inbox]\nroot = \"/synthetic/inbox\"\n\n[engine.mcp.empty]\n[qb]\n");
    let config = Config::load(&path).unwrap();
    let got: Vec<(&str, usize)> = config
        .sections
        .iter()
        .map(|s| (s.name.as_str(), s.line))
        .collect();
    assert_eq!(got, [("inbox", 1), ("engine.mcp.empty", 4), ("qb", 5)]);
}

#[test]
fn a_missing_file_at_the_chosen_path_is_an_error() {
    let dir = Scratch::new();
    let path = dir.0.join("absent.toml");
    let err = Config::load(&path).unwrap_err();
    assert!(matches!(err, Error::Read(..)));
    let line = err.to_string();
    assert!(
        line.starts_with(&format!("config {}: ", path.display())),
        "{line}"
    );
    assert!(!line.contains('\n'));
}

#[test]
fn every_line_outside_the_subset_names_its_reason() {
    let cases = [
        ("[qb\n", "unterminated section header"),
        ("[q b]\n", "unsupported section header"),
        ("[[qb]]\n", "unsupported section header"),
        ("[\"qb\"]\n", "unsupported section header"),
        ("[qb] x\n", "unexpected text after the section header"),
        ("[qb]\nsocket\n", "expected key = value"),
        ("[qb]\n\"socket\" = \"x\"\n", "unsupported key"),
        ("[qb]\na.b = \"x\"\n", "unsupported key"),
        ("[qb]\nsocket = \"x\n", "unterminated string"),
        (
            "[qb]\nsocket = \"a\\tb\"\n",
            "escapes in strings are not supported",
        ),
        ("[qb]\nsocket = 'x'\n", "unsupported value"),
        (
            "[qb]\nsocket = \"a\u{7}b\"\n",
            "control character in a string",
        ),
        (
            "[qb]\nsocket = \"x\" y\n",
            "unexpected text after the value",
        ),
        (
            "[inbox]\ndeliver = true y\n",
            "unexpected text after the value",
        ),
        (
            "[engine]\nmax_steps = 9 y\n",
            "unexpected text after the value",
        ),
        ("[engine]\nmax_steps = 012\n", "unsupported value"),
        ("[engine]\nmax_steps = 1_000\n", "unsupported value"),
        ("[engine]\nmax_steps = 0x10\n", "unsupported value"),
        (
            "[engine]\nmax_steps = 9223372036854775808\n",
            "integer out of range",
        ),
    ];
    for (text, reason) in cases {
        let line = text.lines().count();
        line_error(text, line, reason);
    }
}

#[test]
fn floats_datetimes_arrays_and_inline_tables_are_refused() {
    for value in [
        "1.0",
        "1e3",
        "inf",
        "nan",
        "1979-05-27T07:32:00Z",
        "1979-05-27",
        "07:32:00",
        "[1, 2]",
        "{ a = 1 }",
    ] {
        line_error(
            &format!("[engine.routes.main]\nmax_tokens = {value}\n"),
            2,
            "unsupported value",
        );
    }
}

#[test]
fn a_duplicate_section_and_a_duplicate_key_are_refused() {
    line_error("[qb]\n[inbox]\n[qb]\n", 3, "duplicate section");
    line_error(
        "[qb]\nsocket = \"/a\"\n# again\nsocket = \"/b\"\n",
        4,
        "duplicate key",
    );
    line_error(
        "[engine]\n[engine.routes.main]\nmax_tokens = 1\nmax_tokens = 2\n",
        4,
        "duplicate key",
    );
}

#[test]
fn a_section_outside_the_catalogue_is_refused() {
    line_error("[qb]\n[gateway]\n", 2, "unknown section [gateway]");
    line_error("[engine.journal]\n", 1, "unknown section [engine.journal]");
    line_error(
        "[engine.routes.main.extra]\n",
        1,
        "unknown section [engine.routes.main.extra]",
    );
}

#[test]
fn an_item_named_like_a_key_beside_it_is_refused() {
    line_error("[connectors.log]\n", 1, "unknown section [connectors.log]");
    let dir = Scratch::new();
    let config = Config::load(
        &dir.file("[connectors]\nlog = \"/x\"\n[connectors.logs]\nnetwork = \"direct\"\n"),
    )
    .unwrap();
    assert_eq!(
        config.get("connectors.logs.network").map(|e| &e.value),
        Some(&Value::String("direct".to_owned()))
    );
}

#[test]
fn a_key_outside_the_catalogue_is_refused() {
    line_error("socket = \"/x\"\n", 1, "unknown key socket");
    line_error("[engine]\nroutes = 1\n", 2, "unknown key engine.routes");
    line_error(
        "[engine.mcp.notes]\nheaders = \"x\"\n",
        2,
        "unknown key engine.mcp.notes.headers",
    );
}

#[test]
fn an_inline_telegram_token_is_an_unknown_key_without_its_value() {
    let secret = "123456:synthetic-secret-value";
    let (line, path) = refusal(&format!("[telegram]\ntoken = \"{secret}\"\n"));
    assert_eq!(
        line,
        format!(
            "config {} line 2: unknown key telegram.token",
            path.display()
        )
    );
    assert!(!line.contains(secret));
}

#[test]
fn a_value_of_another_kind_names_the_kind_and_not_the_value() {
    line_error("[inbox]\nroot = 4242\n", 2, "inbox.root must be a string");
    line_error(
        "[engine]\nmemory_char_limit = \"4242\"\n",
        2,
        "engine.memory_char_limit must be an integer",
    );
    line_error(
        "[inbox]\ndeliver = \"4242\"\n",
        2,
        "inbox.deliver must be true or false",
    );
    line_error(
        "[engine]\nmax_steps = false\n",
        2,
        "engine.max_steps must be an integer or a string",
    );
    let (line, _) = refusal("[engine.mcp.notes]\nurl = 4242\n");
    assert!(
        line.ends_with("engine.mcp.notes.url must be a string"),
        "{line}"
    );
    assert!(!line.contains("4242"));
}

#[test]
fn integer_or_string_takes_both() {
    for value in ["\"unlimited\"", "90", "\"2.5\""] {
        let dir = Scratch::new();
        let path = dir.file(&format!("[engine]\nmax_steps = {value}\n"));
        assert!(Config::load(&path).is_ok(), "{value}");
    }
}

#[test]
fn the_switch_kind_takes_a_string_an_integer_or_a_boolean() {
    for value in ["\"on\"", "1", "true"] {
        let dir = Scratch::new();
        let path = dir.file(&format!(
            "[engine]\nstall_guards = {value}\n[engine.mcp.notes.tools]\nprompts = {value}\n"
        ));
        let config = Config::load(&path).unwrap();
        assert_eq!(config.entries.len(), 2, "{value}");
    }
}

fn sample(kind: Kind) -> &'static str {
    match kind {
        Kind::String | Kind::IntegerOrString => "\"x\"",
        Kind::Integer => "7",
        Kind::Boolean | Kind::Any => "true",
    }
}

fn wrong(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::String => Some("7"),
        Kind::Integer => Some("\"7\""),
        Kind::Boolean => Some("\"true\""),
        Kind::IntegerOrString => Some("true"),
        Kind::Any => None,
    }
}

/// `pattern` with every item name `*` as `item`, split into its section and key.
fn split(pattern: &str) -> (String, String) {
    let key = pattern.replace('*', "item-1");
    let (section, name) = key.rsplit_once('.').unwrap();
    (section.to_owned(), name.to_owned())
}

#[test]
fn every_key_of_the_catalogue_takes_a_value_of_its_kind() {
    let mut sections: BTreeMap<String, Vec<(String, Kind)>> = BTreeMap::new();
    for &(pattern, kind) in KEYS {
        let (section, name) = split(pattern);
        sections.entry(section).or_default().push((name, kind));
    }
    let mut text = String::new();
    for (section, keys) in &sections {
        writeln!(text, "[{section}]").unwrap();
        for (name, kind) in keys {
            writeln!(text, "{name} = {}", sample(*kind)).unwrap();
        }
    }
    let dir = Scratch::new();
    let config = Config::load(&dir.file(&text)).unwrap();
    assert_eq!(config.entries.len(), KEYS.len());
}

#[test]
fn every_key_of_the_catalogue_refuses_a_value_of_another_kind() {
    for &(pattern, kind) in KEYS {
        let Some(value) = wrong(kind) else { continue };
        let (section, name) = split(pattern);
        let dir = Scratch::new();
        let err = Config::load(&dir.file(&format!("[{section}]\n{name} = {value}\n")));
        assert!(
            matches!(&err, Err(Error::Line { line: 2, fault: Fault::WrongKind(key, k), .. }) if *k == kind && *key == format!("{section}.{name}")),
            "{pattern}: {err:?}"
        );
    }
}
