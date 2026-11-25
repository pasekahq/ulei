//! Speed probe of the int8 encoder per kernel (ADR-0087 E5); a diagnostic, not part of the
//! library.
//!
//! ```text
//! speed_probe WEIGHT_FILE INPUTS_JSON KERNEL REPEATS [VECTORS_OUT]
//! ```
//!
//! `INPUTS_JSON` is the probe input of `mem_probe` (`{"golden": [[ids...], ...], "texts":
//! [[ids...], ...]}`); `KERNEL` is `portable`, `avx2` or `neon`. The probe binds the
//! encoder, selects the kernel, then embeds every input (golden first, then texts) `REPEATS`
//! times on one thread and prints one JSON line per input and round: round, index, tokens,
//! milliseconds. The pooled vectors of the first round go to `VECTORS_OUT` as little-endian
//! `f32`, in input order.

use std::io::Write;
use std::time::Instant;

use serde_json::{Value, json};
use ulei_infer::bert::Encoder;
use ulei_infer::kernels::{self, Kernel};
use ulei_infer::weights::file::WeightFile;

fn ids_of(v: &Value) -> Vec<Vec<u32>> {
    v.as_array()
        .expect("an array of inputs")
        .iter()
        .map(|ids| {
            ids.as_array()
                .expect("an array of ids")
                .iter()
                .map(|i| u32::try_from(i.as_u64().expect("an id")).expect("a u32 id"))
                .collect()
        })
        .collect()
}

#[allow(clippy::many_single_char_names)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (weights, inputs, kernel, repeats, vectors_out) = match args.as_slice() {
        [w, i, k, r] => (w, i, k, r, None),
        [w, i, k, r, v] => (w, i, k, r, Some(v)),
        _ => panic!("usage: speed_probe WEIGHT_FILE INPUTS_JSON KERNEL REPEATS [VECTORS_OUT]"),
    };
    let kernel = Kernel::ALL
        .into_iter()
        .find(|k| k.name() == kernel)
        .expect("a kernel name");
    assert!(
        kernels::select(kernel),
        "this CPU cannot run {}",
        kernel.name()
    );
    let repeats: usize = repeats.parse().expect("a number of rounds");

    let file = WeightFile::open(std::path::Path::new(weights)).expect("open the weight file");
    let encoder = Encoder::from_file(&file).expect("bind the encoder");
    let inputs: Value = serde_json::from_str(&std::fs::read_to_string(inputs).expect("inputs"))
        .expect("inputs are JSON");
    let mut all = ids_of(&inputs["golden"]);
    all.extend(ids_of(&inputs["texts"]));

    let mut out = std::io::stdout().lock();
    let mut vectors = Vec::new();
    for round in 0..repeats {
        for (i, ids) in all.iter().enumerate() {
            let t = Instant::now();
            let v = encoder.embed(ids).expect("embed");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if round == 0 {
                vectors.extend_from_slice(&v);
            }
            let line = json!({"kernel": kernels::active().name(), "round": round, "i": i,
                              "tokens": ids.len(), "ms": ms});
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }
    if let Some(path) = vectors_out {
        let bytes: Vec<u8> = vectors.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(path, bytes).expect("write the vectors");
    }
}
