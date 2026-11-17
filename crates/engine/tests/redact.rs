//! Secret redaction through `Redactor`: each pattern family masks its shape and leaves a near
//! miss alone, the `code_file` and `file_read` variants, the URL credential flag and the switch.
//!
//! Fixture secrets are assembled at run time from split literals and low-entropy bodies, so no
//! line of this file looks like a credential.

#![allow(missing_docs)]

use ulei_engine::tools::redact::{Flags, Redactor};

/// A 24-character body of low entropy.
fn fake() -> String {
    "fake".repeat(6)
}

/// `fake()` behind the prefix `a` + `b`.
fn token(a: &str, b: &str) -> String {
    format!("{a}{b}{}", fake())
}

/// The head and tail mask of `fake()`.
const MASKED: &str = "fakefa...fake";

const ON: Redactor = Redactor { enabled: true };

fn redact(text: &str, flags: Flags) -> String {
    ON.sensitive_text(text, flags).into_owned()
}

fn plain(text: &str) -> String {
    redact(text, Flags::default())
}

const CODE: Flags = Flags {
    force: false,
    code_file: true,
    file_read: false,
    redact_url_credentials: false,
};
const FILE: Flags = Flags {
    force: false,
    code_file: false,
    file_read: true,
    redact_url_credentials: false,
};
const URLS: Flags = Flags {
    force: false,
    code_file: false,
    file_read: false,
    redact_url_credentials: true,
};

/// Each `(input, expected)` pair through the default passes.
fn table(cases: &[(String, String)]) {
    for (input, want) in cases {
        assert_eq!(&plain(input), want, "input {input:?}");
    }
}

fn same(text: impl Into<String>) -> (String, String) {
    let text = text.into();
    (text.clone(), text)
}

#[test]
fn prefixed_tokens_keep_head_and_tail_and_a_short_or_embedded_one_stays() {
    let gh = token("gh", "p_");
    table(&[
        (
            format!("push with {gh} done"),
            "push with ghp_fa...fake done".into(),
        ),
        (token("s", "k-"), "sk-fak...fake".into()),
        (token("AI", "za") + "fakefake", "AIzafa...fake".into()),
        (
            format!("AK{}{}", "IA", "FAKE".repeat(4)),
            "AKIAFA...FAKE".into(),
        ),
        (
            format!("x{}{}", "o", "xb-") + &fake(),
            "xoxb-f...fake".into(),
        ),
        same(format!("gh{}fake", "p_")),
        same(format!("x{gh}")),
        same(format!("{gh}_")),
    ]);
}

#[test]
fn a_prefixed_token_split_by_a_control_character_is_masked_across_it() {
    let split = format!("{}{}fake\x1b{}", "s", "k-", "fake".repeat(4));
    assert_eq!(plain(&format!("out {split} end")), "out sk-fak...fake end");
}

#[test]
fn environment_assignments_mask_secret_named_keys_only() {
    table(&[
        (
            format!("db_pw=\"{}\"", fake()),
            format!("db_pw=\"{MASKED}\""),
        ),
        (
            format!("MY_SECRET = '{}'", fake()),
            "MY_SECRET='***'".into(),
        ),
        (
            format!("OPENAI_API_KEY={}", fake()),
            "OPENAI_API_KEY=***".into(),
        ),
        ("MAX_TOKENS=100".into(), "MAX_TOKENS=***".into()),
        same("KEYBOARD=layout"),
        same(format!("set MYTOKEN={}", fake())),
        same(format!("note password={}", fake())),
        same("API_KEY=os.getenv('API_KEY')"),
    ]);
}

#[test]
fn config_and_yaml_keys_mask_values_at_a_word_boundary() {
    table(&[
        (
            format!("spring.datasource.password={}", fake()),
            format!("spring.datasource.password={MASKED}"),
        ),
        (
            format!("export api_key={}", fake()),
            "export api_key=***".into(),
        ),
        (
            format!("password: {}", fake()),
            format!("password: {MASKED}"),
        ),
        (
            format!("  clientSecret: {}", fake()),
            format!("  clientSecret: {MASKED}"),
        ),
        same("tokenizer: cl100k_base"),
        same("Secretary: J.Smith"),
        same("note: secret meeting"),
    ]);
}

#[test]
fn json_fields_mask_secret_named_string_values() {
    table(&[
        (
            format!("{{\"apiKey\" :\"{}\"}}", fake()),
            format!("{{\"apiKey\": \"{MASKED}\"}}"),
        ),
        (
            format!("{{\"token\": \"{}\"}}", "fake"),
            "{\"token\": \"***\"}".into(),
        ),
        same(format!("{{\"name\": \"{}\"}}", fake())),
        same("{\"apiKey\": \"os.environ['K']\"}"),
    ]);
}

#[test]
fn auth_and_secret_headers_mask_the_credential_and_keep_the_scheme() {
    table(&[
        (
            format!("Authorization: Bearer {}", fake()),
            format!("Authorization: Bearer {MASKED}"),
        ),
        (
            format!("curl -H \"proxy-authorization: Basic {}\" x", fake()),
            format!("curl -H \"proxy-authorization: Basic {MASKED}\" x"),
        ),
        (
            format!("curl -H x-api-key:{} https://api.example.com", fake()),
            format!("curl -H x-api-key:{MASKED} https://api.example.com"),
        ),
        same(format!("author: {}", fake())),
        same(format!("x-request-id: {}", fake())),
    ]);
}

