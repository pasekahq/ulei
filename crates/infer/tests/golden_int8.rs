//! Golden test of ADR-0086 E3 (feature `bert`): the int8 forward pass on the engine's weight
//! file against the `PyTorch` fp32 dumps of E0 and against the Python simulation of the same
//! quantisation scheme.
//!
//! Needs `ULEI_INFER_INT8_FILE` (the weight file `ulei-garden-embed convert` wrote) and
//! `ULEI_INFER_GOLDEN_DIR` (the fp32 dump directory holding `manifest.json`); skips when either
//! is unset. Acceptance against fp32: pooled cosine >= 0.999 on every case (ADR-0086 section
//! 4, E3). With `ULEI_INFER_SIM_DIR` (dumps of the simulated scheme in the same format) it also
//! compares every tensor with the simulation: max abs diff and pooled cosine. With
//! `ULEI_INFER_MODEL_DIR` (the fp32 checkpoint) it also times the fp32 forward pass of each case
//! (informational). `ULEI_INFER_GOLDEN_CASES` (comma-separated ids) limits the cases.
//!
//! Text in (ADR-0086 E2): the file's tokenizer must give each case's `input_ids` from its
//! `prefixed_text` exactly, and `Encoder::embed_text` the same bytes as the forward pass on the
//! dumped ids.
//!
//! Two narrower checks against the simulation isolate the arithmetic from the propagation of
//! rare int8 rounding flips: `ULEI_INFER_SIM_LINEAR_DIR` (inputs and outputs of every linear
//! layer of the simulation, `linears.json`) runs each linear layer of the file on the
//! simulation's own input; with `ULEI_INFER_SIM_DIR`, every layer also runs on the
//! simulation's output of the layer before it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;
use ulei_infer::bert::{Config, Encoder, Pooling, Workspace};
use ulei_infer::kernels;
use ulei_infer::weights::Checkpoint;
use ulei_infer::weights::file::WeightFile;

const MIN_COSINE: f64 = 0.999;

/// Bound against the simulation of the same scheme, end to end. Not tighter: the scheme is
/// chaotic under float rounding. A value within a few ulp of an int8 rounding boundary
/// rounds the other way under any change of float order (`LayerNorm`, softmax, GELU, the
/// block combination), and the one-step change it makes grows through the layers. The
/// simulation against itself with only its `LayerNorm` in f64 differs as much (E3 report). The
/// arithmetic itself is checked per linear layer by `int8_linear_layers_against_simulation`.
const SIM_MIN_COSINE: f64 = 0.999;

/// Bound per linear layer on the simulation's own input: `|d| <= SIM_LINEAR_REL * (1 + |y|)`.
const SIM_LINEAR_REL: f32 = 1e-5;

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
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

