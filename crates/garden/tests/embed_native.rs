//! The helper's `native` backend (ADR-0087 E6) on the tiny checkpoint of `tiny_bert`, converted
//! at run time: picked by the model file, its identity the weight file's SHA-256, its vectors
//! the engine's own text-in vectors with the header's prefixes bit for bit, the pipe's refusals,
//! and every way the weight file can be wrong refused at start without a panic; no model, no
//! network.

#![cfg(feature = "native")]
#![allow(missing_docs, clippy::unwrap_used)]

mod tiny_bert;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tiny_bert::{Dir, EMBED, H, convert, fixture, hex};
use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, StartError};
use ulei_garden::embed::{ENGINE_FILE, Embedder, ONNX_FILE, TOKENIZER_FILE};
use ulei_infer::bert::Encoder;
use ulei_infer::weights::file::WeightFile;

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
    match start(model_dir, embedder) {
        Err(StartError::Refused(reason)) => reason,
        Err(e) => panic!("not a refusal: {e}"),
        Ok(_) => panic!("the helper got ready"),
    }
}

#[test]
fn auto_picks_the_engine_by_its_file_and_answers_the_engines_own_vectors_with_the_header_prefixes()
{
    let (_dir, model) = model("native-ok", &[]);
    let bytes = fs::read(model.join(ENGINE_FILE)).unwrap();
    let sha = hex(&Sha256::digest(&bytes));
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
fn every_wrong_weight_file_is_refused_at_start_with_its_reason_and_no_panic() {
    let (dir, model) = model("native-bad", &[]);
    let good = fs::read(model.join(ENGINE_FILE)).unwrap();
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

    // Corrupt: truncated, empty, garbage, a flipped byte in the header.
    for (bytes, want) in [
        (good[..good.len() / 2].to_vec(), "weights: "),
        (Vec::new(), "weights: truncated file of 0 bytes"),
        (vec![7u8; 4096], "weights: "),
    ] {
        fs::write(&engine, bytes).unwrap();
        check(&refusal(&model, None), want);
    }
    let mut flipped = good.clone();
    let at = find(&flipped, b"\"pooling\"").unwrap();
    flipped[at + 1] = b'X';
    fs::write(&engine, &flipped).unwrap();
    check(&refusal(&model, None), "weight file names no pooling");

    // An older format: convert the model again.
    let mut old = good.clone();
    let at = find(&old, b"ulei-infer/bert-int8/3").unwrap();
    old[at + "ulei-infer/bert-int8/".len()] = b'2';
    fs::write(&engine, &old).unwrap();
    check(&refusal(&model, None), "convert the model again");

    // The wrong kind of model file: an ONNX file under the engine's name, and a weight file of
    // another model family.
    fs::write(&engine, b"\x08\x07\x12\x0bonnx-model-bytes").unwrap();
    check(&refusal(&model, None), "weights: ");
    let mut other = good.clone();
    let at = find(&other, b"bert-int8").unwrap();
    other[at..at + 9].copy_from_slice(b"conformer");
    fs::write(&engine, &other).unwrap();
    check(
        &refusal(&model, None),
        "\"ulei-infer/conformer/3\" is not a BERT int8 weight file",
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

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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
