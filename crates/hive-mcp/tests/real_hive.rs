//! `ulei-hive-mcp` on the real hive CLI of `hive-py/` with `hive-py/config.example.toml`, its
//! paths moved into a scratch directory, on a fresh store and with no dispatcher: create, list,
//! show, steer and cancel, an idempotent create, a card the hive itself refuses, and the reminder
//! verbs on a reminder and on an alert of the real `hive.py watchdog`. Needs
//! `python3` 3.11 or later on `PATH` (the hive's `tomllib`); the CI test job has 3.12.

#![allow(missing_docs, clippy::unwrap_used, clippy::too_many_lines)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
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
        json!({"card_id": id, "created": false, "state": "created", "card_class": "owner_task", "owner_visible": true})
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

    // The read-only snapshots of the real CLI.
    let inspected = ok("hive_inspect", json!({"card_id": id}));
    assert_eq!(inspected["state"], "created");
    assert_eq!(inspected["snapshot"]["card_id"], id.as_str());
    assert_eq!(inspected["snapshot"]["queue"]["column"], "waiting");
    // accept_review is the judge's acceptance only: no reviewer unless `review: true`.
    assert_eq!(inspected["snapshot"]["card"]["acceptance_kind"], "judge");
    assert_eq!(inspected["snapshot"]["card"]["review"], 0, "{inspected}");
    assert!(inspected.get("waiting_on").is_none(), "{inspected}");
    let health = ok("hive_health", json!({}));
    assert_eq!(health["ticking"], false, "no dispatcher runs");
    assert_eq!(health["slots_used"], 0);
    assert_eq!(
        ok("hive_list", json!({"query": "SYNTHETIC delegation"}))["cards"][0]["card_id"],
        id.as_str()
    );
    assert_eq!(
        ok("hive_list", json!({"placement": "hidden"})),
        json!({"cards": []})
    );
    let outside = server.call(
        &session,
        "hive_file",
        &json!({"card_id": id, "path": "@out/other.md"}),
    );
    assert_eq!(outside["isError"], true, "{outside}");

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

