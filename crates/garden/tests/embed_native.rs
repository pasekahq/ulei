//! The helper's `native` backend (ADR-0087 E6) on the tiny checkpoint of `tiny_bert`, converted
//! at run time: picked by the model file, its identity the content digest sealed in the weight
//! file (ADR-0087 E6c), its vectors
//! the engine's own text-in vectors with the header's prefixes bit for bit, the pipe's refusals,
//! and every way the weight file can be wrong refused at start without a panic (a truncated,
//! longer or malformed file as "model file corrupted", an older format with "convert the model
//! again"); no model, no network.

#![cfg(feature = "native")]
#![allow(missing_docs, clippy::unwrap_used)]

mod tiny_bert;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tiny_bert::{Dir, EMBED, H, blob_at, convert, find, fixture, hex};
use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, StartError};
use ulei_garden::embed::{ENGINE_FILE, Embedder, ONNX_FILE, TOKENIZER_FILE};
use ulei_infer::bert::Encoder;
use ulei_infer::weights::file::{DIGEST, WeightFile};

/// The fixture `tag` with its weight file converted to `model.ulei-infer` in `model/`.
fn model(tag: &str, extra: &[&str]) -> (Dir, std::path::PathBuf) {
    let dir = fixture(tag);
    let out = convert(&dir.0, "w.uiw", extra);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let model = dir.0.join("model");
    fs::create_dir_all(&model).unwrap();
    fs::rename(dir.0.join("w.uiw"), model.join(ENGINE_FILE)).unwrap();
    (dir, model)
}

fn start(model_dir: &Path, embedder: Option<&str>) -> Result<Helper, StartError> {
    let mut command = Command::new(EMBED);
    if let Some(e) = embedder {
        command.args(["--embedder", e]);
    }
    command.arg("--model-dir").arg(model_dir);
    Helper::start(command, Duration::from_secs(30), CALL_DEADLINE)
}

fn refusal(model_dir: &Path, embedder: Option<&str>) -> String {
    classed(model_dir, embedder).0
}

/// The refusal's reason and whether it carries the class `corrupted` (ADR-0087 open question
/// 15), checked against its text.
fn classed(model_dir: &Path, embedder: Option<&str>) -> (String, bool) {
    let (reason, corrupted) = match start(model_dir, embedder) {
        Err(StartError::Refused(reason)) => (reason, false),
        Err(StartError::Corrupt(reason)) => (reason, true),
        Err(e) => panic!("not a refusal: {e}"),
        Ok(_) => panic!("the helper got ready"),
    };
    assert_eq!(
        reason.contains("model file corrupted"),
        corrupted,
        "{reason}"
    );
    (reason, corrupted)
}

/// The exit code of the helper on `model_dir` with its stdin closed.
fn exit_code(model_dir: &Path) -> Option<i32> {
    Command::new(EMBED)
        .arg("--model-dir")
        .arg(model_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .code()
}

#[test]
fn auto_picks_the_engine_by_its_file_and_answers_the_engines_own_vectors_with_the_header_prefixes()
{
    let (_dir, model) = model("native-ok", &[]);
    let bytes = fs::read(model.join(ENGINE_FILE)).unwrap();
    // The identity is the digest sealed in the preamble, not the file's plain SHA-256.
    let sha = hex(&bytes[DIGEST]);
    assert_ne!(sha, hex(&Sha256::digest(&bytes)));
    let helper = start(&model, None).unwrap();
    let id = helper.identity();
    assert_eq!(id.name, "ulei-infer-bert-int8-mean-6");
    assert_eq!(id.model_sha256, sha);
    assert_eq!(
        id.tokenizer_sha256, sha,
        "the weight file holds the tokenizer"
    );
    assert_eq!(id.dim, H);
    assert_eq!(
        helper.model_id(),
        format!("ulei-infer-bert-int8-mean-6/{sha}/{sha}")
    );

    let file = WeightFile::read(bytes.as_slice()).unwrap();
    let engine = Encoder::from_file(&file).unwrap();
    let own = |text: &str| engine.embed_text(text).unwrap();
    // Both kinds, the empty text, a batch, and a text far over the 6 tokens of the model.
    let long = "a ".repeat(500_000);
    for q in ["a", "", "b a", long.as_str()] {
        let asked = Instant::now();
        assert_eq!(helper.embed_query(q).unwrap(), own(&format!("query: {q}")));
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
    }
    let passages = ["a a", "", "b", long.as_str(), "a"];
    let got = helper.embed_passages(&passages).unwrap();
    let want: Vec<Vec<f32>> = passages
        .iter()
        .map(|p| own(&format!("passage: {p}")))
        .collect();
    assert_eq!(got, want);
    assert_ne!(got[0], own("a a"), "the prefix is put before the text");
    assert_eq!(helper.embed_passages(&[]).unwrap(), Vec::<Vec<f32>>::new());
    for v in &got {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "{norm}");
    }
}