/// `||a - b|| / ||b||`.
fn rel_err(a: &[f32], b: &[f32]) -> f64 {
    let d: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    let n: f64 = b.iter().map(|y| f64::from(*y).powi(2)).sum();
    (d / n).sqrt()
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

fn manifest(dir: &Path) -> Value {
    let text = std::fs::read_to_string(dir.join("manifest.json")).expect("manifest.json");
    serde_json::from_str(&text).expect("manifest.json is JSON")
}

fn tensor(dir: &Path, case: &Value, name: &str) -> Vec<f32> {
    read_f32(&dir.join(case["tensors"][name]["file"].as_str().expect("tensor file")))
}

#[test]
#[allow(clippy::too_many_lines)]
fn int8_forward_against_fp32_and_simulation() {
    let (Some(file), Some(golden)) = (
        std::env::var_os("ULEI_INFER_INT8_FILE").map(PathBuf::from),
        std::env::var_os("ULEI_INFER_GOLDEN_DIR").map(PathBuf::from),
    ) else {
        eprintln!("skipped: set ULEI_INFER_INT8_FILE and ULEI_INFER_GOLDEN_DIR");
        return;
    };
    let sim = std::env::var_os("ULEI_INFER_SIM_DIR").map(PathBuf::from);
    let model = std::env::var_os("ULEI_INFER_MODEL_DIR").map(PathBuf::from);
    let only: Option<Vec<String>> = std::env::var("ULEI_INFER_GOLDEN_CASES")
        .ok()
        .map(|s| s.split(',').map(str::to_string).collect());

    let started = Instant::now();
    let weights = WeightFile::open(&file).unwrap();
    let encoder = Encoder::from_file(&weights).unwrap();
    let mut ws = Workspace::new();
    eprintln!(
        "weight file read in {:.0} ms, act_block {}",
        started.elapsed().as_secs_f64() * 1e3,
        weights.meta()["act_block"]
    );
    let gold = manifest(&golden);
    assert_eq!(
        weights.meta()["source"]["checkpoint_sha256"],
        gold["checkpoint_sha256"],
        "the weight file was converted from another checkpoint than the dumps"
    );
    assert_eq!(weights.meta()["config"], gold["config"]);
    let sim_manifest = sim.as_deref().map(manifest);
    let fp32_ckpt = model
        .as_deref()
        .map(|m| Checkpoint::open(&m.join("model.safetensors")).unwrap());
    let fp32 = fp32_ckpt.as_ref().map(|c| {
        let pooling = Pooling::parse(gold["pooling"].as_str().unwrap()).unwrap();
        let text = std::fs::read_to_string(model.as_deref().unwrap().join("config.json")).unwrap();
        Encoder::new(Config::from_json(&text, pooling).unwrap(), c).unwrap()
    });

    let layers = encoder.config().num_layers;
    let mut names = vec!["emb".to_string()];
    names.extend((0..layers).map(|i| format!("layer_{i:02}")));
    let mut failures = Vec::new();
    let mut lines_fp32 = Vec::new();
    let mut lines_sim = Vec::new();
    let mut worst_cos = 1.0f64;
    let mut worst_sim_abs = 0.0f32;
    let mut worst_sim_cos = 1.0f64;
    let mut timing = Vec::new();
    for (n, case) in gold["cases"].as_array().unwrap().iter().enumerate() {
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
        let t0 = Instant::now();
        let f = encoder.forward(&ids, true).unwrap();
        let pooled = encoder.pool(&f);
        let int8_ms = t0.elapsed().as_secs_f64() * 1e3;

        // Text in: the same ids from the text, the same bytes from the vector.
        let text = case["prefixed_text"].as_str().unwrap();
        let from_text = encoder.tokenizer().unwrap().encode(text).unwrap();
        if from_text != ids {
            failures.push(format!("{id}: the tokenizer's ids differ from the dump's"));
        }
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        // In one workspace kept across the cases: the bits must not depend on what it held.
        if bits(&encoder.embed_text_with(&mut ws, text).unwrap()) != bits(&pooled) {
            failures.push(format!(
                "{id}: the text-in vector differs from the ids-in vector"
            ));
        }
        let fp32_ms = fp32.as_ref().map(|e| {
            let t0 = Instant::now();
            let _ = e.embed(&ids).unwrap();
            t0.elapsed().as_secs_f64() * 1e3
        });
        timing.push((id.to_string(), ids.len(), int8_ms, fp32_ms));

        // Against PyTorch fp32: relative error per tensor (informational), pooled cosine.
        let mut line = format!("{id:<18} {:>4}", ids.len());
        for (name, got) in names.iter().zip(&f.intermediates) {
            write!(line, " {:>7.4}", rel_err(got, &tensor(&golden, case, name))).unwrap();
        }
        let cos = cosine(&pooled, &tensor(&golden, case, "pooled"));
        write!(line, " {cos:>9.6}").unwrap();
        lines_fp32.push(line);
        worst_cos = worst_cos.min(cos);
        if cos.is_nan() || cos < MIN_COSINE {
            failures.push(format!("{id}: pooled cosine against fp32 {cos}"));
        }

        // Against the simulation of the same scheme.
        if let (Some(dir), Some(m)) = (sim.as_deref(), sim_manifest.as_ref()) {
            let sc = &m["cases"][n];
            assert_eq!(sc["id"], case["id"]);
            let mut line = format!("{id:<18} {:>4}", ids.len());
            let mut case_worst = 0.0f32;
            for (name, got) in names.iter().zip(&f.intermediates) {
                let d = max_abs_diff(got, &tensor(dir, sc, name));
                case_worst = case_worst.max(d);
                write!(line, " {d:>8.1e}").unwrap();
            }
            let want = tensor(dir, sc, "pooled");
            let c = cosine(&pooled, &want);
            let pd = max_abs_diff(&pooled, &want);
            write!(line, " {pd:>8.1e} {:>9.2e}", 1.0 - c).unwrap();
            lines_sim.push(line);
            worst_sim_abs = worst_sim_abs.max(case_worst);
            worst_sim_cos = worst_sim_cos.min(c);
            if c.is_nan() || c < SIM_MIN_COSINE {
                failures.push(format!("{id}: pooled cosine against the simulation {c}"));
            }
        }
    }

    let mut header = format!("{:<18} {:>4}", "case", "T");
    for n in &names {
        write!(header, " {:>7}", n.replace("layer_", "L")).unwrap();
    }
    eprintln!("\nint8 vs PyTorch fp32: ||d|| / ||fp32|| per tensor, pooled cosine");
    eprintln!("{header} {:>9}", "cos");
    for l in &lines_fp32 {
        eprintln!("{l}");
    }
    eprintln!("worst pooled cosine against fp32 {worst_cos:.6}");
    if sim.is_some() {
        let mut header = format!("{:<18} {:>4}", "case", "T");
        for n in &names {
            write!(header, " {:>8}", n.replace("layer_", "L")).unwrap();
        }
        eprintln!("\nint8 vs simulation: max abs diff per tensor, pooled max abs diff, 1 - cos");
        eprintln!("{header} {:>8} {:>9}", "pooled", "1-cos");
        for l in &lines_sim {
            eprintln!("{l}");
        }
        eprintln!(
            "worst tensor max abs diff against the simulation {worst_sim_abs:e}, worst pooled cosine {worst_sim_cos:.9}"
        );
    }
    eprintln!("\ntiming (ms, one thread, intermediates kept for int8)");
    for (id, t, i, f) in &timing {
        let f = f.map_or_else(|| "-".to_string(), |f| format!("{f:.1}"));
        eprintln!("{id:<18} {t:>4} int8 {i:>9.1} fp32 {f:>9}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Every linear layer of the weight file on the simulation's input to it: the same int8
/// arithmetic up to the order of the float operations that combine block sums.
#[test]
#[allow(clippy::many_single_char_names)]
fn int8_linear_layers_against_simulation() {
    let (Some(file), Some(dir)) = (
        std::env::var_os("ULEI_INFER_INT8_FILE").map(PathBuf::from),
        std::env::var_os("ULEI_INFER_SIM_LINEAR_DIR").map(PathBuf::from),
    ) else {
        eprintln!("skipped: set ULEI_INFER_INT8_FILE and ULEI_INFER_SIM_LINEAR_DIR");
        return;
    };
    let weights = WeightFile::open(&file).unwrap();
    let block = usize::try_from(weights.meta()["act_block"].as_u64().unwrap()).unwrap();
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("linears.json")).unwrap()).unwrap();
    let (mut worst_abs, mut worst_rel, mut count, mut values) = (0.0f32, 0.0f32, 0usize, 0usize);
    let mut failures = Vec::new();
    let mut worst_at = String::new();
    for entry in index["linears"].as_array().unwrap() {
        let name = entry["linear"].as_str().unwrap();
        let x = read_f32(&dir.join(entry["x"].as_str().unwrap()));
        let want = read_f32(&dir.join(entry["y"].as_str().unwrap()));
        let w = weights.i8(&format!("{name}.weight")).unwrap();
        let ws = weights.f32(&format!("{name}.scale")).unwrap();
        let b = weights.f32(&format!("{name}.bias")).unwrap();
        let q = kernels::quantize_rows(&x, w.shape[1], block);
        let got = kernels::linear_i8(&q, w.data, ws.data, b.data);
        assert_eq!(got.len(), want.len());
        for (g, r) in got.iter().zip(&want) {
            let d = (g - r).abs();
            if d > SIM_LINEAR_REL * (1.0 + r.abs()) {
                failures.push(format!("{} {name}: {g} vs {r}", entry["case"]));
            }
            let rel = d / (1.0 + r.abs());
            if d > worst_abs {
                worst_abs = d;
                worst_at = format!("{} {name}: {g} vs {r}", entry["case"]);
            }
            worst_rel = worst_rel.max(rel);
        }
        count += 1;
        values += got.len();
    }
    eprintln!(
        "{count} linear calls, {values} outputs: max abs diff {worst_abs:e} ({worst_at}), max abs diff / (1 + |y|) {worst_rel:e}"
    );
    assert!(
        failures.is_empty(),
        "{} outputs off: {:?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}

/// Every layer on the simulation's output of the layer before it.
#[test]
fn int8_layers_teacher_forced_against_simulation() {
    let (Some(file), Some(dir)) = (
        std::env::var_os("ULEI_INFER_INT8_FILE").map(PathBuf::from),
        std::env::var_os("ULEI_INFER_SIM_DIR").map(PathBuf::from),
    ) else {
        eprintln!("skipped: set ULEI_INFER_INT8_FILE and ULEI_INFER_SIM_DIR");
        return;
    };
    let weights = WeightFile::open(&file).unwrap();
    let encoder = Encoder::from_file(&weights).unwrap();
    let m = manifest(&dir);
    let layers = encoder.config().num_layers;
    eprintln!("teacher-forced: per layer max abs diff; share of values off by more than 1e-4");
    let mut worst = 0.0f32;
    let (mut off, mut total) = (0usize, 0usize);
    for case in m["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let tokens = case["input_ids"].as_array().unwrap().len();
        let mut line = format!("{id:<18} {tokens:>4}");
        let mut prev = tensor(&dir, case, "emb");
        let mut case_off = 0usize;
        for i in 0..layers {
            let want = tensor(&dir, case, &format!("layer_{i:02}"));
            let got = encoder.forward_layer(i, &prev, tokens).unwrap();
            let d = max_abs_diff(&got, &want);
            case_off += got
                .iter()
                .zip(&want)
                .filter(|(g, w)| (*g - *w).abs() > 1e-4)
                .count();
            total += got.len();
            worst = worst.max(d);
            write!(line, " {d:>8.1e}").unwrap();
            prev = want;
        }
        off += case_off;
        write!(line, " {case_off:>6}").unwrap();
        eprintln!("{line}");
    }
    #[allow(clippy::cast_precision_loss)]
    let share = off as f64 / total as f64;
    eprintln!(
        "worst teacher-forced layer max abs diff {worst:e}; {off} of {total} values off by > 1e-4 ({share:.2e})"
    );
}

/// ADR-0086 section 5: every kernel this CPU runs gives the same pooled vector, bit for bit,
/// on every golden case. Switching the kernel is process-wide, but every kernel's output is
/// the same, so the other tests of this binary are not affected.
#[test]
fn every_kernel_gives_bit_identical_vectors() {
    let (Some(file), Some(golden)) = (
        std::env::var_os("ULEI_INFER_INT8_FILE").map(PathBuf::from),
        std::env::var_os("ULEI_INFER_GOLDEN_DIR").map(PathBuf::from),
    ) else {
        eprintln!("skipped: set ULEI_INFER_INT8_FILE and ULEI_INFER_GOLDEN_DIR");
        return;
    };
    let weights = WeightFile::open(&file).unwrap();
    let encoder = Encoder::from_file(&weights).unwrap();
    let mut ws = Workspace::new();
    let available = kernels::Kernel::all_available();
    let names: Vec<&str> = available.iter().map(|k| k.name()).collect();
    eprintln!("kernels on this CPU: {names:?}");
    for case in manifest(&golden)["cases"].as_array().unwrap() {
        let ids: Vec<u32> = case["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect();
        let vectors: Vec<Vec<u32>> = available
            .iter()
            .map(|&k| {
                assert!(kernels::select(k));
                let v = encoder.embed_with(&mut ws, &ids).unwrap();
                v.iter().map(|x| x.to_bits()).collect()
            })
            .collect();
        for (k, v) in available.iter().zip(&vectors) {
            assert_eq!(v, &vectors[0], "{}: case {}", k.name(), case["id"]);
        }
    }
    kernels::select(kernels::Kernel::best());
}
