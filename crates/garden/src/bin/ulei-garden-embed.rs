//! `ulei-garden-embed`: the embedder helper of `ulei-garden serve` (ADR-0047 sections 1 and 2).
//!
//! ```text
//! ulei-garden-embed [--embedder onnx|fake] --model-dir DIR [--ort-lib LIB] [--threads N]
//! ```
//! `onnx` (the default) runs bge-m3 through the ONNX Runtime library `--ort-lib` on `--threads`
//! intra-op threads and exists only in a build with the `onnx` feature; `fake` is the crate's
//! deterministic `HashEmbedder` and ignores both. Either way the SHA-256 of the two artifacts in
//! `--model-dir` enter the model's identity. Speaks `ulei_garden::embed::pipe` on stdin and
//! stdout until stdin ends; a failure to start is the pipe's refusal line and exit 1.

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ulei_garden::embed::pipe::{self, Identity};
use ulei_garden::embed::{Embedder, HashEmbedder};

/// Buckets of the fake.
const FAKE_DIM: usize = 256;

struct Args {
    embedder: String,
    model_dir: PathBuf,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    ort_lib: Option<PathBuf>,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    threads: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut embedder = "onnx".to_owned();
    let mut model_dir = None;
    let mut ort_lib = None;
    let mut threads = 2;
    let mut it = std::env::args_os().skip(1);
    while let Some(flag) = it.next() {
        let flag = flag.to_string_lossy().into_owned();
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--embedder" => embedder = value.to_string_lossy().into_owned(),
            "--model-dir" => model_dir = Some(PathBuf::from(value)),
            "--ort-lib" => ort_lib = Some(PathBuf::from(value)),
            "--threads" => {
                threads = value
                    .to_string_lossy()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("--threads must be a count above 0")?;
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(Args {
        embedder,
        model_dir: model_dir.ok_or("--model-dir is required")?,
        ort_lib,
        threads,
    })
}

fn load(args: &Args) -> Result<Box<dyn Embedder>, String> {
    match args.embedder.as_str() {
        "fake" => Ok(Box::new(HashEmbedder::new(FAKE_DIM))),
        "onnx" => onnx(args),
        other => Err(format!("--embedder {other:?}: expected onnx or fake")),
    }
}

#[cfg(feature = "onnx")]
fn onnx(args: &Args) -> Result<Box<dyn Embedder>, String> {
    let lib = args.ort_lib.as_deref().ok_or("--ort-lib is required")?;
    ulei_garden::embed::onnx::OnnxEmbedder::load(&args.model_dir, lib, args.threads)
        .map(|e| Box::new(e) as Box<dyn Embedder>)
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "onnx"))]
fn onnx(_: &Args) -> Result<Box<dyn Embedder>, String> {
    Err("--embedder onnx: this build has no onnx feature".into())
}

fn main() -> ExitCode {
    let ready = parse_args().and_then(|args| {
        let embedder = load(&args)?;
        let identity = Identity::of(embedder.as_ref(), &args.model_dir)?;
        Ok((embedder, identity))
    });
    let (embedder, identity) = match ready {
        Ok(ready) => ready,
        Err(reason) => {
            let _ = pipe::refuse(io::stdout().lock(), &reason);
            return ExitCode::from(1);
        }
    };
    match pipe::serve(
        embedder.as_ref(),
        &identity,
        io::stdin().lock(),
        io::BufWriter::new(io::stdout().lock()),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "ulei-garden-embed: pipe: {e}");
            ExitCode::from(1)
        }
    }
}
