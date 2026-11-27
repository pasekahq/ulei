//! Deadline, step limit, Exa base URL and tool-loop guardrail settings: the environment, then
//! the config file, then the default; and the file checks of `Config::read`.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fake_gateway::Scratch;
use ulei_engine::compress::{Prune, Settings as CompressionSettings};
use ulei_engine::config::{
    CONCURRENT_TOOL_TIMEOUT, Config, Error, MAX_STEPS, MAX_TIMEOUT, ROUTE_CONTEXT_LENGTH,
    ROUTE_MAX_TOKENS, WEB_BASE_URL,
};
use ulei_engine::policy::GuardrailConfig;

const DEFAULT: Duration = Duration::from_secs(420);

fn from_env(value: &str) -> Result<Option<Duration>, Error> {
    empty().timeout(
        CONCURRENT_TOOL_TIMEOUT,
        Some(OsString::from(value)),
        DEFAULT,
    )
}

/// `ulei.toml` of `text` in `dir`.
fn write(dir: &Scratch, text: &str) -> PathBuf {
    let path = dir.path("ulei.toml");
    fs::write(&path, text).unwrap();
    path
}

/// The file at `path` read with the setting variables `env` gives.
fn read(path: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> Result<Config, Error> {
    Config::read(Some(path), env)
}

/// A config file of `line` in the `[engine]` section.
fn file(line: &str) -> Config {
    let dir = Scratch::new();
    read(&write(&dir, &format!("[engine]\n{line}\n")), &|_| None).unwrap()
}

/// An empty config file.
fn empty() -> Config {
    let dir = Scratch::new();
    read(&write(&dir, ""), &|_| None).unwrap()
}

/// The error line of the file of `line` in the `[engine]` section, with `{path}` for its path.
fn refused(line: &str) -> String {
    let dir = Scratch::new();
    let path = write(&dir, &format!("[engine]\n{line}\n"));
    read(&path, &|_| None)
        .unwrap_err()
        .to_string()
        .replace(path.to_str().unwrap(), "{path}")
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
        empty()
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
    assert_eq!(
        from_file("concurrent_tool_timeout = \"soon\"", None)
            .unwrap_err()
            .to_string(),
        "engine.concurrent_tool_timeout (ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT) must be a number \
         of seconds"
    );
    assert_eq!(
        refused("concurrent_tool_timeout = true"),
        "config {path} line 2: engine.concurrent_tool_timeout must be an integer or a string"
    );
}

#[test]
fn the_step_limit_is_a_count_with_no_limit_for_zero_negatives_and_unlimited_spellings() {
    let none = empty();
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
        steps(&file("max_steps = \"many\""), None),
        Err(Error::NotLimit(_))
    ));
    assert_eq!(
        refused("max_steps = true"),
        "config {path} line 2: engine.max_steps must be an integer or a string"
    );
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
        url(&empty(), None),
        Err(Error::Missing(WEB_BASE_URL))
    ));
}

#[cfg(feature = "web-loopback-http")]
#[test]
fn the_test_switch_also_takes_a_loopback_http_exa_base_url() {
    let config = empty();
    let url = |env: &str| config.https_url(WEB_BASE_URL, Some(OsString::from(env)));
    for taken in [
        "http://127.0.0.1:8080",
        "http://[::1]:9/x",
        "HTTP://127.0.0.2",
    ] {
        assert_eq!(url(taken).unwrap(), taken);
    }
    for refused in [
        "http://localhost:8080",
        "http://10.0.0.1",
        "http://exa.example",
    ] {
        assert!(matches!(url(refused), Err(Error::NotHttps(_))), "{refused}");
    }
}

