//! Time per operation of one int8 encoder layer on synthetic data (ADR-0086 E7k); a
//! diagnostic, not part of the library.
//!
//! ```text
//! op_probe HIDDEN FFN HEADS TOKENS[,TOKENS...] REPEATS [KERNEL]
//! ```
//!
//! Runs the operations of one int8 layer in the order `bert::Layer` runs them (quantisation of
//! the layer's inputs, the six linear layers, attention, GELU, the residual additions and the
//! two `LayerNorm`s) on random data of the given shape, `REPEATS` times per token count after one
//! warm-up, one thread, and prints one JSON line per token count with the median time of each
//! operation in microseconds per layer. The sum stands for one layer of a model of that shape;
//! times one model's layer count, it is that model's encoder without its embeddings and pooling.

use std::io::Write;
use std::time::Instant;

use serde_json::json;
use ulei_infer::kernels::{self, Kernel, Quant, Scratch, Softmax};
use ulei_infer::ops;

/// xorshift64*, deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Roughly normal, scaled.
    #[allow(clippy::cast_precision_loss)]
    fn normal(&mut self, scale: f32) -> f32 {
        let s: f32 = (0..4)
            .map(|_| (self.next() >> 40) as f32 / (1u64 << 24) as f32)
            .sum();
        (s - 2.0) * scale
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn int8(&mut self) -> i8 {
        (self.next() >> 56) as u8 as i8
    }
}

/// One int8 linear layer's parameters.
struct Lin {
    w: Vec<i8>,
    ws: Vec<f32>,
    b: Vec<f32>,
    k: usize,
}

impl Lin {
    fn new(rng: &mut Rng, out: usize, k: usize) -> Self {
        Self {
            w: (0..out * k).map(|_| rng.int8()).collect(),
            ws: (0..out).map(|_| 0.001 + rng.normal(0.0005).abs()).collect(),
            b: (0..out).map(|_| rng.normal(0.1)).collect(),
            k,
        }
    }
}

/// The quantised input of a layer in buffers kept across calls.
#[derive(Default)]
struct Q {
    values: Vec<i8>,
    scales: Vec<f32>,
}

const BLOCK: usize = 32;

impl Q {
    fn quantise(&mut self, x: &[f32], width: usize) -> Quant<'_> {
        let rows = x.len() / width;
        self.values.resize(x.len(), 0);
        self.scales.resize(rows * width.div_ceil(BLOCK), 0.0);
        kernels::quantize_rows_into(x, width, BLOCK, &mut self.values, &mut self.scales);
        Quant {
            values: &self.values,
            scales: &self.scales,
            width,
            block: BLOCK,
        }
    }
}

const OPS: [&str; 12] = [
    "quant_qkv",
    "lin_qkv",
    "attention",
    "quant_ao",
    "lin_ao",
    "residual_ln1",
    "quant_fi",
    "lin_fi",
    "gelu",
    "quant_fo",
    "lin_fo",
    "residual_ln2",
];