#[test]
fn private_key_blocks_are_replaced_whole() {
    let block = |kind: &str| {
        format!(
            "-----BEGIN {kind}-----\n{}\n-----END {kind}-----",
            fake().repeat(3)
        )
    };
    table(&[
        (
            format!("key:\n{}\nend", block(&format!("RSA {} KEY", "PRIVATE"))),
            "key:\n[REDACTED PRIVATE KEY]\nend".into(),
        ),
        same(block("PUBLIC KEY")),
    ]);
}

#[test]
fn connection_strings_mask_the_password_and_code_keeps_a_template() {
    let template = "f\"postgresql://{user}:{password}@{host}/db\"";
    assert_eq!(
        plain(&format!("postgres://app:{}@db:5432/x", fake())),
        "postgres://app:***@db:5432/x"
    );
    assert_eq!(plain(template), "f\"postgresql://{user}:***@{host}/db\"");
    assert_eq!(redact(template, CODE), template);
    assert_eq!(
        plain("postgres://db:5432/x and user@host"),
        "postgres://db:5432/x and user@host"
    );
}

#[test]
fn jwts_are_masked_and_a_short_one_stays() {
    let jwt = format!("{}{}.{}.{}", "ey", "JfakefakefakeQ", fake(), "fake");
    assert_eq!(plain(&format!("jwt {jwt}")), "jwt eyJfak...fake");
    assert_eq!(plain("eyJshort.x"), "eyJshort.x");
}

#[test]
fn url_credentials_pass_unless_asked_for_and_a_bare_userinfo_token_is_masked() {
    let query = format!("https://example.com/cb?code={}&state=s#top", fake());
    let userinfo = format!("https://user:{}@example.com/x", fake());
    assert_eq!(plain(&query), query);
    assert_eq!(plain(&userinfo), userinfo);
    assert_eq!(
        redact(&query, URLS),
        "https://example.com/cb?code=***&state=s#top"
    );
    assert_eq!(redact(&userinfo, URLS), "https://user:***@example.com/x");
    assert_eq!(
        redact("see https://x.io/?Acc%65ss-Token=abc;page=2", URLS),
        "see https://x.io/?Acc%65ss-Token=***;page=2"
    );
    assert_eq!(
        plain(&format!(
            "git remote set-url o https://{}@github.com/o/r",
            fake()
        )),
        format!("git remote set-url o https://{MASKED}@github.com/o/r")
    );
    assert_eq!(
        plain("https://git@github.com/o/r"),
        "https://git@github.com/o/r"
    );
}

#[test]
fn a_whole_form_body_masks_sensitive_keys_and_other_text_stays() {
    table(&[
        (
            format!(" grant_type=x&code={} ", fake()),
            "grant_type=x&code=***".into(),
        ),
        same("page=2&sort=name&q=x"),
        same(format!("grant_type=x&code={}\nnext", fake())),
    ]);
}

#[test]
fn phone_numbers_keep_their_ends() {
    table(&[
        (
            "call +14155550123 now".into(),
            "call +141****0123 now".into(),
        ),
        ("+1234567".into(), "+1****67".into()),
        same("+14155550123abc"),
        same("+123456"),
    ]);
}

#[test]
fn telegram_bot_tokens_keep_the_bot_id() {
    let secret = format!("{}{}", "ABC", "fake".repeat(8));
    assert_eq!(plain(&format!("bot123456789:{secret}")), "bot123456789:***");
    assert_eq!(plain("12345:short"), "12345:short");
}

#[test]
fn code_files_skip_assignments_and_fields_but_mask_tokens() {
    let gh = token("gh", "p_");
    let source = format!(
        "MAX_TOKENS = 100\nAPI_KEY = \"{}\"\n{{\"apiKey\": \"{}\"}}\nGH = \"{gh}\"",
        fake(),
        fake()
    );
    assert_eq!(redact(&source, CODE), source.replace(&gh, "ghp_fa...fake"));
}

#[test]
fn file_reads_put_a_non_reusable_sentinel_for_tokens_and_skip_assignments() {
    let gh = token("gh", "p_");
    let text = format!(
        "github: {gh}\nOPENAI_API_KEY={}\nsk: {}",
        fake(),
        token("s", "k_")
    );
    assert_eq!(
        redact(&text, FILE),
        format!(
            "github: «redacted:ghp_…»\nOPENAI_API_KEY={}\nsk: «redacted:sk_…»",
            fake()
        )
    );
    assert_eq!(
        redact(&format!("Authorization: Bearer {}", fake()), FILE),
        format!("Authorization: Bearer {MASKED}")
    );
}

#[test]
fn the_switch_off_leaves_text_unless_the_call_is_forced() {
    let off = Redactor { enabled: false };
    let text = format!("OPENAI_API_KEY={}", fake());
    assert_eq!(off.sensitive_text(&text, Flags::default()), text);
    let forced = Flags {
        force: true,
        ..Flags::default()
    };
    assert_eq!(off.sensitive_text(&text, forced), "OPENAI_API_KEY=***");
}

#[test]
fn terminal_output_runs_assignments_for_environment_dumps_and_env_file_reads_only() {
    let line = format!("SERVICE_TOKEN={}", fake());
    let masked = "SERVICE_TOKEN=***";
    for command in [
        "env",
        "printenv | sort",
        "cd app && export",
        "cat .env",
        "head -n 5 'conf/.ENV.local'",
        "tail .envrc",
    ] {
        assert_eq!(ON.terminal_output(&line, command), masked, "{command}");
    }
    for command in [
        "cat settings.py",
        "cat .env.example",
        "grep -r TOKEN .",
        "echo env",
    ] {
        assert_eq!(ON.terminal_output(&line, command), line, "{command}");
    }
}
