//! Golden test of ADR-0087 E1 (feature `bert`): the forward pass against `PyTorch` fp32 dumps
//! (E0), layer by layer.
//!
//! Needs `ULEI_INFER_GOLDEN_DIR` (a model's dump directory holding `manifest.json`) and
//! `ULEI_INFER_MODEL_DIR` (the checkpoint: `config.json`, `model.safetensors`); skips when
//! either is unset. `ULEI_INFER_GOLDEN_CASES` (comma-separated ids) limits the cases.
//! Acceptance: every `emb` / `layer_NN` max abs diff <= 1e-4, pooled cosine >= 0.99999.
//! The table also prints each case's forward wall time (informational; it depends on the
//! build profile and the host's load).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;
use ulei_infer::bert::{Config, Encoder, Pooling};
use ulei_infer::weights::Checkpoint;

const MAX_ABS: f32 = 1e-4;
const MIN_COSINE: f64 = 0.99999;

fn dirs() -> Option<(PathBuf, PathBuf)> {
    let golden = std::env::var_os("ULEI_INFER_GOLDEN_DIR")?;
    let model = std::env::var_os("ULEI_INFER_MODEL_DIR")?;
    Some((golden.into(), model.into()))
}

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(bytes.len() % 4, 0, "{}", path.display());
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let na: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    dot / (na * nb)
}

fn shape(t: &Value) -> Vec<usize> {
    t["shape"]
        .as_array()
        .expect("shape is an array")
        .iter()
        .map(|d| d.as_u64().and_then(|d| usize::try_from(d).ok()))
        .collect::<Option<Vec<usize>>>()
        .expect("shape holds sizes")
}

#[test]
fn forward_matches_pytorch_fp32() {
    let Some((golden, model)) = dirs() else {
        eprintln!("skipped: set ULEI_INFER_GOLDEN_DIR and ULEI_INFER_MODEL_DIR");
        return;
    };
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(golden.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["format"], 1);
    let pooling = Pooling::parse(manifest["pooling"].as_str().unwrap()).unwrap();
    let config_text = std::fs::read_to_string(model.join("config.json")).unwrap();
    let config = Config::from_json(&config_text, pooling).unwrap();
    let ckpt_config: Value = serde_json::from_str(&config_text).unwrap();
    assert_eq!(
        ckpt_config, manifest["config"],
        "the dumps were made from another config"
    );
    let layers = config.num_layers;
    let ckpt = Checkpoint::open(&model.join("model.safetensors")).unwrap();
    let encoder = Encoder::new(config, &ckpt).unwrap();
    let only: Option<Vec<String>> = std::env::var("ULEI_INFER_GOLDEN_CASES")
        .ok()
        .map(|s| s.split(',').map(str::to_string).collect());

    let mut names = vec!["emb".to_string()];
    names.extend((0..layers).map(|i| format!("layer_{i:02}")));
    let mut header = format!("{:<18} {:>4}", "case", "T");
    for n in &names {
        write!(header, " {:>8}", n.replace("layer_", "L")).unwrap();
    }
    write!(header, " {:>10} {:>9} {:>9}", "1-cos", "worst", "ms").unwrap();
    eprintln!("{header}");

    let mut failures = Vec::new();
    let mut worst_layer = 0.0f32;
    let mut worst_cos = 1.0f64;
    for case in manifest["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        if only.as_ref().is_some_and(|o| !o.iter().any(|c| c == id)) {
            continue;
        }
        let ids: Vec<u32> = case["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect();
        let started = Instant::now();
        let f = encoder.forward(&ids, true).unwrap();
        let millis = started.elapsed().as_secs_f64() * 1e3;
        let mut line = format!("{id:<18} {:>4}", ids.len());
        let mut case_worst = 0.0f32;
        for (name, got) in names.iter().zip(&f.intermediates) {
            let t = &case["tensors"][name.as_str()];
            assert_eq!(shape(t), vec![ids.len(), encoder.config().hidden_size]);
            let want = read_f32(&golden.join(t["file"].as_str().unwrap()));
            let d = max_abs_diff(got, &want);
            write!(line, " {d:>8.1e}").unwrap();
            case_worst = case_worst.max(d);
            if d.is_nan() || d > MAX_ABS {
                failures.push(format!("{id} {name}: max abs diff {d:e}"));
            }
        }
        let t = &case["tensors"]["pooled"];
        let want = read_f32(&golden.join(t["file"].as_str().unwrap()));
        let got = encoder.pool(&f);
        let cos = cosine(&got, &want);
        write!(
            line,
            " {:>10.2e} {case_worst:>9.2e} {millis:>9.1}",
            1.0 - cos
        )
        .unwrap();
        eprintln!("{line}");
        worst_layer = worst_layer.max(case_worst);
        worst_cos = worst_cos.min(cos);
        if cos.is_nan() || cos < MIN_COSINE {
            failures.push(format!("{id} pooled: cosine {cos}"));
        }
    }
    eprintln!("worst layer max abs diff {worst_layer:e}, worst pooled cosine {worst_cos:.9}");
    assert!(failures.is_empty(), "{failures:#?}");
}
