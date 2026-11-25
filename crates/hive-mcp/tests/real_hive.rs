//! `ulei-hive-mcp` on the real hive CLI of `hive-py/` with `hive-py/config.example.toml`, its
//! paths moved into a scratch directory, on a fresh store and with no dispatcher: create, list,
//! show, steer and cancel, an idempotent create, and a card the hive itself refuses. Needs
//! `python3` 3.11 or later on `PATH` (the hive's `tomllib`); the CI test job has 3.12.

#![allow(missing_docs, clippy::unwrap_used, clippy::too_many_lines)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Running, Scratch, write_config};
use serde_json::{Value, json};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The absolute path of `python3` on `PATH`, checked to be 3.11 or later.
fn python() -> PathBuf {
    let found = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("python3"))
        .find(|p| p.is_file())
        .expect("python3 on PATH: the real hive test runs the hive's CLI");
    let out = Command::new(&found)
        .args(["-c", "import sys; print(sys.version_info >= (3, 11))"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "True",
        "python3 3.11 or later"
    );
    found
}

/// The hive's example config with its paths under `dir`.
fn hive_config(dir: &Path) -> PathBuf {
    let hive = repo().join("hive-py");
    let srv = dir.join("srv");
    fs::create_dir_all(srv.join("data")).unwrap();
    let text = fs::read_to_string(hive.join("config.example.toml"))
        .unwrap()
        .replace("/srv/hive", &srv.display().to_string());
    let library = hive.join("tests/hive/fake_inbox_library");
    let text: String = text
        .lines()
        .map(|l| {
            if l.starts_with("inbox_library =") {
                format!("inbox_library = \"{}\"", library.display())
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let config = dir.join("hive-config.toml");
    fs::write(&config, text + "\n").unwrap();
    for entry in fs::read_dir(hive.join("texts")).unwrap() {
        let entry = entry.unwrap();
        fs::create_dir_all(dir.join("texts")).unwrap();
        fs::copy(entry.path(), dir.join("texts").join(entry.file_name())).unwrap();
    }
    config
}

#[test]
fn create_list_show_steer_cancel_on_the_real_hive_without_a_dispatcher() {
    let dir = Scratch::new();
    let command = format!(
        "{} {}",
        python().display(),
        repo().join("hive-py/scripts/hive.py").display()
    );
    let server = Running::start(&write_config(
        &dir.0,
        &command,
        &hive_config(&dir.0),
        "timeout = 60\n",
        0o600,
    ));
    let session = server.session();

    let ok = |name: &str, args: Value| {
        let result = server.call(&session, name, &args);
        assert_eq!(
            result["isError"],
            false,
            "{name}: {result} {:?}",
            server.stderr()
        );
        result["structuredContent"].clone()
    };
    let card = ok(
        "hive_create",
        json!({
        "title": "--synthetic delegation", "goal": "Write a synthetic summary.",
        "details": "Use the synthetic notes only.", "accept_review": "The report names the notes.",
        "step": 2, "source": "msg:1001", "idem_key": "req-1"}),
    );
    let id = card["card_id"].as_str().unwrap().to_owned();
    assert_eq!(card["created"], true);
    assert_eq!(card["state"], "created");
    let again = ok(
        "hive_create",
        json!({
        "title": "--synthetic delegation", "goal": "Write a synthetic summary.",
        "accept_review": "The report names the notes.", "step": 2, "source": "msg:1001", "idem_key": "req-1"}),
    );
    assert_eq!(
        again,
        json!({"card_id": id, "created": false, "state": "created"})
    );

    let listed = ok("hive_list", json!({}));
    assert_eq!(
        listed,
        json!({"cards": [{"card_id": id, "state": "created", "round": 0, "title": "--synthetic delegation"}]})
    );
    assert_eq!(
        ok("hive_list", json!({"state": "done"})),
        json!({"cards": []})
    );
    let shown = ok("hive_show", json!({"card_id": id}));
    assert_eq!(
        shown,
        json!({"card_id": id, "state": "created", "round": 0, "title": "--synthetic delegation"})
    );

    // No dispatcher runs: the steer waits for the next head.
    let steered = ok(
        "hive_steer",
        json!({"card_id": id, "note": "--by=owner use the notes"}),
    );
    assert_eq!(steered["ok"], true);
    let message = steered["message"].as_str().unwrap();
    assert!(
        message.contains("not delivered yet: a new worker head gets it"),
        "{message}"
    );

    let cancelled = ok(
        "hive_cancel",
        json!({"card_id": id, "reason": "Synthetic: no longer needed."}),
    );
    assert_eq!(
        cancelled,
        json!({"ok": true, "message": format!("{id} cancelled")})
    );
    let again = ok(
        "hive_cancel",
        json!({"card_id": id, "reason": "Synthetic: again."}),
    );
    assert_eq!(
        again["message"],
        format!("{id} cancelled (already cancelled)")
    );
    let shown = ok("hive_show", json!({"card_id": id}));
    assert_eq!(shown["state"], "cancelled");
    assert_eq!(shown["result_line"], "Synthetic: no longer needed.");
    assert_eq!(
        ok("hive_list", json!({})),
        json!({"cards": []}),
        "cancelled cards are left out"
    );

    // The hive's own refusals come back as tool errors with its text.
    let bad_bash = server.call(
        &session,
        "hive_create",
        &json!({
        "title": "T", "goal": "G", "acceptance": "if then", "step": 1, "source": "manual"}),
    );
    assert_eq!(bad_bash["isError"], true);
    let said = bad_bash["content"][0]["text"].as_str().unwrap();
    assert!(
        said.starts_with("card rejected: acceptance is not valid bash"),
        "{said}"
    );
    let missing = server.call(&session, "hive_show", &json!({"card_id": "h_00000000"}));
    assert_eq!(missing["content"][0]["text"], "no such card h_00000000");

    // The engine's card is attributable: created_by is the configured actor.
    let out = Command::new(python())
        .arg(repo().join("hive-py/scripts/hive.py"))
        .args(["show", "--json", &id])
        .env("HIVE_CONFIG", dir.0.join("hive-config.toml"))
        .output()
        .unwrap();
    let card: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(card["created_by"], "secretary");
    assert_eq!(card["body"], "Use the synthetic notes only.");
    assert_eq!(card["acceptance_kind"], "judge");
    assert_eq!(card["card_class"], "owner_task");
    let steers = card["steers"].as_array().unwrap();
    assert_eq!(steers[0]["text"], "--by=owner use the notes");
    assert_eq!(steers[0]["by"], "secretary");
}