#[test]
fn the_tool_loop_guardrails_section_overrides_each_switch_and_count() {
    let no_env = |_: &str| None;
    assert_eq!(
        GuardrailConfig::from_settings(&empty(), &no_env).unwrap(),
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
        let error = GuardrailConfig::from_settings(&empty(), &env).unwrap_err();
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
    let config =
        file("# web search cap\n[engine.tool_loop_guardrails.loop_caps]\nmax_web_searches = -1");
    let error = GuardrailConfig::from_settings(&config, &|_| None).unwrap_err();
    assert_eq!(error.to_string(), message);
    assert_eq!(
        refused(
            "# web search cap\n[engine.tool_loop_guardrails.loop_caps]\nmax_web_searches = \"50\""
        ),
        "config {path} line 4: engine.tool_loop_guardrails.loop_caps.max_web_searches must be an \
         integer"
    );
    for value in ["-1", "two", "1.5", ""] {
        let env = |name: &str| {
            (name == "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES")
                .then(|| OsString::from(value))
        };
        let error = GuardrailConfig::from_settings(&empty(), &env).unwrap_err();
        assert_eq!(error.to_string(), message, "{value:?}");
    }
}

#[test]
fn the_compression_section_overrides_the_route_timeout_tail_floor_ratio_and_threshold() {
    let no_env = |_: &str| None;
    let defaults = CompressionSettings::from_settings(&empty(), &no_env, 16_384, None).unwrap();
    assert_eq!(
        defaults,
        CompressionSettings {
            route: "compact".into(),
            timeout: Some(Duration::from_secs(450)),
            max_tokens: 16_384,
            cache: None,
            protect_first_n: 3,
            protect_last_n: 20,
            target_ratio: 0.2,
            threshold: 0.3,
            threshold_tokens: Some(300_000),
            prune: Prune {
                tokens: 120_000,
                min_result_chars: 8000,
                min_reclaim_tokens: 4096,
            },
        }
    );
    let config = file(
        "[engine.compression]\n\
         route = \"summary\"\n\
         timeout = 90\n\
         protect_last_n = 6\n\
         target_ratio = \"0.35\"\n\
         threshold = \"0.5\"\n\
         threshold_tokens = 0\n\
         proactive_prune_tokens = 0\n\
         proactive_prune_min_result_chars = 100\n\
         proactive_prune_min_reclaim_tokens = 0",
    );
    let want = CompressionSettings {
        route: "summary".into(),
        timeout: Some(Duration::from_secs(90)),
        protect_last_n: 6,
        target_ratio: 0.35,
        threshold: 0.5,
        threshold_tokens: None,
        prune: Prune {
            tokens: 0,
            min_result_chars: 200,
            min_reclaim_tokens: 0,
        },
        ..defaults
    };
    assert_eq!(
        CompressionSettings::from_settings(&config, &no_env, 16_384, None).unwrap(),
        want
    );
    let env = |name: &str| {
        (name == "ULEI_ENGINE_COMPRESSION_TARGET_RATIO").then(|| OsString::from("0.5"))
    };
    assert_eq!(
        CompressionSettings::from_settings(&config, &env, 16_384, None)
            .unwrap()
            .target_ratio,
        0.5
    );
    let bad = file("[engine.compression]\ntarget_ratio = \"a fifth\"");
    assert!(matches!(
        CompressionSettings::from_settings(&bad, &no_env, 16_384, None),
        Err(Error::NotNumber(s)) if s.key == "engine.compression.target_ratio"
    ));
}

#[test]
fn a_route_alias_takes_its_window_and_reserve_from_its_section_or_its_variables() {
    let no_env = |_: &str| None;
    let config = file(
        "[engine.routes.chat]\n\
         context_length = 200000\n\
         max_tokens = 32000\n\
         [engine.routes.fast-lane]\n\
         context_length = 0",
    );
    let chat = |setting| config.route_positive(setting, "chat", &no_env).unwrap();
    assert_eq!(chat(ROUTE_CONTEXT_LENGTH), Some(200_000));
    assert_eq!(chat(ROUTE_MAX_TOKENS), Some(32_000));
    assert_eq!(
        config
            .route_positive(ROUTE_CONTEXT_LENGTH, "other", &no_env)
            .unwrap(),
        None
    );
    assert_eq!(
        config
            .route_positive(ROUTE_CONTEXT_LENGTH, "fast-lane", &no_env)
            .unwrap_err()
            .to_string(),
        "engine.routes.fast-lane.context_length (ULEI_ENGINE_ROUTES_FAST_LANE_CONTEXT_LENGTH) must be a positive integer"
    );
    let env = |name: &str| {
        (name == "ULEI_ENGINE_ROUTES_FAST_LANE_CONTEXT_LENGTH").then(|| OsString::from("64000"))
    };
    assert_eq!(
        config
            .route_positive(ROUTE_CONTEXT_LENGTH, "fast-lane", &env)
            .unwrap(),
        Some(64_000)
    );
}

#[test]
fn a_route_alias_with_a_dot_is_refused_by_name() {
    let no_env = |_: &str| None;
    let config = empty();
    let err = config
        .route_positive(ROUTE_CONTEXT_LENGTH, "v1.5", &no_env)
        .unwrap_err();
    assert!(matches!(&err, Error::RouteAlias(alias) if alias == "v1.5"));
    assert_eq!(
        err.to_string(),
        "route alias 'v1.5' contains '.', which the config file cannot name in engine.routes"
    );
    let env = |_: &str| Some(OsString::from("64000"));
    assert!(matches!(
        config.route_positive(ROUTE_MAX_TOKENS, "v1.5", &env),
        Err(Error::RouteAlias(_))
    ));
}

#[test]
fn an_unknown_key_anywhere_in_the_file_is_refused_with_its_line() {
    assert_eq!(
        refused("journal = \"/synthetic/journal.sqlite3\"\nmax_step = 5"),
        "config {path} line 3: unknown key engine.max_step"
    );
    assert_eq!(
        refused("[telegram]\ntoken = \"x\""),
        "config {path} line 3: unknown key telegram.token"
    );
}

#[test]
fn a_server_section_that_the_server_list_does_not_name_is_refused_with_its_line() {
    let dir = Scratch::new();
    let path = write(
        &dir,
        "[engine]\n\
         mcp_servers = \"docs\"\n\
         [engine.mcp.docs]\n\
         url = \"http://127.0.0.1:9/mcp\"\n\
         [engine.mcp.stale.tools]\n\
         prompts = false\n",
    );
    assert_eq!(
        read(&path, &|_| None).unwrap_err().to_string(),
        format!(
            "config {} line 5: section [engine.mcp.stale] is not listed in engine.mcp_servers \
             (ULEI_ENGINE_MCP_SERVERS)",
            path.display()
        )
    );
    let env = |name: &str| (name == "ULEI_ENGINE_MCP_SERVERS").then(|| OsString::from("docs"));
    assert!(matches!(
        read(&path, &env),
        Err(Error::Unlisted { line: 5, name, .. }) if name == "stale"
    ));
    let env =
        |name: &str| (name == "ULEI_ENGINE_MCP_SERVERS").then(|| OsString::from("docs, stale"));
    assert!(read(&path, &env).is_ok());
}

#[test]
fn a_server_section_header_without_keys_that_the_list_does_not_name_is_refused_with_its_line() {
    let dir = Scratch::new();
    let path = write(
        &dir,
        "[engine]\n\
         mcp_servers = \"docs\"\n\
         [engine.mcp.docs]\n\
         url = \"http://127.0.0.1:9/mcp\"\n\
         \n\
         [engine.mcp.ghost]\n",
    );
    assert_eq!(
        read(&path, &|_| None).unwrap_err().to_string(),
        format!(
            "config {} line 6: section [engine.mcp.ghost] is not listed in engine.mcp_servers \
             (ULEI_ENGINE_MCP_SERVERS)",
            path.display()
        )
    );
}
