//! `ulei-garden-embed convert` (ADR-0086 section 2) on a tiny synthetic checkpoint written at
//! run time: the weight file opens in the encoder engine with its tokenizer, carries the
//! digests of its inputs, is the same on a second run, and bad input is refused; no model, no
//! network.

#![allow(missing_docs, clippy::unwrap_used)]

mod tiny_bert;

use std::fs;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tiny_bert::{H, VOCAB, VOCAB_U32, convert, convert_raw, fixture, hex};
use ulei_infer::bert::Encoder;
use ulei_infer::weights::file::WeightFile;
#[test]
fn convert_writes_a_deterministic_weight_file_the_engine_opens() {
    let dir = fixture("ok");
    let first = convert(&dir.0, "a.uiw", &["--act-block", "2"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let line: Value = serde_json::from_slice(&first.stdout).unwrap();
    let bytes = fs::read(dir.0.join("a.uiw")).unwrap();
    assert_eq!(line["bytes"], bytes.len());
    assert_eq!(line["sha256"], hex(&Sha256::digest(&bytes)));
    assert!(!dir.0.join("a.uiw.part").exists());

    let second = convert(&dir.0, "b.uiw", &["--act-block", "2"]);
    assert!(second.status.success());
    assert_eq!(fs::read(dir.0.join("b.uiw")).unwrap(), bytes);

    let file = WeightFile::read(bytes.as_slice()).unwrap();
    let meta = file.meta();
    assert_eq!(meta["act_block"], 2);
    assert_eq!(meta["pooling"], "mean");
    assert_eq!(meta["format"], "ulei-infer/bert-int8/4");
    assert_eq!(meta["prefixes"]["query"], "query: ");
    assert_eq!(meta["prefixes"]["passage"], "passage: ");
    let model = fs::read(dir.0.join("model.safetensors")).unwrap();
    let order = fs::read(dir.0.join("order.u32")).unwrap();
    assert_eq!(
        meta["source"]["checkpoint_sha256"],
        hex(&Sha256::digest(&model))
    );
    assert_eq!(
        meta["source"]["token_order_sha256"],
        hex(&Sha256::digest(&order))
    );
    let tokenizer = fs::read(dir.0.join("tokenizer.json")).unwrap();
    assert_eq!(
        meta["source"]["tokenizer_sha256"],
        hex(&Sha256::digest(&tokenizer))
    );
    assert_eq!(meta["tokenizer"]["pipeline"], "whitespace-split");
    assert_eq!(meta["tokenizer"]["max_tokens"], 6);
    let map = file.u32("embeddings.word_embeddings.row_of_id").unwrap();
    assert_eq!(map.data, &[5, 4, 3, 2, 1, 0]);
    let encoder = Encoder::from_file(&file).unwrap();
    assert_eq!(encoder.prefixes().query, "query: ");
    let v = encoder.embed(&[0, 3, 5]).unwrap();
    assert_eq!(v.len(), H);
    assert!(v.iter().all(|x| x.is_finite()));
    let ids = encoder.tokenizer().unwrap().encode("a b a a a a").unwrap();
    assert_eq!(ids, vec![0, 5, 4, 3, 5, 2]);
    assert_eq!(
        encoder.embed_text("a b a a a a").unwrap(),
        encoder.embed(&ids).unwrap()
    );

    // The default block is 32.
    let default = convert(&dir.0, "c.uiw", &[]);
    assert!(default.status.success());
    let file = WeightFile::open(&dir.0.join("c.uiw")).unwrap();
    assert_eq!(file.meta()["act_block"], 32);

    // Empty prefixes (a model without them) are written as such.
    let bare = convert(
        &dir.0,
        "d.uiw",
        &["--query-prefix", "", "--passage-prefix", ""],
    );
    assert!(bare.status.success());
    let file = WeightFile::open(&dir.0.join("d.uiw")).unwrap();
    assert_eq!(
        file.meta()["prefixes"],
        serde_json::json!({"query": "", "passage": ""})
    );
}

#[test]
fn convert_refuses_bad_input_without_leaving_a_file() {
    let dir = fixture("bad");
    // A row order that is not a permutation of the vocabulary.
    fs::write(dir.0.join("order.u32"), [0u8; 4 * VOCAB]).unwrap();
    let out = convert(&dir.0, "x.uiw", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("ulei-garden-embed convert: "));
    assert!(!dir.0.join("x.uiw").exists());
    assert!(!dir.0.join("x.uiw.part").exists());
    // With a good row order again, each bad flag alone is refused.
    let order: Vec<u8> = (0..VOCAB_U32).flat_map(u32::to_le_bytes).collect();
    fs::write(dir.0.join("order.u32"), order).unwrap();
    assert!(convert(&dir.0, "z.uiw", &[]).status.success());
    for bad in [
        &["--act-block", "0"][..],
        &["--pooling", "max"],
        &["--nope", "1"],
        &["--max-tokens", "9"],
        &["--max-tokens", "1"],
        &["--tokenizer-pipeline", "sentencepiece"],
        &["--tokenizer", "/nonexistent/tokenizer.json"],
    ] {
        let out = convert(&dir.0, "y.uiw", bad);
        assert_eq!(out.status.code(), Some(1), "{bad:?}");
    }
    // Each prefix is required, so none is left out by mistake.
    for (args, missing) in [
        (&["--query-prefix", "query: "][..], "--passage-prefix"),
        (&["--passage-prefix", "passage: "], "--query-prefix"),
    ] {
        let out = convert_raw(&dir.0, "y.uiw", args);
        assert_eq!(out.status.code(), Some(1));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(&format!("{missing} is required")), "{err}");
    }
    assert!(!dir.0.join("y.uiw").exists());
}

/// `--tokenizer FILE` and `--tokenizer-pipeline` reach the converter: the tokenizer is read from
/// the given file and its digest recorded, an explicit pipeline is taken, and `collapse-spaces`
/// is the pipeline that needs the precompiled character map this fixture lacks. Two surviving
/// mutants of E12a (either match arm deleted) showed only the refusals were tested.
#[test]
fn convert_takes_the_tokenizer_file_and_pipeline_it_is_given() {
    let dir = fixture("tokenizer-flags");
    // The same tokenizer with other bytes (a trailing newline), so its digest tells the files apart.
    let elsewhere = dir.0.join("other-tokenizer.json");
    let mut other = fs::read(dir.0.join("tokenizer.json")).unwrap();
    other.push(b'\n');
    fs::write(&elsewhere, &other).unwrap();
    let out = convert(
        &dir.0,
        "t.uiw",
        &[
            "--tokenizer",
            elsewhere.to_str().unwrap(),
            "--tokenizer-pipeline",
            "whitespace-split",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = WeightFile::open(&dir.0.join("t.uiw")).unwrap();
    let meta = file.meta();
    assert_eq!(meta["tokenizer"]["pipeline"], "whitespace-split");
    assert_eq!(
        meta["source"]["tokenizer_sha256"],
        hex(&Sha256::digest(fs::read(&elsewhere).unwrap()))
    );
    let collapse = convert(
        &dir.0,
        "c.uiw",
        &[
            "--tokenizer",
            elsewhere.to_str().unwrap(),
            "--tokenizer-pipeline",
            "collapse-spaces",
        ],
    );
    assert_eq!(collapse.status.code(), Some(1));
    let err = String::from_utf8_lossy(&collapse.stderr);
    assert!(err.contains("precompiled character map"), "{err}");
}
