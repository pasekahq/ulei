//! `garden-eval`: file-level retrieval evaluation of the garden over a Markdown corpus.
//!
//! ```text
//! garden-eval --gold GOLD.jsonl --corpus DIR [--files LIST] [--k 3] [--out REPORT.json]
//!             [--embedder none|fake|onnx] [--model-dir DIR --ort-lib LIB] [--threads 2]
//!             [--passage-chars 1200] [--dense-head N] [--db PATH]
//!             [--min-recall R] [--max-p50-ms MS]
//!             [--pointer-min-cosine C] [--pointer-min-lexical L]
//! ```
//! `--files` names corpus-relative Markdown paths, one per line (default: every `*.md` under
//! the corpus). Every passage is embedded unless `--dense-head N` limits it to the first `N`
//! passages of each document. The store lives in memory unless `--db PATH` names a SQLite file:
//! that file then holds the full text of every passage, all marked public, so give it a private
//! location. The report is JSON on stdout or in `--out`; exit 2 on an input error.
//!
//! The pointers of every scored row and of every row with `expected_source` `none` are asked
//! behind a `PointerGate` with the two thresholds (default: the garden's defaults) and counted
//! in each mode's `pointers`.
//!
//! `--min-recall` and `--max-p50-ms` hold every mode of the report (lexical, and hybrid with an
//! embedder) to recall@k and warm p50 thresholds: one `garden-eval: measured ...` line with the
//! pointer counts goes to stderr, then one line per missed threshold, and a miss exits 1.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use ulei_garden::embed::{Embedder, HashEmbedder};
use ulei_garden::eval::{self, EvalError};
use ulei_garden::search::{PointerGate, SearchParams, index_missing_where};
use ulei_garden::store::Store;

struct Args {
    gold: PathBuf,
    corpus: PathBuf,
    files: Option<PathBuf>,
    k: usize,
    out: Option<PathBuf>,
    embedder: String,
    model_dir: Option<PathBuf>,
    ort_lib: Option<PathBuf>,
    threads: usize,
    passage_chars: usize,
    db: Option<PathBuf>,
    dense_head: Option<usize>,
    min_recall: Option<f64>,
    max_p50_ms: Option<f64>,
    gate: PointerGate,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        gold: PathBuf::new(),
        corpus: PathBuf::new(),
        files: None,
        k: 3,
        out: None,
        embedder: String::new(),
        model_dir: None,
        ort_lib: None,
        threads: 2,
        passage_chars: 1200,
        db: None,
        dense_head: None,
        min_recall: None,
        max_p50_ms: None,
        gate: PointerGate::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        let number = |v: String| v.parse::<usize>().map_err(|e| format!("{flag}: {e}"));
        let threshold = |v: String| match v.parse::<f64>() {
            Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
            Ok(_) => Err(format!("{flag} must be a finite non-negative number")),
            Err(e) => Err(format!("{flag}: {e}")),
        };
        match flag.as_str() {
            "--gold" => a.gold = value()?.into(),
            "--corpus" => a.corpus = value()?.into(),
            "--files" => a.files = Some(value()?.into()),
            "--k" => a.k = number(value()?)?,
            "--out" => a.out = Some(value()?.into()),
            "--embedder" => a.embedder = value()?,
            "--model-dir" => a.model_dir = Some(value()?.into()),
            "--ort-lib" => a.ort_lib = Some(value()?.into()),
            "--threads" => a.threads = number(value()?)?,
            "--passage-chars" => a.passage_chars = number(value()?)?,
            "--db" => a.db = Some(value()?.into()),
            "--dense-head" => a.dense_head = Some(number(value()?)?),
            "--min-recall" => a.min_recall = Some(threshold(value()?)?),
            "--max-p50-ms" => a.max_p50_ms = Some(threshold(value()?)?),
            "--pointer-min-cosine" => a.gate.min_cosine = gate_threshold(&flag, &value()?)?,
            "--pointer-min-lexical" => a.gate.min_lexical = gate_threshold(&flag, &value()?)?,
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if a.gold.as_os_str().is_empty() || a.corpus.as_os_str().is_empty() {
        return Err(format!("--gold and --corpus are required\n{USAGE}"));
    }
    if a.k == 0 {
        return Err("--k must be positive".into());
    }
    if a.embedder.is_empty() {
        a.embedder = if a.model_dir.is_some() {
            "onnx"
        } else {
            "none"
        }
        .into();
    }
    Ok(a)
}

/// A finite gate threshold.
fn gate_threshold(flag: &str, v: &str) -> Result<f32, String> {
    match v.parse::<f32>() {
        Ok(x) if x.is_finite() => Ok(x),
        Ok(_) => Err(format!("{flag} must be finite")),
        Err(e) => Err(format!("{flag}: {e}")),
    }
}

const USAGE: &str = "usage: garden-eval --gold GOLD.jsonl --corpus DIR [--files LIST] [--k 3] \
[--out REPORT.json] [--embedder none|fake|onnx] [--model-dir DIR --ort-lib LIB] [--threads 2] \
[--passage-chars 1200] [--dense-head N] [--db PATH] [--min-recall R] [--max-p50-ms MS] \
[--pointer-min-cosine C] [--pointer-min-lexical L]\n\
--db PATH writes the whole corpus text, marked public, into that SQLite file (default: in memory)";

fn load_embedder(a: &Args) -> Result<Option<Box<dyn Embedder>>, String> {
    match a.embedder.as_str() {
        "none" => Ok(None),
        "fake" => Ok(Some(Box::new(HashEmbedder::new(256)))),
        "onnx" => onnx(a),
        other => Err(format!("unknown --embedder {other:?}")),
    }
}

#[cfg(feature = "onnx")]
fn onnx(a: &Args) -> Result<Option<Box<dyn Embedder>>, String> {
    let (Some(dir), Some(lib)) = (&a.model_dir, &a.ort_lib) else {
        return Err("--embedder onnx needs --model-dir and --ort-lib".into());
    };
    ulei_garden::embed::onnx::OnnxEmbedder::load(dir, lib, a.threads)
        .map(|e| Some(Box::new(e) as Box<dyn Embedder>))
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "onnx"))]
fn onnx(_: &Args) -> Result<Option<Box<dyn Embedder>>, String> {
    Err("this build has no `onnx` feature".into())
}