#[test]
fn a_weight_file_without_prefixes_embeds_the_text_as_it_is() {
    let (_dir, model) = model(
        "native-bare",
        &["--query-prefix", "", "--passage-prefix", ""],
    );
    let file = WeightFile::open(&model.join(ENGINE_FILE)).unwrap();
    let engine = Encoder::from_file(&file).unwrap();
    let helper = start(&model, Some("native")).unwrap();
    assert_eq!(
        helper.embed_query("a b").unwrap(),
        engine.embed_text("a b").unwrap()
    );
    assert_eq!(
        helper.embed_passages(&["a b"]).unwrap(),
        [engine.embed_text("a b").unwrap()]
    );
}

#[test]
fn the_embedder_keeps_the_engines_buffers_between_calls_and_its_vectors_do_not_change() {
    use ulei_garden::embed::native::{KEEP_BYTES, NativeEmbedder, QUERY_KEEP_BYTES};
    let (_dir, model) = model("native-ws", &[]);
    let native = NativeEmbedder::load(&model.join(ENGINE_FILE)).unwrap();
    let file = WeightFile::open(&model.join(ENGINE_FILE)).unwrap();
    let engine = Encoder::from_file(&file).unwrap();
    assert_eq!(
        native.workspace_bytes(),
        (0, 0),
        "nothing before the first call"
    );
    let passages = ["a a b", "", "b a b a b a", "a"];
    let want: Vec<Vec<f32>> = passages
        .iter()
        .map(|p| engine.embed_text(&format!("passage: {p}")).unwrap())
        .collect();
    assert_eq!(native.embed_passages(&passages).unwrap(), want);
    let (held, none) = native.workspace_bytes();
    assert!(held > 0 && held <= KEEP_BYTES && none == 0, "{held} {none}");
    // The same texts again, a query between: the same vectors, the passages' buffers not grown,
    // the query's own kept.
    assert_eq!(
        native.embed_query("b").unwrap(),
        engine.embed_text("query: b").unwrap()
    );
    assert_eq!(native.embed_passages(&passages).unwrap(), want);
    let (again, query) = native.workspace_bytes();
    assert_eq!(again, held);
    assert!(query > 0 && query <= QUERY_KEEP_BYTES, "{query}");
}