/// Whether the hive's one runtime package, `cronsim`, imports in the environment the server
/// gives the CLI (`HOME` the scratch directory): the hive needs it for `--cron`.
fn has_cronsim(home: &Path) -> bool {
    Command::new(python())
        .args(["-c", "import cronsim"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// RFC 3339 of `days` from now at 09:00 UTC, as the model writes a time.
fn days_ahead(days: u64) -> String {
    let out = Command::new(python())
        .args([
            "-c",
            "import datetime as d, sys; t = d.datetime.now(d.timezone.utc) + d.timedelta(days=int(sys.argv[1])); print(t.strftime('%Y-%m-%dT09:00:00+00:00'))",
            &days.to_string(),
        ])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn reminders_and_series_on_the_real_hive() {
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
    let ok = |args: Value| {
        let result = server.call(&session, "hive_create", &args);
        assert_eq!(result["isError"], false, "{result} {:?}", server.stderr());
        result["structuredContent"].clone()
    };
    let show = |id: &str| -> Value {
        let out = Command::new(python())
            .arg(repo().join("hive-py/scripts/hive.py"))
            .args(["show", "--json", id])
            .env("HIVE_CONFIG", dir.0.join("hive-config.toml"))
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let tomorrow = days_ahead(1);
    let later = days_ahead(3);

    // "Remind me tomorrow at 9": one card held until then.
    let reminder = ok(json!({
        "title": "Synthetic reminder", "goal": "Tell the owner about the synthetic call.",
        "acceptance": "true", "step": 1, "source": "msg:1002", "not_before": tomorrow}));
    assert_eq!(reminder["created"], true);
    assert_eq!(reminder["state"], "created");
    assert_eq!(reminder["card_class"], "owner_task");
    assert_eq!(reminder["owner_visible"], true);
    let card = show(reminder["card_id"].as_str().unwrap());
    assert_eq!(card["kind"], "task");
    let held = card["not_before"].as_f64().unwrap();
    let want = Command::new(python())
        .args([
            "-c",
            "import datetime as d, sys; print(int(d.datetime.fromisoformat(sys.argv[1]).timestamp()))",
            &tomorrow,
        ])
        .output()
        .unwrap();
    let want: f64 = String::from_utf8(want.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!((held - want).abs() < 1.0, "{held} {want}");
    assert_eq!(card["deliver"], "owner-inbox");

    // "Watch every 15 minutes until then": a series of `every`.
    let every = ok(json!({
        "title": "Synthetic watch", "goal": "Check the synthetic page; report news.",
        "acceptance": "true", "step": 1, "source": "msg:1003", "every": "15m", "until": later,
        "owner_visible": true}));
    assert_eq!(every["card_class"], "series_run");
    assert_eq!(every["owner_visible"], true);
    assert_eq!(every["state"], "blocked");
    let card = show(every["card_id"].as_str().unwrap());
    assert_eq!(card["kind"], "recurring");
    assert_eq!(card["schedule"]["every"], 900.0);
    assert_eq!(card["deliver"], "owner-inbox");
    assert_eq!(card["created_by"], "secretary");

    // The hive's own rules still apply: an `until` in the past is its refusal.
    let past = server.call(
        &session,
        "hive_create",
        &json!({"title": "T", "goal": "G", "acceptance": "true", "step": 1, "source": "manual",
            "every": "1h", "until": "2001-01-01T00:00:00Z"}),
    );
    assert_eq!(past["isError"], true);
    assert_eq!(
        past["content"][0]["text"],
        "card rejected: until must be in the future"
    );

    if !has_cronsim(&dir.0) {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI installs hive-py/requirements.lock for this test"
        );
        eprintln!(
            "cron part skipped: python3 has no cronsim (pip install -r hive-py/requirements.lock)"
        );
        return;
    }
    // "Every weekday at 9 Moscow time, ten times": a cron series.
    let cron = ok(json!({
        "title": "Synthetic digest", "goal": "Collect the synthetic digest.",
        "acceptance": "true", "step": 1, "source": "msg:1004", "cron": "0 9 * * 1-5",
        "tz": "Europe/Moscow", "max_runs": 10}));
    assert_eq!(cron["card_class"], "series_run");
    assert_eq!(cron["owner_visible"], false);
    let card = show(cron["card_id"].as_str().unwrap());
    assert_eq!(card["schedule"]["cron"], "0 9 * * 1-5");
    assert_eq!(card["schedule"]["tz"], "Europe/Moscow");
    assert_eq!(card["schedule"]["max_runs"], 10);
    let zone = server.call(
        &session,
        "hive_create",
        &json!({"title": "T", "goal": "G", "acceptance": "true", "step": 1, "source": "manual",
            "cron": "0 9 * * *", "tz": "Nowhere/Synthetic"}),
    );
    assert_eq!(
        zone["content"][0]["text"],
        "card rejected: invalid cron expression or time zone"
    );
}

/// Puts card `id` of the scratch store into the wait a head's `RESULT: disputed` leaves: state
/// `blocked`, wait `disputed`, round 1 (the store is the hive's SQLite file; no dispatcher runs).
fn dispute(dir: &Path, id: &str) {
    let out = Command::new(python())
        .args([
            "-c",
            "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.execute(\"UPDATE cards SET state='blocked', wait_reason='disputed', block_reason='disputed by the worker: synthetic', round=1, version=version+1 WHERE id=?\", (sys.argv[2],)); c.commit(); print(c.total_changes)",
            &dir.join("srv/data/hive.db").display().to_string(),
            id,
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1", "{out:?}");
}

#[test]
fn resolve_decides_only_a_disputed_card_of_its_author_on_the_real_hive() {
    let dir = Scratch::new();
    let command = format!(
        "{} {}",
        python().display(),
        repo().join("hive-py/scripts/hive.py").display()
    );
    let hive = hive_config(&dir.0);
    let server = Running::start(&write_config(
        &dir.0,
        &command,
        &hive,
        "timeout = 60\n",
        0o600,
    ));
    let session = server.session();
    let call = |name: &str, args: Value| server.call(&session, name, &args);
    let show = |id: &str| -> Value {
        let out = Command::new(python())
            .arg(repo().join("hive-py/scripts/hive.py"))
            .args(["show", "--json", id])
            .env("HIVE_CONFIG", &hive)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let create = |title: &str| -> String {
        let card = call(
            "hive_create",
            json!({"title": title, "goal": "Write a synthetic summary.", "accept_review": "The report names the notes.",
                "step": 2, "source": "msg:1003"}),
        );
        assert_eq!(card["isError"], false, "{card}");
        card["structuredContent"]["card_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // A card that is not disputed is refused with the hive's own text, and stays as it was.
    let id = create("Synthetic disputed card");
    let early = call(
        "hive_resolve",
        json!({"card_id": id, "action": "accept", "text": "ok"}),
    );
    assert_eq!(early["isError"], true, "{early}");
    let said = early["content"][0]["text"].as_str().unwrap();
    assert!(
        said.starts_with(&format!("resolve rejected: card {id} is not disputed")),
        "{said}"
    );
    assert_eq!(show(&id)["state"], "created");

    // rework: back to the queue, the text is the next round's instructions; an option-like text
    // stays the text.
    dispute(&dir.0, &id);
    let reworked = call(
        "hive_resolve",
        json!({"card_id": id, "action": "rework", "text": "--by=owner Work in the synthetic notes."}),
    );
    assert_eq!(reworked["isError"], false, "{reworked}");
    assert_eq!(
        reworked["structuredContent"],
        json!({"ok": true, "message": format!("{id} dispute resolved: rework, now queued")})
    );
    let card = show(&id);
    assert_eq!(card["state"], "queued");
    assert!(
        card["feedback"]
            .as_str()
            .unwrap()
            .ends_with("Follow these instructions:\n--by=owner Work in the synthetic notes."),
        "{card}"
    );
    let resolved = card["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "dispute_resolved")
        .unwrap();
    assert_eq!(resolved["data"]["by"], "secretary");
    assert_eq!(resolved["data"]["decision"], "rework");

    // accept: done with the reason.
    dispute(&dir.0, &id);
    let accepted = call(
        "hive_resolve",
        json!({"card_id": id, "action": "accept", "text": "The head is right."}),
    );
    assert_eq!(accepted["isError"], false, "{accepted}");
    assert_eq!(
        accepted["structuredContent"]["message"],
        format!("{id} dispute resolved: accept, now done")
    );
    let card = show(&id);
    assert_eq!(card["state"], "done");
    assert_eq!(
        card["result"],
        "dispute resolved by secretary: accepted: The head is right."
    );

    // The server's actor decides only its own cards: another actor is refused by the hive.
    let other = create("Synthetic disputed card of the secretary");
    dispute(&dir.0, &other);
    drop(server);
    let stranger = Running::start(&write_config(
        &dir.0,
        &command,
        &hive,
        "timeout = 60\nactor = \"synthetic-bot\"\n",
        0o600,
    ));
    let session = stranger.session();
    let refused = stranger.call(
        &session,
        "hive_resolve",
        &json!({"card_id": other, "action": "accept", "text": "ok"}),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    let said = refused["content"][0]["text"].as_str().unwrap();
    assert!(
        said.starts_with("resolve rejected: a dispute is decided by the card's author secretary"),
        "{said}"
    );
    assert_eq!(show(&other)["wait_reason"], "disputed");
}

/// Runs `code` under the scratch store's `python3` with the store's path as `sys.argv[1]` and the
/// other `args` after it; returns its stdout.
fn store_python(dir: &Path, code: &str, args: &[&str]) -> String {
    let out = Command::new(python())
        .args([
            "-c",
            code,
            &dir.join("srv/data/hive.db").display().to_string(),
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Puts reminder card `id` into the state its `not_before` leaves it in: due, waiting `blocked`
/// with wait `reminder` (no dispatcher runs).
fn make_due(dir: &Path, id: &str) {
    let changed = store_python(
        dir,
        "import sqlite3, sys, time; c = sqlite3.connect(sys.argv[1]); c.execute(\"UPDATE cards SET state='blocked', wait_reason='reminder', version=version+1 WHERE id=?\", (sys.argv[2],)); c.execute(\"INSERT INTO reminders(card_id,state,due_at,updated_at) VALUES(?,'due',?,?)\", (sys.argv[2], time.time(), time.time())); c.commit(); print(c.total_changes)",
        &[id],
    );
    assert_eq!(changed.trim(), "2");
}

#[test]
fn reminders_and_watchdog_alerts_are_claimed_sent_and_withdrawn_on_the_real_hive() {
    let dir = Scratch::new();
    let hive_py = repo().join("hive-py/scripts/hive.py");
    let command = format!("{} {}", python().display(), hive_py.display());
    // The example's outside sources of the watchdog do not exist here: their conditions are off.
    let hive = hive_config(&dir.0);
    let text = fs::read_to_string(&hive).unwrap();
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with("escalations_command") && !l.starts_with("bridge_unit"))
        .collect();
    fs::write(&hive, kept.join("\n") + "\n").unwrap();
    // `ulei-tg notify` of the example's [reminders]: the hive tests' fake, recording each call.
    let tg = dir.0.join("tg");
    fs::create_dir_all(&tg).unwrap();
    fs::create_dir_all(dir.0.join("srv/bin")).unwrap();
    let fake = dir.0.join("srv/bin/ulei-tg");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\nexec {} {} {} \"$@\"\n",
            python().display(),
            repo().join("hive-py/tests/hive/fake_ulei_tg.py").display(),
            tg.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let server = Running::start(&write_config(
        &dir.0,
        &command,
        &hive,
        "timeout = 60\n",
        0o600,
    ));
    let session = server.session();
    let call = |name: &str, args: Value| server.call(&session, name, &args);
    let ok = |name: &str, args: Value| {
        let result = call(name, args);
        assert_eq!(
            result["isError"],
            false,
            "{name}: {result} {:?}",
            server.stderr()
        );
        result["structuredContent"].clone()
    };
    let refusal = |args: Value| {
        let result = call("hive_reminder", args);
        assert_eq!(result["isError"], true, "{result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    };
    let show = |id: &str| -> Value {
        let out = Command::new(python())
            .arg(&hive_py)
            .args(["show", "--json", id])
            .env("HIVE_CONFIG", &hive)
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let tg_calls = || -> Vec<Value> {
        fs::read_to_string(tg.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    // A reminder: its text verbatim, no head.
    let words = "  Synthetic reminder: --by=owner\nwater the plants ";
    let created = ok(
        "hive_create",
        json!({"title": "Synthetic reminder", "goal": "Remind the owner.", "acceptance": "true",
            "step": 1, "source": "msg:1005", "not_before": days_ahead(1), "reminder": words}),
    );
    let id = created["card_id"].as_str().unwrap().to_owned();
    assert_eq!(created["state"], "created");
    assert_eq!(show(&id)["reminder"], words);
    assert_eq!(
        refusal(json!({"card_id": id, "action": "claim"})),
        format!("reminder claim refused: reminder {id} is not due yet (card created)")
    );
    let plain = ok(
        "hive_create",
        json!({"title": "Synthetic plain card", "goal": "G", "acceptance": "true", "step": 1,
            "source": "msg:1006"}),
    )["card_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        refusal(json!({"card_id": plain, "action": "claim"})),
        format!("reminder claim refused: card {plain} is not a reminder")
    );

    make_due(&dir.0, &id);
    assert_eq!(
        refusal(json!({"card_id": id, "action": "send"})),
        format!("reminder send refused: reminder {id} is due; this needs claimed")
    );
    assert_eq!(
        ok("hive_reminder", json!({"card_id": id, "action": "claim"})),
        json!({"ok": true, "message": format!("{id} reminder claimed by secretary")})
    );
    assert_eq!(
        refusal(json!({"card_id": id, "action": "claim"})),
        format!("reminder claim refused: reminder {id} is claimed; this needs due")
    );
    let edited = "--by=owner Water the plants now.";
    let sent = ok(
        "hive_reminder",
        json!({"card_id": id, "action": "send", "text": edited}),
    );
    let receipt: Value = serde_json::from_str(sent["message"].as_str().unwrap()).unwrap();
    assert_eq!(
        receipt,
        json!({"card": id, "state": "delivered",
            "receipt": {"chats": [{"chat_id": 4242, "message_ids": [101]}]}})
    );
    let calls = tg_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["stdin"], edited);
    assert_eq!(
        calls[0]["argv"],
        json!([
            "notify",
            "--config",
            dir.0.join("srv/ulei.toml").display().to_string()
        ])
    );
    let card = show(&id);
    assert_eq!(card["state"], "done");
    assert_eq!(card["reminder_state"]["sent_by"], "secretary");
    assert_eq!(
        refusal(json!({"card_id": id, "action": "withdraw", "reason": "late"})),
        format!("reminder withdraw refused: reminder {id} is delivered; this needs claimed")
    );

    // A watchdog alert of the real `hive.py watchdog`, acted on the same way.
    store_python(
        &dir.0,
        "import json, sqlite3, sys, time; c = sqlite3.connect(sys.argv[1]); c.execute(\"INSERT INTO meta(key,value,at) VALUES('provider_cooldown:claude',?,?)\", (json.dumps({'failures': 3, 'until': time.time() + 86400}), time.time())); c.commit()",
        &[],
    );
    let out = Command::new(python())
        .arg(&hive_py)
        .arg("watchdog")
        .env("HIVE_CONFIG", &hive)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["errors"], json!([]), "{summary}");
    let fired = summary["fired"].as_array().unwrap();
    assert_eq!(fired.len(), 1, "{summary}");
    assert_eq!(fired[0]["key"], "vendor_cooldown:claude");
    let alert = fired[0]["card"].as_str().unwrap().to_owned();
    let card = show(&alert);
    assert_eq!(card["wait_reason"], "reminder");
    assert_eq!(card["created_by"], "watchdog");
    assert!(
        card["reminder"]
            .as_str()
            .unwrap()
            .starts_with("Hive watchdog: vendor claude is cooling down after 3 provider failures"),
        "{card}"
    );
    ok(
        "hive_reminder",
        json!({"card_id": alert, "action": "claim"}),
    );
    fs::write(tg.join("exits.json"), "[1]").unwrap();
    assert_eq!(
        refusal(json!({"card_id": alert, "action": "send"})),
        "reminder send failed: ulei-tg notify: exit 1: ulei-tg notify: sent 0 of 1 messages, then telegram network: synthetic failure 2"
    );
    assert_eq!(show(&alert)["reminder_state"]["state"], "claimed");
    assert_eq!(
        ok(
            "hive_reminder",
            json!({"card_id": alert, "action": "withdraw", "reason": "--by=owner The owner knows."}),
        ),
        json!({"ok": true, "message": format!("{alert} reminder withdrawn")})
    );
    let card = show(&alert);
    assert_eq!(card["state"], "cancelled");
    assert_eq!(
        card["reminder_state"]["reason"],
        "--by=owner The owner knows."
    );
    assert_eq!(tg_calls().len(), 2);
}