#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [h, f, heads, tokens, repeats, rest @ ..] = args.as_slice() else {
        panic!("usage: op_probe HIDDEN FFN HEADS TOKENS[,TOKENS...] REPEATS [KERNEL]");
    };
    let p = |s: &String| s.parse::<usize>().expect("a number");
    let (h, f, heads, repeats) = (p(h), p(f), p(heads), p(repeats));
    if let Some(k) = rest.first() {
        let k = Kernel::ALL
            .into_iter()
            .find(|x| x.name() == k)
            .expect("a kernel name");
        assert!(kernels::select(k), "this CPU cannot run {}", k.name());
    }
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let lq = Lin::new(&mut rng, h, h);
    let lk = Lin::new(&mut rng, h, h);
    let lv = Lin::new(&mut rng, h, h);
    let lo = Lin::new(&mut rng, h, h);
    let lfi = Lin::new(&mut rng, f, h);
    let lfo = Lin::new(&mut rng, h, f);
    let gamma: Vec<f32> = (0..h).map(|_| 1.0 + rng.normal(0.1)).collect();
    let beta: Vec<f32> = (0..h).map(|_| rng.normal(0.1)).collect();
    let mut scratch = Scratch::default();
    let mut q = Q::default();
    for t in tokens
        .split(',')
        .map(|s| s.parse::<usize>().expect("tokens"))
    {
        let n = t * h;
        let x0: Vec<f32> = (0..n).map(|_| rng.normal(1.0)).collect();
        let (mut x, mut qq, mut kk, mut vv, mut ctx, mut att) = (
            x0.clone(),
            vec![0.0; n],
            vec![0.0; n],
            vec![0.0; n],
            vec![0.0; n],
            vec![0.0; n],
        );
        let mut inner = vec![0.0f32; t * f];
        let mut times: Vec<Vec<f64>> = vec![Vec::new(); OPS.len()];
        let scale = 1.0 / ((h / heads) as f32).sqrt();
        for round in 0..=repeats {
            x.copy_from_slice(&x0);
            let mut tm = [0.0f64; OPS.len()];
            let mut lap = Instant::now();
            let mut mark = |i: usize, lap: &mut Instant| {
                let now = Instant::now();
                tm[i] = (now - *lap).as_secs_f64() * 1e6;
                *lap = now;
            };
            let xq = q.quantise(&x, h);
            mark(0, &mut lap);
            for (l, y) in [(&lq, &mut qq), (&lk, &mut kk), (&lv, &mut vv)] {
                kernels::linear_i8_into(&xq, &l.w, &l.ws, &l.b, y, &mut scratch);
            }
            mark(1, &mut lap);
            kernels::attention(
                &qq,
                &kk,
                &vv,
                t,
                heads,
                scale,
                Softmax::F32,
                &mut ctx,
                &mut scratch,
            );
            mark(2, &mut lap);
            let cq = q.quantise(&ctx, h);
            mark(3, &mut lap);
            kernels::linear_i8_into(&cq, &lo.w, &lo.ws, &lo.b, &mut att, &mut scratch);
            mark(4, &mut lap);
            for (a, r) in att.iter_mut().zip(&x) {
                *a += r;
            }
            ops::layer_norm(&mut att, &gamma, &beta, 1e-12);
            mark(5, &mut lap);
            let aq = q.quantise(&att, h);
            mark(6, &mut lap);
            kernels::linear_i8_into(&aq, &lfi.w, &lfi.ws, &lfi.b, &mut inner, &mut scratch);
            mark(7, &mut lap);
            kernels::gelu(&mut inner);
            mark(8, &mut lap);
            let iq = q.quantise(&inner, lfo.k);
            mark(9, &mut lap);
            kernels::linear_i8_into(&iq, &lfo.w, &lfo.ws, &lfo.b, &mut x, &mut scratch);
            mark(10, &mut lap);
            for (a, r) in x.iter_mut().zip(&att) {
                *a += r;
            }
            ops::layer_norm(&mut x, &gamma, &beta, 1e-12);
            mark(11, &mut lap);
            if round > 0 {
                for (v, t) in times.iter_mut().zip(tm) {
                    v.push(t);
                }
            }
            std::hint::black_box(&x);
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let mut line = serde_json::Map::new();
        line.insert("kernel".into(), json!(kernels::active().name()));
        line.insert("hidden".into(), json!(h));
        line.insert("ffn".into(), json!(f));
        line.insert("tokens".into(), json!(t));
        let mut total = 0.0;
        for (name, v) in OPS.iter().zip(&mut times) {
            let m = med(v);
            total += m;
            line.insert((*name).into(), json!((m * 10.0).round() / 10.0));
        }
        line.insert("total_us".into(), json!((total * 10.0).round() / 10.0));
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", serde_json::Value::Object(line));
    }
}
