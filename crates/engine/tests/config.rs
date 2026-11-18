//! Deadline settings: the environment, then the config file, then the default.

#![allow(missing_docs, clippy::unwrap_used)]

use std::ffi::OsString;
use std::fs;
use std::time::Duration;

use ulei_engine::config::{CONCURRENT_TOOL_TIMEOUT, Config, Error, MAX_TIMEOUT};

const DEFAULT: Duration = Duration::from_secs(420);

fn from_env(value: &str) -> Result<Option<Duration>, Error> {
    Config::default().timeout(
        CONCURRENT_TOOL_TIMEOUT,
        Some(OsString::from(value)),
        DEFAULT,
    )
}

fn from_file(line: &str, env: Option<&str>) -> Result<Option<Duration>, Error> {
    let path = std::env::temp_dir().join(format!(
        "ulei-config-{}-{}.toml",
        std::process::id(),
        line.len()
    ));
    fs::write(&path, format!("[engine]\n{line}\n")).unwrap();
    let config = Config::load(&path).unwrap();
    fs::remove_file(&path).unwrap();
    config.timeout(CONCURRENT_TOOL_TIMEOUT, env.map(OsString::from), DEFAULT)
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