/// Sends `lines` to a helper on `model_dir` and returns its ready line and each answer's header.
fn raw(model_dir: &Path, embedder: &str, lines: &[&str]) -> (String, Vec<String>) {
    let mut child = Command::new(EMBED)
        .args(["--embedder", embedder, "--model-dir"])
        .arg(model_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    out.read_line(&mut ready).unwrap();
    let mut answers = Vec::new();
    for line in lines {
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut header = String::new();
        out.read_line(&mut header).unwrap();
        let v: serde_json::Value = serde_json::from_str(&header).unwrap();
        if let (Some(n), Some(d)) = (v["count"].as_u64(), v["dim"].as_u64()) {
            let mut body = vec![0u8; usize::try_from(n * d * 4).unwrap()];
            out.read_exact(&mut body).unwrap();
        }
        answers.push(header.trim_end().to_owned());
    }
    drop(stdin);
    assert!(child.wait().unwrap().success(), "exit 0 when stdin ends");
    (ready, answers)
}

#[test]
fn the_pipe_refuses_bad_requests_as_the_other_backends_do_and_stays_in_step() {
    let (dir, model) = model("native-pipe", &[]);
    let lines = [
        "not json",
        r#"{"kind": "summary", "texts": ["a"]}"#,
        r#"{"kind": "query", "texts": "a"}"#,
        r#"{"kind": "passage", "texts": ["a", 1]}"#,
        r#"{"kind": "query"}"#,
        r#"{"kind": "query", "texts": ["a", ""]}"#,
        r#"{"kind": "passage", "texts": []}"#,
    ];
    let (ready, native) = raw(&model, "native", &lines);
    assert!(ready.contains("\"dim\":4"), "{ready}");
    // The fake backend over ONNX-shaped artifacts answers the same stream with the same lines.
    let fake_dir = dir.0.join("fake");
    fs::create_dir_all(fake_dir.join(ONNX_FILE).parent().unwrap()).unwrap();
    fs::write(fake_dir.join(ONNX_FILE), "x").unwrap();
    fs::write(fake_dir.join(TOKENIZER_FILE), "").unwrap();
    let (_, fake) = raw(&fake_dir, "fake", &lines);
    let shape = |answers: &[String]| -> Vec<String> {
        answers
            .iter()
            .map(|a| a.replace("\"dim\":256", "\"dim\":4"))
            .collect()
    };
    assert_eq!(shape(&native), shape(&fake));
    assert!(
        native[0].starts_with(r#"{"error":"request: "#),
        "{}",
        native[0]
    );
    assert_eq!(
        native[1],
        r#"{"error":"request: kind must be \"query\" or \"passage\""}"#
    );
    assert_eq!(native[5], r#"{"count":2,"dim":4}"#);
    assert_eq!(native[6], r#"{"count":0,"dim":4}"#);
}

#[test]
fn both_lanes_give_the_engines_own_vectors_also_when_they_run_at_once() {
    let (_dir, model) = model("native-lanes", &[]);
    let file = WeightFile::open(&model.join(ENGINE_FILE)).unwrap();
    let engine = Encoder::from_file(&file).unwrap();
    let helper = std::sync::Arc::new(start(&model, None).unwrap());
    assert!(helper.has_lanes());
    let words = ["a", "b", "a b", "b a b", "a a a b", ""];
    let text = move |n: usize| -> String {
        (0..=n % 5)
            .map(|i| words[(n + i) % words.len()])
            .collect::<Vec<_>>()
            .join(" ")
    };
    let rounds = 200;
    let queries = {
        let helper = std::sync::Arc::clone(&helper);
        std::thread::spawn(move || {
            (0..rounds)
                .map(|n| helper.embed_query(&text(n)).unwrap())
                .collect::<Vec<_>>()
        })
    };
    let passages: Vec<Vec<f32>> = (0..rounds)
        .map(|n| {
            helper
                .embed_passages(&[&text(n + 1)])
                .unwrap()
                .pop()
                .unwrap()
        })
        .collect();
    let queries = queries.join().unwrap();
    for n in 0..rounds {
        assert_eq!(
            queries[n],
            engine.embed_text(&format!("query: {}", text(n))).unwrap(),
            "query {n}"
        );
        assert_eq!(
            passages[n],
            engine
                .embed_text(&format!("passage: {}", text(n + 1)))
                .unwrap(),
            "passage {n}"
        );
    }
}

/// The nice value of each thread of the process `pid`, by thread name.
fn nice_by_thread(pid: u32) -> Vec<(String, i64)> {
    let mut threads = Vec::new();
    for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let task = task.unwrap().path();
        // A thread of this test process (another test's) may end between the listing and the
        // read: it is gone, not a failure.
        let (Ok(name), Ok(stat)) = (
            fs::read_to_string(task.join("comm")),
            fs::read_to_string(task.join("stat")),
        ) else {
            continue;
        };
        // The fields after the command's closing parenthesis; nice is the 19th field.
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        let nice = rest.split(' ').nth(16).unwrap().parse().unwrap();
        threads.push((name.trim().to_owned(), nice));
    }
    threads.sort();
    threads
}

#[test]
fn the_indexing_lane_runs_at_a_lower_priority_and_the_query_lane_keeps_the_helpers() {
    let (_dir, model) = model("native-nice", &[]);
    let mut child = Command::new(EMBED)
        .args(["--model-dir"])
        .arg(&model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    out.read_line(&mut ready).unwrap();
    assert!(ready.contains("\"lanes\":2"), "{ready}");
    // A passage and a query with ids, then one without: each answer names its request's id,
    // the one without none.
    writeln!(stdin, r#"{{"id":7,"kind":"passage","texts":["a b"]}}"#).unwrap();
    stdin.flush().unwrap();
    let mut headers = Vec::new();
    let read = |out: &mut BufReader<_>| {
        let mut header = String::new();
        out.read_line(&mut header).unwrap();
        let mut body = vec![0u8; H * 4];
        out.read_exact(&mut body).unwrap();
        header.trim_end().to_owned()
    };
    headers.push(read(&mut out));
    writeln!(stdin, r#"{{"id":8,"kind":"query","texts":["b"]}}"#).unwrap();
    writeln!(stdin, r#"{{"kind":"query","texts":["a"]}}"#).unwrap();
    stdin.flush().unwrap();
    headers.push(read(&mut out));
    headers.push(read(&mut out));
    assert_eq!(
        headers,
        [
            r#"{"count":1,"dim":4,"id":7}"#,
            r#"{"count":1,"dim":4,"id":8}"#,
            r#"{"count":1,"dim":4}"#
        ]
    );
    // The helper's main thread keeps the nice value it was started with, this process's.
    let own = nice_by_thread(std::process::id())
        .into_iter()
        .map(|(_, nice)| nice)
        .min()
        .unwrap();
    assert_eq!(
        nice_by_thread(child.id()),
        [
            ("garden-embed-in".to_owned(), (own + 10).min(19)),
            ("ulei-garden-emb".to_owned(), own)
        ],
        "the thread names are cut to 15 bytes"
    );
    drop(stdin);
    assert!(child.wait().unwrap().success(), "exit 0 when stdin ends");
}

#[test]
fn a_missing_weight_file_is_refused_at_start_with_its_reason_and_no_panic() {
    let (dir, model) = model("native-bad", &[]);
    let engine = model.join(ENGINE_FILE);
    let check = |reason: &str, want: &str| {
        assert!(reason.contains(want), "{reason:?} lacks {want:?}");
        assert!(!reason.contains("panicked"), "{reason}");
    };

    // Missing: auto finds no model file; native names the file it could not open.
    fs::remove_file(&engine).unwrap();
    check(
        &refusal(&model, None),
        "holds neither model.ulei-infer nor onnx/model_quantized.onnx",
    );
    check(
        &refusal(&model, Some("native")),
        "model.ulei-infer: No such file",
    );

    drop(dir);
}

#[test]
fn a_corrupted_weight_file_is_refused_at_start_as_such_from_its_preamble_and_header_alone() {
    let (dir, model) = model("native-corrupt", &[]);
    let good = fs::read(model.join(ENGINE_FILE)).unwrap();
    let engine = model.join(ENGINE_FILE);
    let check = |reason: &str, want: &str| {
        assert!(reason.contains(want), "{reason:?} lacks {want:?}");
        assert!(!reason.contains("panicked"), "{reason}");
    };
    // Corrupt: truncated, empty, garbage, garbage appended, a digest never sealed, a tensor
    // table that does not hold together. Each is "model file corrupted" with its reason, from
    // the preamble, the file's length and the header alone.
    let corrupted = format!("model file corrupted: {}: ", engine.display());
    let mut longer = good.clone();
    longer.extend_from_slice(b"garbage");
    let mut unsealed = good.clone();
    unsealed[DIGEST].fill(0);
    let mut table = good.clone();
    let at = find(&table, b"\"offset\":0").unwrap();
    table[at + "\"offset\":".len()] = b'8';
    for (bytes, want) in [
        (
            good[..good.len() / 2].to_vec(),
            format!(
                "the file is {} bytes, its preamble says {}",
                good.len() / 2,
                good.len()
            ),
        ),
        (
            good[..good.len() - 1].to_vec(),
            format!(
                "the file is {} bytes, its preamble says {}",
                good.len() - 1,
                good.len()
            ),
        ),
        (Vec::new(), "truncated file of 0 bytes".into()),
        (vec![7u8; 4096], "not a weight file (bad magic)".into()),
        (
            longer,
            format!(
                "the file is {} bytes, its preamble says {}",
                good.len() + 7,
                good.len()
            ),
        ),
        (unsealed, "no content digest in the preamble".into()),
        (
            table,
            "tensor `embeddings.word_embeddings.weight`: offset 8 is misaligned or overlaps".into(),
        ),
    ] {
        fs::write(&engine, bytes).unwrap();
        let (reason, class) = classed(&model, None);
        assert!(class, "{reason}");
        check(&reason, &format!("{corrupted}{want}"));
        assert!(!reason.contains("convert the model again"), "{reason}");
        assert_eq!(exit_code(&model), Some(3), "{reason}");
    }
    // A changed blob byte is not seen at start (no blob is read): the helper starts under the
    // sealed identity; `ulei-garden-embed verify` finds it (`embed_verify.rs`).
    let mut blob = good.clone();
    blob[blob_at(&good, "encoder.layer.0.attention.self.query.weight") + 1] ^= 1;
    fs::write(&engine, &blob).unwrap();
    assert_eq!(
        start(&model, None).unwrap().identity().model_sha256,
        hex(&good[DIGEST])
    );
    drop(dir);
}

#[test]
fn a_weight_file_of_an_older_format_or_another_kind_is_refused_with_its_reason() {
    let (dir, model) = model("native-old", &[]);
    let good = fs::read(model.join(ENGINE_FILE)).unwrap();
    let engine = model.join(ENGINE_FILE);
    let check = |reason: &str, want: &str| {
        assert!(reason.contains(want), "{reason:?} lacks {want:?}");
        assert!(!reason.contains("panicked"), "{reason}");
    };
    let mut flipped = good.clone();
    let at = find(&flipped, b"\"pooling\"").unwrap();
    flipped[at + 1] = b'X';
    fs::write(&engine, &flipped).unwrap();
    check(&refusal(&model, None), "weight file names no pooling");

    // An older format: convert the model again, not "corrupted". The BERT format `/3` in a file
    // of this version, and a file of the version before the digest.
    let mut old = good.clone();
    let at = find(&old, b"ulei-infer/bert-int8/4").unwrap();
    old[at + "ulei-infer/bert-int8/".len()] = b'3';
    fs::write(&engine, &old).unwrap();
    check(&refusal(&model, None), "convert the model again");
    let mut old = good.clone();
    old[8] = 2;
    fs::write(&engine, &old).unwrap();
    let (reason, class) = classed(&model, None);
    assert!(!class, "{reason}");
    check(&reason, "format version 2, expected 3");
    check(&reason, "convert the model again");
    assert!(!reason.contains("corrupted"), "{reason}");
    assert_eq!(exit_code(&model), Some(1));

    // The wrong kind of model file: an ONNX file under the engine's name, and a weight file of
    // another model family.
    fs::write(&engine, b"\x08\x07\x12\x0bonnx-model-bytes").unwrap();
    check(&refusal(&model, None), "model file corrupted: ");
    let mut other = good.clone();
    let at = find(&other, b"bert-int8").unwrap();
    other[at..at + 9].copy_from_slice(b"conformer");
    fs::write(&engine, &other).unwrap();
    check(
        &refusal(&model, None),
        "\"ulei-infer/conformer/4\" is not a BERT int8 weight file",
    );

    // Both model files: auto refuses to guess; neither file alone was asked for.
    fs::write(&engine, &good).unwrap();
    fs::create_dir_all(model.join(ONNX_FILE).parent().unwrap()).unwrap();
    fs::write(model.join(ONNX_FILE), "x").unwrap();
    check(
        &refusal(&model, None),
        "holds both model.ulei-infer and onnx/model_quantized.onnx",
    );
    assert!(
        start(&model, Some("native")).is_ok(),
        "an explicit backend picks its file"
    );
    drop(dir);
}

#[test]
fn a_weight_file_renamed_over_the_running_ones_takes_effect_only_at_the_next_start() {
    let (dir, model) = model("native-swap", &[]);
    let helper = start(&model, None).unwrap();
    let before = helper.embed_query("a b").unwrap();
    let first = helper.identity().clone();

    // A new file, written beside and renamed into place: the running helper keeps the mapping
    // of the old one and answers as before under its old identity.
    let out = convert(&dir.0, "next.uiw", &["--act-block", "2"]);
    assert!(out.status.success());
    fs::rename(dir.0.join("next.uiw"), model.join(ENGINE_FILE)).unwrap();
    assert_eq!(helper.embed_query("a b").unwrap(), before);
    assert_eq!(helper.fault(), None);

    // Removing the file does not disturb it either.
    let kept = dir.0.join("kept.uiw");
    fs::rename(model.join(ENGINE_FILE), &kept).unwrap();
    assert_eq!(helper.embed_query("a b").unwrap(), before);
    fs::rename(&kept, model.join(ENGINE_FILE)).unwrap();

    // The next start reads the new file and gets a new identity.
    let next = start(&model, None).unwrap();
    assert_ne!(next.identity().model_sha256, first.model_sha256);
    assert_ne!(next.model_id(), helper.model_id());
}
