//! Deadline, step limit, Exa base URL and tool-loop guardrail settings: the environment, then the
//! config file, then the default.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::ffi::OsString;
use std::fs;
use std::time::Duration;

use fake_gateway::Scratch;
use ulei_engine::config::{
    CONCURRENT_TOOL_TIMEOUT, Config, Error, MAX_STEPS, MAX_TIMEOUT, WEB_BASE_URL,
};
use ulei_engine::policy::GuardrailConfig;

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
    let dir = Scratch::new();
    let path = dir.path("config.toml");
    fs::write(&path, format!("[engine]\n{line}\n")).unwrap();
    Config::load(&path).unwrap()
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

#[test]
fn the_tool_loop_guardrails_section_overrides_each_switch_and_count() {
    let no_env = |_: &str| None;
    assert_eq!(
        GuardrailConfig::from_settings(&Config::default(), &no_env).unwrap(),
        GuardrailConfig::default()
    );
    let config = file(
        "[engine.tool_loop_guardrails]\n\
         warnings_enabled = false\n\
         hard_stop_enabled = \"on\"\n\
         [engine.tool_loop_guardrails.warn_after]\n\
         exact_failure = 11\n\
         same_tool_failure = 12\n\
         idempotent_no_progress = 13\n\
         [engine.tool_loop_guardrails.hard_stop_after]\n\
         exact_failure = 21\n\
         same_tool_failure = 22\n\
         idempotent_no_progress = 23",
    );
    let want = GuardrailConfig {
        warnings_enabled: false,
        hard_stop_enabled: true,
        exact_failure_warn_after: 11,
        exact_failure_block_after: 21,
        same_tool_failure_warn_after: 12,
        same_tool_failure_halt_after: 22,
        no_progress_warn_after: 13,
        no_progress_block_after: 23,
        ..GuardrailConfig::default()
    };
    assert_eq!(
        GuardrailConfig::from_settings(&config, &no_env).unwrap(),
        want
    );
    let env = |name: &str| {
        (name == "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_SAME_TOOL_FAILURE")
            .then(|| OsString::from("4"))
    };
    assert_eq!(
        GuardrailConfig::from_settings(&config, &env).unwrap(),
        GuardrailConfig {
            same_tool_failure_halt_after: 4,
            ..want
        }
    );
}

#[test]
fn a_tool_loop_guardrails_count_that_is_not_positive_is_a_settings_error_naming_its_key() {
    let config = file("[engine.tool_loop_guardrails.warn_after]\nexact_failure = 0");
    let error = GuardrailConfig::from_settings(&config, &|_| None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "engine.tool_loop_guardrails.warn_after.exact_failure \
         (ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_EXACT_FAILURE) must be a positive integer"
    );
    for value in ["-1", "two", ""] {
        let env = |name: &str| {
            (name == "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_IDEMPOTENT_NO_PROGRESS")
                .then(|| OsString::from(value))
        };
        let error = GuardrailConfig::from_settings(&Config::default(), &env).unwrap_err();
        assert_eq!(
            error.to_string(),
            "engine.tool_loop_guardrails.hard_stop_after.idempotent_no_progress \
             (ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_IDEMPOTENT_NO_PROGRESS) must be a \
             positive integer",
            "{value:?}"
        );
    }
}

#[test]
fn stall_guards_and_the_web_search_cap_come_from_the_file_then_the_environment() {
    let config = file(
        "stall_guards = false\n\
         [engine.tool_loop_guardrails.loop_caps]\n\
         max_web_searches = 0",
    );
    let want = GuardrailConfig {
        stall_guards: false,
        max_web_searches: 0,
        ..GuardrailConfig::default()
    };
    assert_eq!(
        GuardrailConfig::from_settings(&config, &|_| None).unwrap(),
        want
    );
    let env = |name: &str| match name {
        "ULEI_ENGINE_STALL_GUARDS" => Some(OsString::from("on")),
        "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES" => Some(OsString::from("7")),
        _ => None,
    };
    assert_eq!(
        GuardrailConfig::from_settings(&config, &env).unwrap(),
        GuardrailConfig {
            stall_guards: true,
            max_web_searches: 7,
            ..want
        }
    );
}

#[test]
fn a_web_search_cap_that_is_negative_or_not_an_integer_is_a_settings_error_naming_its_key() {
    let message = "engine.tool_loop_guardrails.loop_caps.max_web_searches \
                   (ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES) must be a \
                   non-negative integer";
    for line in ["max_web_searches = -1", "max_web_searches = \"50\""] {
        let config = file(&format!(
            "# web search cap\n[engine.tool_loop_guardrails.loop_caps]\n{line}"
        ));
        let error = GuardrailConfig::from_settings(&config, &|_| None).unwrap_err();
        assert_eq!(error.to_string(), message, "{line}");
    }
    for value in ["-1", "two", "1.5", ""] {
        let env = |name: &str| {
            (name == "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES")
                .then(|| OsString::from(value))
        };
        let error = GuardrailConfig::from_settings(&Config::default(), &env).unwrap_err();
        assert_eq!(error.to_string(), message, "{value:?}");
    }
}
