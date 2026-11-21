//! The ONNX embedder and its helper on ONNX Runtime with the tiny model of `ci/garden_onnx.py`,
//! whose CLS vector is the token histogram plus the CLS token. Ignored by default; the CI job
//! `garden-onnx` runs them with `ULEI_GARDEN_TINY_MODEL_DIR` (the model directory) and
//! `ULEI_GARDEN_ORT_LIB` (`libonnxruntime.so`): `cargo test -p ulei-garden --features onnx
//! --test onnx_tiny -- --ignored`.

#![cfg(feature = "onnx")]
#![allow(missing_docs, clippy::unwrap_used)]

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use ulei_garden::embed::onnx::{MAX_TOKENS, OnnxEmbedder};
use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, Identity};
use ulei_garden::embed::tokenizer::Tokenizer;
use ulei_garden::embed::{Embedder, ONNX_FILE, TOKENIZER_FILE, dot, l2_normalize};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");

struct Tiny {
    dir: PathBuf,
    lib: PathBuf,
    embedder: OnnxEmbedder,
    tokenizer: Tokenizer,
    vocab: usize,
}

fn env_path(key: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key} is not set")))
}

/// The tiny model, loaded once per process as ONNX Runtime is.
fn tiny() -> &'static Tiny {
    static TINY: OnceLock<Tiny> = OnceLock::new();
    TINY.get_or_init(|| {
        let dir = env_path("ULEI_GARDEN_TINY_MODEL_DIR");
        let lib = env_path("ULEI_GARDEN_ORT_LIB");
        let embedder = OnnxEmbedder::load(&dir, &lib, 1).unwrap();
        let json = fs::read_to_string(dir.join(TOKENIZER_FILE)).unwrap();
        let tokenizer = Tokenizer::from_json(&json).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        let vocab = doc["model"]["vocab"].as_array().unwrap().len();
        Tiny {
            dir,
            lib,
            embedder,
            tokenizer,
            vocab,
        }
    })
}

/// The unit-length CLS vector the tiny graph yields for `ids`: their histogram plus one more
/// count of the first id.
fn cls_of(ids: &[u32], dim: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    for id in ids.iter().chain(ids.first()) {
        v[usize::try_from(*id).unwrap()] += 1.0;
    }
    l2_normalize(&mut v);
    v
}

fn assert_close(actual: &[f32], expected: &[f32], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}");
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, e)| (a - e).abs())
        .fold(0.0_f32, f32::max);
    assert!(worst < 1e-5, "{what}: off by {worst}");
}

fn sha256_hex(path: &Path) -> String {
    Sha256::digest(fs::read(path).unwrap())
        .iter()
        .fold(String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").unwrap();
            hex
        })
}

#[test]
#[ignore = "needs ULEI_GARDEN_TINY_MODEL_DIR and ULEI_GARDEN_ORT_LIB"]
fn a_query_vector_is_the_unit_length_cls_row_of_the_last_hidden_state() {
    let t = tiny();
    assert_eq!(t.embedder.dim(), t.vocab);
    for text in ["Ilsa Varn keeps a brass lantern in the attic.", "aaa b", ""] {
        let v = t.embedder.embed_query(text).unwrap();
        assert!((dot(&v, &v).sqrt() - 1.0).abs() < 1e-5, "{text:?}");
        let ids = t.tokenizer.encode(text, MAX_TOKENS);
        let cls = cls_of(&ids, t.vocab);
        assert_close(&v, &cls, text);
        // Mean pooling over the rows would give the plain histogram, which differs.
        let mut mean = vec![0.0_f32; t.vocab];
        for id in &ids {
            mean[usize::try_from(*id).unwrap()] += 1.0;
        }
        l2_normalize(&mut mean);
        assert!(
            dot(&cls, &mean) < 0.999,
            "{text:?} cannot tell CLS from mean"
        );
    }
}

#[test]
#[ignore = "needs ULEI_GARDEN_TINY_MODEL_DIR and ULEI_GARDEN_ORT_LIB"]
fn a_long_input_is_cut_to_the_token_limit_with_its_frame() {
    let t = tiny();
    let text = format!("{}zq", "ab ".repeat(400));
    let ids = t.tokenizer.encode(&text, MAX_TOKENS);
    assert_eq!(ids.len(), MAX_TOKENS);
    let v = t.embedder.embed_query(&text).unwrap();
    assert_close(&v, &cls_of(&ids, t.vocab), "long input");
    let tail = t.tokenizer.encode("zq", MAX_TOKENS);
    let (eos, z) = (tail[tail.len() - 1], tail[tail.len() - 3]);
    assert!(
        v[usize::try_from(eos).unwrap()] > 0.0,
        "the end token is kept"
    );
    assert!(
        v[usize::try_from(z).unwrap()].abs() < f32::EPSILON,
        "the text past the limit is cut"
    );
}

#[test]
#[ignore = "needs ULEI_GARDEN_TINY_MODEL_DIR and ULEI_GARDEN_ORT_LIB"]
fn a_passage_batched_with_longer_ones_gets_its_own_vector_in_input_order() {
    let t = tiny();
    let passages: Vec<String> = (0..11)
        .map(|i| format!("Odo Quill buys {} figs.", "ripe ".repeat(i * 7 % 11)))
        .collect();
    let texts: Vec<&str> = passages.iter().map(String::as_str).collect();
    let batched = t.embedder.embed_passages(&texts).unwrap();
    assert_eq!(batched.len(), texts.len());
    for (text, v) in texts.iter().zip(&batched) {
        assert_close(v, &t.embedder.embed_query(text).unwrap(), text);
    }
}

#[test]
#[ignore = "needs ULEI_GARDEN_TINY_MODEL_DIR and ULEI_GARDEN_ORT_LIB"]
fn the_helper_names_the_model_and_both_digests_and_answers_with_the_same_vectors() {
    let t = tiny();
    let mut command = Command::new(EMBED);
    command
        .args(["--embedder", "onnx", "--threads", "1", "--model-dir"])
        .arg(&t.dir)
        .arg("--ort-lib")
        .arg(&t.lib);
    let helper = Helper::start(command, Duration::from_secs(60), CALL_DEADLINE).unwrap();
    assert_eq!(
        helper.identity(),
        &Identity {
            name: t.embedder.model_id().to_owned(),
            model_sha256: sha256_hex(&t.dir.join(ONNX_FILE)),
            tokenizer_sha256: sha256_hex(&t.dir.join(TOKENIZER_FILE)),
            dim: t.vocab,
        }
    );
    let query = "Wren Holt walks by the quiet pond.";
    assert_close(
        &helper.embed_query(query).unwrap(),
        &t.embedder.embed_query(query).unwrap(),
        query,
    );
}
