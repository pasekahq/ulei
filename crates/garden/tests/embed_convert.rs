//! `ulei-garden-embed convert` (ADR-0086 section 2) on a tiny synthetic checkpoint written at
//! run time: the weight file opens in the encoder engine with its tokenizer, carries the
//! digests of its inputs, is the same on a second run, and bad input is refused; no model, no
//! network.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulei_infer::bert::Encoder;
use ulei_infer::weights::file::WeightFile;

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");
const VOCAB: usize = 6;
const VOCAB_U32: u32 = 6;
const H: usize = 4;
const FFN: usize = 8;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// A tiny BERT checkpoint (`config.json`, `model.safetensors`, a Unigram `tokenizer.json`
/// without a character map) and a reversed row order.
#[allow(clippy::cast_precision_loss)]
fn fixture(tag: &str) -> Dir {
    let dir = Dir(std::env::temp_dir().join(format!(
        "ulei-garden-embed-convert-{}-{tag}",
        std::process::id()
    )));
    let _ = fs::remove_dir_all(&dir.0);
    fs::create_dir_all(&dir.0).unwrap();
    let config = json!({"model_type": "bert", "hidden_act": "gelu", "hidden_size": H,
        "intermediate_size": FFN, "layer_norm_eps": 1e-12, "max_position_embeddings": 8,
        "num_attention_heads": 2, "num_hidden_layers": 1, "pad_token_id": 0,
        "type_vocab_size": 2, "vocab_size": VOCAB});
    fs::write(dir.0.join("config.json"), config.to_string()).unwrap();
    let mut shapes: Vec<(String, Vec<usize>)> = vec![
        ("embeddings.word_embeddings.weight".into(), vec![VOCAB, H]),
        ("embeddings.position_embeddings.weight".into(), vec![8, H]),
        ("embeddings.token_type_embeddings.weight".into(), vec![2, H]),
        ("embeddings.LayerNorm.weight".into(), vec![H]),
        ("embeddings.LayerNorm.bias".into(), vec![H]),
    ];
    for (name, o, k) in [
        ("attention.self.query", H, H),
        ("attention.self.key", H, H),
        ("attention.self.value", H, H),
        ("attention.output.dense", H, H),
        ("intermediate.dense", FFN, H),
        ("output.dense", H, FFN),
    ] {
        shapes.push((format!("encoder.layer.0.{name}.weight"), vec![o, k]));
        shapes.push((format!("encoder.layer.0.{name}.bias"), vec![o]));
    }
    for name in ["attention.output.LayerNorm", "output.LayerNorm"] {
        shapes.push((format!("encoder.layer.0.{name}.weight"), vec![H]));
        shapes.push((format!("encoder.layer.0.{name}.bias"), vec![H]));
    }
    let mut header = serde_json::Map::new();
    let mut data = Vec::new();
    for (i, (name, shape)) in shapes.iter().enumerate() {
        let begin = data.len();
        for j in 0..shape.iter().product::<usize>() {
            let v = ((i * 31 + j * 17) % 23) as f32 / 23.0 - 0.5;
            data.extend_from_slice(&v.to_le_bytes());
        }
        header.insert(
            format!("bert.{name}"),
            json!({"dtype": "F32", "shape": shape, "data_offsets": [begin, data.len()]}),
        );
    }
    let header = serde_json::to_vec(&Value::Object(header)).unwrap();
    let mut file = (header.len() as u64).to_le_bytes().to_vec();
    file.extend_from_slice(&header);
    file.extend_from_slice(&data);
    fs::write(dir.0.join("model.safetensors"), file).unwrap();
    let tokenizer = json!({
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": {"type": "Metaspace", "replacement": "\u{2581}", "add_prefix_space": true},
        "post_processor": {"type": "RobertaProcessing", "sep": ["</s>", 2], "cls": ["<s>", 0]},
        "model": {"type": "Unigram", "unk_id": 3, "vocab": [
            ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
            ["\u{2581}", -1.0], ["\u{2581}a", -1.0]]}
    });
    fs::write(dir.0.join("tokenizer.json"), tokenizer.to_string()).unwrap();
    let order: Vec<u8> = (0..VOCAB_U32).rev().flat_map(u32::to_le_bytes).collect();
    fs::write(dir.0.join("order.u32"), order).unwrap();
    dir
}

fn convert(dir: &Path, out: &str, extra: &[&str]) -> std::process::Output {
    Command::new(EMBED)
        .arg("convert")
        .arg("--checkpoint")
        .arg(dir)
        .arg("--token-order")
        .arg(dir.join("order.u32"))
        .args(["--pooling", "mean", "--max-tokens", "6", "--out"])
        .arg(dir.join(out))
        .args(extra)
        .output()
        .unwrap()
}

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
}
