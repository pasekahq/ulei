//! A tiny synthetic BERT checkpoint written at run time and its conversion by
//! `ulei-garden-embed convert`, shared by the converter's and the native helper's tests.

// Each test crate compiles this module and uses only a subset of it.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::helper_bin::embed;

pub(crate) const VOCAB: usize = 6;
pub(crate) const VOCAB_U32: u32 = 6;
pub(crate) const H: usize = 4;
pub(crate) const FFN: usize = 8;

pub(crate) struct Dir(pub(crate) PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// A tiny BERT checkpoint (`config.json`, `model.safetensors`, a Unigram `tokenizer.json`
/// without a character map) and a reversed row order.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn fixture(tag: &str) -> Dir {
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

/// `convert` of the fixture in `dir` to `dir/out` with mean pooling, 6 tokens and e5's
/// prefixes, then `extra` (a later flag overrides an earlier one).
pub(crate) fn convert(dir: &Path, out: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["--query-prefix", "query: ", "--passage-prefix", "passage: "];
    args.extend_from_slice(extra);
    convert_raw(dir, out, &args)
}

/// `convert` of the fixture in `dir` to `dir/out` with mean pooling and 6 tokens, then `extra`.
pub(crate) fn convert_raw(dir: &Path, out: &str, extra: &[&str]) -> std::process::Output {
    Command::new(embed())
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

/// `ulei-garden-embed verify` of `args`.
pub(crate) fn verify(args: &[&Path]) -> std::process::Output {
    Command::new(embed())
        .arg("verify")
        .args(args)
        .output()
        .unwrap()
}

/// Where the blob of tensor `name` starts in the weight file `bytes` (version 3: the JSON
/// header's length at 16..24, the header from byte 64, the data section at the next multiple of
/// 64 after it).
pub(crate) fn blob_at(bytes: &[u8], name: &str) -> usize {
    let n = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap())).unwrap();
    let header: Value = serde_json::from_slice(&bytes[64..64 + n]).unwrap();
    let data = (64 + n).div_ceil(64) * 64;
    let t = header["tensors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap();
    data + usize::try_from(t["offset"].as_u64().unwrap()).unwrap()
}

/// The first position of `needle` in `hay`.
pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
