//! Deadline, step limit and Exa base URL settings: the environment, then the config file, then
//! the default.

#![allow(missing_docs, clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;
use std::time::Duration;

use ulei_engine::config::{
    CONCURRENT_TOOL_TIMEOUT, Config, Error, MAX_STEPS, MAX_TIMEOUT, WEB_BASE_URL,
};

const DEFAULT: Duration = Duration::from_secs(420);

fn from_env(value: &str) -> Result<Option<Duration>, Error> {
    Config::default().timeout(
        CONCURRENT_TOOL_TIMEOUT,
        Some(OsString::from(value)),
        DEFAULT,
    )
}

/// A config file of `line` in the `[engine]` section.
fn file(line: &str) -> Config {
    let path = std::env::temp_dir().join(format!(
        "ulei-config-{}-{}.toml",
        std::process::id(),
        line.len()
    ));
    fs::write(&path, format!("[engine]\n{line}\n")).unwrap();
    let config = Config::load(&path).unwrap();
    fs::remove_file(&path).unwrap();
    config
}

fn from_file(line: &str, env: Option<&str>) -> Result<Option<Duration>, Error> {
    file(line).timeout(CONCURRENT_TOOL_TIMEOUT, env.map(OsString::from), DEFAULT)
}

fn steps(config: &Config, env: Option<&str>) -> Result<usize, Error> {
    config.limit(MAX_STEPS, env.map(OsString::from), 7)
}

#[test]
fn the_deadline_is_seconds_from_the_environment_with_zero_and_nan_for_none() {
    assert_eq!(
        Config::default()
            .timeout(CONCURRENT_TOOL_TIMEOUT, None, DEFAULT)
            .unwrap(),
        Some(DEFAULT)
    );
    assert_eq!(from_env(" 0.5 ").unwrap(), Some(Duration::from_millis(500)));
    assert_eq!(from_env("30").unwrap(), Some(Duration::from_secs(30)));
    assert_eq!(from_env("0").unwrap(), None);
    assert_eq!(from_env("-1").unwrap(), None);
    assert_eq!(from_env("nan").unwrap(), None);
    assert_eq!(from_env("inf").unwrap(), Some(MAX_TIMEOUT));
    assert_eq!(from_env("  ").unwrap(), Some(DEFAULT));
    let error = from_env("soon").unwrap_err();
    assert_eq!(
        error.to_string(),
        "engine.concurrent_tool_timeout (ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT) must be a number \
         of seconds"
    );
}

#[test]
fn the_config_file_sets_the_deadline_when_the_environment_does_not() {
    assert_eq!(
        from_file("concurrent_tool_timeout = 60", None).unwrap(),
        Some(Duration::from_secs(60))
    );
    assert_eq!(
        from_file("concurrent_tool_timeout = \"1.5\"", None).unwrap(),
        Some(Duration::from_millis(1500))
    );
    assert_eq!(
        from_file("concurrent_tool_timeout = 0", None).unwrap(),
        None
    );
    assert_eq!(
        from_file("concurrent_tool_timeout = 60", Some("5")).unwrap(),
        Some(Duration::from_secs(5))
    );
    assert!(matches!(
        from_file("concurrent_tool_timeout = true", None),
        Err(Error::NotSeconds(_))
    ));
}

#[test]
fn the_step_limit_is_a_count_with_no_limit_for_zero_negatives_and_unlimited_spellings() {
    let none = Config::default();
    assert_eq!(steps(&none, None).unwrap(), 7);
    assert_eq!(steps(&none, Some("  ")).unwrap(), 7);
    assert_eq!(steps(&none, Some(" 30 ")).unwrap(), 30);
    assert_eq!(steps(&none, Some("2.9")).unwrap(), 2);
    for unlimited in [
        "0",
        "-1",
        "-5",
        "none",
        "Null",
        " UNLIMITED ",
        "infinite",
        "Infinity",
        "inf",
        "∞",
    ] {
        assert_eq!(
            steps(&none, Some(unlimited)).unwrap(),
            usize::MAX,
            "{unlimited}"
        );
    }
    let error = steps(&none, Some("many")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "engine.max_steps (ULEI_ENGINE_MAX_STEPS) must be a number of steps or unlimited"
    );
}

#[test]
fn the_config_file_sets_the_step_limit_when_the_environment_does_not() {
    assert_eq!(steps(&file("max_steps = 50"), None).unwrap(), 50);
    assert_eq!(steps(&file("max_steps = \"40\""), None).unwrap(), 40);
    assert_eq!(
        steps(&file("max_steps = \"none\""), None).unwrap(),
        usize::MAX
    );
    assert_eq!(steps(&file("max_steps = 0"), None).unwrap(), usize::MAX);
    assert_eq!(steps(&file("max_steps = \"\""), None).unwrap(), 7);
    assert_eq!(steps(&file("max_steps = 50"), Some("3")).unwrap(), 3);
    assert!(matches!(
        steps(&file("max_steps = true"), None),
        Err(Error::NotLimit(_))
    ));
}

#[test]
fn the_exa_base_url_is_required_and_https_only() {
    let url = |config: &Config, env: Option<&str>| {
        config.https_url(WEB_BASE_URL, env.map(OsString::from))
    };
    let config = file("web_base_url = \"HTTPS://exa.example\"");
    assert_eq!(url(&config, None).unwrap(), "HTTPS://exa.example");
    assert_eq!(
        url(&config, Some("https://other.example")).unwrap(),
        "https://other.example"
    );
    for refused in ["http://exa.example", "ftp://exa.example", "exa.example"] {
        let error = url(&config, Some(refused)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "engine.web_base_url (ULEI_ENGINE_WEB_BASE_URL) must be an https:// URL"
        );
    }
    assert!(matches!(
        url(&Config::default(), None),
        Err(Error::Missing(WEB_BASE_URL))
    ));
}
