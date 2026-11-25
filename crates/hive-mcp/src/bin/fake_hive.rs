//! `ulei-hive-mcp-fake-hive`, the stand-in for the hive's CLI in the tests: it appends its argv
//! and `HIVE_CONFIG` as one JSON line to the file `FAKE_HIVE_LOG` names beside `HIVE_CONFIG`
//! (`<HIVE_CONFIG>.log`) and answers as `hive.py` does:
//!
//! - `create`: `{"id": "h_0000000N", "created": true, "state": "queued", ...}`, N the line count;
//!   a title `reject` exits 2 with `hive: card rejected: synthetic refusal`;
//! - `list`: two cards; `show h_00000000`: exit 1 `no such card`; other ids a done card with a
//!   result and two reports; `steer` and `cancel`: their CLI line;
//! - a title `slow` sleeps 30 s; a title `flood` writes 8 MiB to stdout.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::process::ExitCode;

use serde_json::json;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = std::env::var("HIVE_CONFIG").unwrap_or_default();
    let env: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let log = format!("{config}.log");
    let count = std::fs::read_to_string(&log).map_or(0, |t| t.lines().count());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(f, "{}", json!({"argv": args, "env": env}));
    }
    let value = |flag: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(&format!("--{flag}=")))
            .unwrap_or_default()
            .to_owned()
    };
    let positional = |n: usize| {
        args.iter()
            .position(|a| a == "--")
            .and_then(|at| args.get(at + 1 + n))
            .cloned()
            .unwrap_or_default()
    };
    let mut out = std::io::stdout().lock();
    match args.first().map(String::as_str) {
        Some("create") => match value("title").as_str() {
            "reject" => {
                eprintln!("hive: card rejected: synthetic refusal");
                return ExitCode::from(2);
            }
            "slow" => std::thread::sleep(std::time::Duration::from_secs(30)),
            "flood" => {
                let chunk = "x".repeat(1 << 20);
                for _ in 0..8 {
                    let _ = out.write_all(chunk.as_bytes());
                }
            }
            _ => {
                let card = json!({"id": format!("h_{count:08x}"), "created": true, "state": "queued",
                    "card_class": "owner_task", "owner_visible": true});
                let _ = writeln!(out, "{card}");
            }
        },
        Some("list") => {
            let rows = json!([
                {"id": "h_0000000a", "state": "queued", "round": 0, "title": "Synthetic one", "kind": "task"},
                {"id": "h_0000000b", "state": "done", "round": 1, "title": "Synthetic two", "kind": "task"}
            ]);
            let _ = writeln!(out, "{rows}");
        }
        Some("show") => {
            let id = positional(0);
            if id == "h_00000000" {
                eprintln!("no such card");
                return ExitCode::from(1);
            }
            let card = json!({"id": id, "state": "done", "round": 1, "title": "Synthetic card",
                "result": "Synthetic result.\nmore", "runs": [{"report": "# old"}, {"report": "# Synthetic report\nbody"}],
                "events": [], "goal": "g"});
            let _ = writeln!(out, "{card:#}");
        }
        Some("steer") => {
            let _ = writeln!(
                out,
                "{} steer 1 recorded, not delivered yet: a new worker head gets it in the same workspace",
                positional(0)
            );
        }
        Some("cancel") => {
            let _ = writeln!(out, "{} cancelled", positional(0));
        }
        _ => {
            eprintln!("usage: hive.py ...");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
