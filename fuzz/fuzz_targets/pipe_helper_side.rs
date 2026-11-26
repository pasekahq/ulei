//! The helper's side of the daemon-helper pipe (embed/pipe.rs) on an arbitrary request stream,
//! with the fake embedder: both the lanes protocol (`serve_lanes`) and the single-call one
//! (`serve`). Whatever comes in, what goes out must stay framed: the ready line, then for each
//! request line exactly one answer, a JSON header line followed by exactly count x dim x 4
//! bytes, or an error header; an id in a header must be one a request carried. The run must end
//! when the input ends (no hang: libFuzzer's -timeout).
#![no_main]
use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use ulei_garden::embed::HashEmbedder;
use ulei_garden::embed::pipe::{self, Identity};

const DIM: usize = 8;

fn identity() -> Identity {
    Identity {
        name: "hash-ngram-3-8".into(),
        model_sha256: "0".repeat(64),
        tokenizer_sha256: "0".repeat(64),
        dim: DIM,
    }
}

/// Splits `out` into its frames; panics on anything that is not a frame.
fn frames(out: &[u8], lanes: bool) -> Vec<Value> {
    let mut at = 0;
    let mut headers = Vec::new();
    let mut first = true;
    while at < out.len() {
        let nl = out[at..].iter().position(|&b| b == b'\n').expect("header line ends with a newline");
        let line = &out[at..at + nl];
        at += nl + 1;
        let v: Value = serde_json::from_slice(line).expect("header is JSON");
        if first {
            first = false;
            assert_eq!(v.get("dim").and_then(Value::as_u64), Some(DIM as u64), "ready line");
            assert_eq!(v.get("lanes").is_some(), lanes, "lanes announced iff serve_lanes");
            continue;
        }
        if let Some(count) = v.get("count").and_then(Value::as_u64) {
            assert_eq!(v.get("dim").and_then(Value::as_u64), Some(DIM as u64));
            let len = count as usize * DIM * 4;
            assert!(at + len <= out.len(), "answer body cut short");
            for x in out[at..at + len].chunks_exact(4) {
                assert!(f32::from_le_bytes(x.try_into().unwrap()).is_finite(), "finite vector");
            }
            at += len;
        } else {
            assert!(v.get("error").and_then(Value::as_str).is_some(), "header without count or error: {v}");
        }
        headers.push(v);
    }
    headers
}

fuzz_target!(|data: &[u8]| {
    let embedder = HashEmbedder::new(DIM);
    let id = identity();
    let lines = data.iter().filter(|&&b| b == b'\n').count() + usize::from(!data.ends_with(b"\n") && !data.is_empty());
    let mut out = Vec::new();
    let _ = pipe::serve(&embedder, &id, Cursor::new(data), &mut out);
    let single = frames(&out, false);
    assert!(single.len() <= lines, "more answers than request lines");
    // A read error (invalid UTF-8) ends the loop early: then fewer answers, never more.
    let mut out = Vec::new();
    let _ = pipe::serve_lanes(&embedder, &id, Cursor::new(data), &mut out, || {});
    let laned = frames(&out, true);
    assert!(laned.len() <= lines, "more answers than request lines (lanes)");
    // Every id in an answer is one some request carried.
    let asked: Vec<u64> = data
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
        .filter_map(|v| v.get("id").and_then(Value::as_u64))
        .collect();
    for h in &laned {
        if let Some(i) = h.get("id").and_then(Value::as_u64) {
            assert!(asked.contains(&i), "answer id {i} that no request carried");
        }
    }
});
