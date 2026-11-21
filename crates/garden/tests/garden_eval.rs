//! The `garden-eval` binary on a small fictional corpus: every passage is embedded unless
//! `--dense-head` limits it, and `--min-recall` / `--max-p50-ms` fail the run with a line naming
//! the missed threshold.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// A corpus whose answer sits in the second passage of `notes/birds.md`, behind an English first
/// section; `notes/battery.md` shares part of the query's trigrams. The glued query words match
/// no lexical token.
fn corpus(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ulei-garden-eval-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("corpus/notes")).unwrap();
    std::fs::write(
        root.join("corpus/notes/birds.md"),
        "# Birds\n\nRobins nest in the hedge by the river path every April and the wrens follow \
         a week later. The heron stands on the weir at dawn.\n\n## Keys\n\n\
         Запасной ключ от гаража висит за синим почтовым ящиком у калитки.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("corpus/notes/battery.md"),
        "# Battery\n\nЗапасной аккумулятор для фонаря лежит в ящике кладовки.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("gold.jsonl"),
        "{\"id\":\"k1\",\"query\":\"запаснойключ отгаража\",\"category\":\"keys\",\
         \"expected_source\":\"garden\",\"gold_paths\":[\"notes/birds.md\"]}\n",
    )
    .unwrap();
    root
}

fn garden_eval(root: &Path, extra: &[&str]) -> Output {
    let gold = root.join("gold.jsonl");
    let corpus = root.join("corpus");
    let mut args = vec![
        "--gold".as_ref(),
        gold.as_os_str(),
        "--corpus".as_ref(),
        corpus.as_os_str(),
        "--embedder".as_ref(),
        "fake".as_ref(),
        "--k".as_ref(),
        "1".as_ref(),
    ];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    Command::new(env!("CARGO_BIN_EXE_garden-eval"))
        .args(args)
        .output()
        .unwrap()
}

fn report(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn dense_side_finds_an_answer_beyond_the_first_passage_by_default() {
    let root = corpus("dense");
    let all = report(&garden_eval(&root, &[]));
    let head = report(&garden_eval(&root, &["--dense-head", "1"]));
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        (
            all["build"]["facts"].as_u64(),
            all["build"]["files"].as_u64()
        ),
        (Some(3), Some(2))
    );
    assert_eq!(all["build"]["embedded"], all["build"]["facts"]);
    assert_eq!(all["lexical"]["recall@1"], 0.0, "{all:#}");
    assert_eq!(all["hybrid"]["recall@1"], 1.0, "{all:#}");

    assert_eq!(head["build"]["embedded"], 2);
    assert_eq!(head["hybrid"]["recall@1"], 0.0, "{head:#}");
}

#[test]
fn the_time_of_one_passage_is_reported_with_an_embedder_only() {
    let root = corpus("passage");
    let fake = report(&garden_eval(&root, &[]));
    let none = report(&garden_eval(&root, &["--embedder", "none"]));
    std::fs::remove_dir_all(&root).unwrap();

    let ms = |q: &str| fake["build"]["passage_ms"][q].as_f64().unwrap();
    assert!(
        0.0 <= ms("p50") && ms("p50") <= ms("p95") && ms("p95") <= ms("max"),
        "{fake:#}"
    );
    assert!(
        ms("max") <= fake["build"]["embed_ms"].as_f64().unwrap(),
        "{fake:#}"
    );
    assert_eq!(none["build"]["passage_ms"], Value::Null);
}

#[test]
fn thresholds_fail_the_run_with_a_line_per_miss() {
    let root = corpus("thresholds");
    let pass = garden_eval(&root, &["--max-p50-ms", "10000"]);
    let recall = garden_eval(&root, &["--min-recall", "0.5"]);
    let p50 = garden_eval(&root, &["--max-p50-ms", "0"]);
    let bad = garden_eval(&root, &["--min-recall", "-1"]);
    std::fs::remove_dir_all(&root).unwrap();

    let stderr = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    assert_eq!(pass.status.code(), Some(0), "{}", stderr(&pass));
    assert!(
        stderr(&pass).starts_with("garden-eval: measured hybrid recall@1 1.000, MRR 1.000, p50 "),
        "{}",
        stderr(&pass)
    );

    assert_eq!(recall.status.code(), Some(1), "{}", stderr(&recall));
    let lines: Vec<String> = stderr(&recall).lines().skip(1).map(str::to_owned).collect();
    assert_eq!(
        lines,
        ["garden-eval: lexical recall@1 0.000 is below --min-recall 0.5"],
        "the hybrid mode meets the threshold"
    );

    assert_eq!(p50.status.code(), Some(1), "{}", stderr(&p50));
    let misses = stderr(&p50)
        .lines()
        .filter(|l| l.contains("is above --max-p50-ms 0"))
        .count();
    assert_eq!(misses, 2, "{}", stderr(&p50));

    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
}
