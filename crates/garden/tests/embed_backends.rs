//! The same request stream to the helper's `onnx` and `native` backends on one real e5 model
//! (ADR-0086 E6). Ignored: it needs the model outside the repository, named by
//! `ULEI_GARDEN_ENGINE_DIR` (a directory holding `model.ulei-infer`), `ULEI_GARDEN_ONNX_DIR` (one
//! holding `onnx/model_quantized.onnx` and `tokenizer.json` of the same model) and
//! `ULEI_GARDEN_ORT_LIB` (`libonnxruntime.so`). The answers must agree line for line on the
//! protocol (refusals, counts, dimension) and in direction on every vector; the two are different
//! quantisations, so the vectors are not equal.

#![cfg(all(feature = "native", feature = "onnx"))]
#![allow(missing_docs, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");

/// One answer: its header line (or `error`) and its vectors.
type Answer = (String, Vec<Vec<f32>>);

/// The answers of one helper: its ready line's dimension, then per request the header line
/// and the vectors.
fn answers(args: &[&str], lines: &[String]) -> (u64, Vec<Answer>) {
    let mut child = Command::new(EMBED)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    out.read_line(&mut ready).unwrap();
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let dim = ready["dim"].as_u64().unwrap_or_else(|| panic!("{ready}"));
    let mut got = Vec::new();
    for line in lines {
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut header = String::new();
        out.read_line(&mut header).unwrap();
        let v: serde_json::Value = serde_json::from_str(&header).unwrap();
        let mut vectors = Vec::new();
        if let (Some(n), Some(d)) = (v["count"].as_u64(), v["dim"].as_u64()) {
            for _ in 0..n {
                let mut row = vec![0u8; usize::try_from(d * 4).unwrap()];
                out.read_exact(&mut row).unwrap();
                vectors.push(
                    row.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .collect(),
                );
            }
        }
        // A refusal's reason names the backend's own failure; only its kind must agree.
        let header = if v.get("error").is_some() {
            "error".to_owned()
        } else {
            header.trim_end().to_owned()
        };
        got.push((header, vectors));
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    (dim, got)
}

#[test]
#[ignore = "needs a real e5 model in both forms and ONNX Runtime, see the module docs"]
fn the_onnx_and_native_backends_answer_one_stream_alike() {
    let var = |k: &str| PathBuf::from(std::env::var_os(k).unwrap_or_else(|| panic!("{k}")));
    let engine = var("ULEI_GARDEN_ENGINE_DIR");
    let onnx = var("ULEI_GARDEN_ONNX_DIR");
    let ort = var("ULEI_GARDEN_ORT_LIB");
    let long = "Fogtown expects drizzle. ".repeat(2_000);
    let lines: Vec<String> = [
        "not json".to_owned(),
        r#"{"kind": "summary", "texts": ["a"]}"#.to_owned(),
        r#"{"kind": "query", "texts": [1]}"#.to_owned(),
        serde_json::json!({"kind": "query", "texts": ["Which cafe has a green door?"]}).to_string(),
        serde_json::json!({"kind": "query", "texts": [""]}).to_string(),
        serde_json::json!({"kind": "passage", "texts": []}).to_string(),
        serde_json::json!({"kind": "passage", "texts": [
            "Cafe Lumen has a green door.", "", "Кафе «Люмен» с зелёной дверью.", long,
        ]})
        .to_string(),
    ]
    .into();
    let engine = engine.to_str().unwrap();
    let (native_dim, native) = answers(&["--model-dir", engine], &lines);
    let onnx_args = [
        "--model-dir",
        onnx.to_str().unwrap(),
        "--ort-lib",
        ort.to_str().unwrap(),
        "--threads",
        "1",
        "--onnx-pooling",
        "mean",
        "--onnx-query-prefix",
        "query: ",
        "--onnx-passage-prefix",
        "passage: ",
    ];
    let (onnx_dim, onnx) = answers(&onnx_args, &lines);
    assert_eq!(native_dim, onnx_dim);
    // The empty text is the bare prefix, which ends in a space: the ONNX helper's tokenizer
    // (collapse-spaces) adds a piece for it that the engine's (whitespace-split, ADR-0086 open
    // question 10) does not, so it gets only the looser bound.
    let bounds: [&[f32]; 7] = [
        &[],
        &[],
        &[],
        &[0.98],
        &[0.9],
        &[],
        &[0.98, 0.9, 0.98, 0.98],
    ];
    for (((nh, nv), (oh, ov)), bound) in native.iter().zip(&onnx).zip(bounds) {
        assert_eq!(nh, oh);
        assert_eq!(nv.len(), bound.len(), "{nh}");
        assert_eq!(ov.len(), bound.len(), "{oh}");
        for ((a, b), least) in nv.iter().zip(ov).zip(bound) {
            let cos: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            eprintln!("cosine {cos} for {nh}");
            assert!(cos > *least, "cosine {cos} for {nh}");
        }
    }
}