fn run(a: &Args) -> Result<(serde_json::Value, eval::Outcome), String> {
    let s = |e: EvalError| e.to_string();
    let gold_text =
        std::fs::read_to_string(&a.gold).map_err(|e| format!("{}: {e}", a.gold.display()))?;
    let gold = eval::parse_gold(&gold_text).map_err(s)?;
    let files = match &a.files {
        Some(list) => std::fs::read_to_string(list)
            .map_err(|e| format!("{}: {e}", list.display()))?
            .lines()
            .map(str::trim)
            .filter(|l| eval::is_markdown(l))
            .map(str::to_owned)
            .collect(),
        None => eval::markdown_files(&a.corpus).map_err(s)?,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

    let t = Instant::now();
    let embedder = load_embedder(a)?;
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let mut store = match &a.db {
        Some(p) => Store::open(p),
        None => Store::open_in_memory(),
    }
    .map_err(|e| e.to_string())?;
    let corpus = eval::ingest(&mut store, &a.corpus, &files, a.passage_chars, 0).map_err(s)?;
    let ingest_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let vectors_added = match &embedder {
        Some(e) => {
            let head = a.dense_head.unwrap_or(usize::MAX);
            let keep = |id: &ulei_garden::model::FactId| {
                corpus.ordinals.get(id).is_some_and(|o| *o < head)
            };
            index_missing_where(&mut store, e.as_ref(), 8, &keep).map_err(|e| e.to_string())?
        }
        None => 0,
    };
    let embed_ms = t.elapsed().as_secs_f64() * 1000.0;

    let params = SearchParams {
        k: a.k,
        ..SearchParams::at(now)
    };
    let out = eval::evaluate(
        &store,
        embedder.as_deref(),
        &corpus,
        &gold,
        &params,
        &a.gate,
    )
    .map_err(s)?;
    let report = json!({
        "k": a.k,
        "embedder": embedder.as_ref().map(|e| e.model_id().to_owned()),
        "rows": gold.len(),
        "build": {
            "files": corpus.files,
            "facts": corpus.fact_paths.len(),
            "rejected": corpus.rejected,
            "passage_chars": a.passage_chars,
            "dense_head": a.dense_head,
            "ingest_ms": ingest_ms,
            "model_load_ms": load_ms,
            "embedded": vectors_added,
            "embed_ms": embed_ms,
        },
        "pointer_gate": { "min_cosine": a.gate.min_cosine, "min_lexical": a.gate.min_lexical },
        "lexical": out.lexical.to_json(a.k),
        "hybrid": out.hybrid.as_ref().map(|h| h.to_json(a.k)),
    });
    Ok((report, out))
}

/// The `measured` line and one line per threshold of `a` that a mode of `out` misses.
fn thresholds(a: &Args, out: &eval::Outcome) -> (String, Vec<String>) {
    let k = a.k;
    let modes = out
        .hybrid
        .as_ref()
        .map(|h| ("hybrid", h))
        .into_iter()
        .chain([("lexical", &out.lexical)]);
    let mut measured = Vec::new();
    let mut missed = Vec::new();
    for (mode, m) in modes {
        let p50 = m.latency_percentile(0.5);
        let ptr = &m.pointers;
        measured.push(format!(
            "{mode} recall@{k} {:.3}, MRR {:.3}, p50 {p50:.2} ms, p95 {:.2} ms, \
             pointers right {}/{} wrong {} unrelated {}/{}",
            m.recall,
            m.mrr,
            m.latency_percentile(0.95),
            ptr.right,
            m.n,
            ptr.wrong,
            ptr.unrelated_pointed,
            ptr.unrelated,
        ));
        if let Some(min) = a.min_recall
            && m.recall < min
        {
            missed.push(format!(
                "{mode} recall@{k} {:.3} is below --min-recall {min}",
                m.recall
            ));
        }
        if let Some(max) = a.max_p50_ms
            && p50 > max
        {
            missed.push(format!(
                "{mode} p50 {p50:.2} ms is above --max-p50-ms {max}"
            ));
        }
    }
    (format!("measured {}", measured.join("; ")), missed)
}

fn main() -> ExitCode {
    let report = parse_args().and_then(|a| {
        let (r, out) = run(&a)?;
        let text = serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?;
        match &a.out {
            Some(p) => std::fs::write(p, text + "\n").map_err(|e| format!("{}: {e}", p.display())),
            None => writeln!(std::io::stdout().lock(), "{text}").map_err(|e| e.to_string()),
        }?;
        if a.min_recall.is_none() && a.max_p50_ms.is_none() {
            return Ok(Vec::new());
        }
        let (measured, missed) = thresholds(&a, &out);
        eprintln!("garden-eval: {measured}");
        Ok(missed)
    });
    match report {
        Ok(missed) if missed.is_empty() => ExitCode::SUCCESS,
        Ok(missed) => {
            for m in missed {
                eprintln!("garden-eval: {m}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("garden-eval: {e}");
            ExitCode::from(2)
        }
    }
}
