//! `ulei-garden-embed`: the embedder helper of `ulei-garden serve` (ADR-0047 sections 1 and 2).
//!
//! ```text
//! ulei-garden-embed [--embedder onnx|fake] --model-dir DIR [--ort-lib LIB] [--threads N]
//! ulei-garden-embed convert --checkpoint DIR --token-order FILE --pooling mean|cls
//!                           [--act-block N] --out FILE
//! ```
//! `onnx` (the default) runs bge-m3 through the ONNX Runtime library `--ort-lib` on `--threads`
//! intra-op threads and exists only in a build with the `onnx` feature; `fake` is the crate's
//! deterministic `HashEmbedder` and ignores both. Either way the SHA-256 of the two artifacts in
//! `--model-dir` enter the model's identity. Speaks `ulei_garden::embed::pipe` on stdin and
//! stdout until stdin ends; a failure to start is the pipe's refusal line and exit 1.
//!
//! `convert` writes the encoder engine's int8 weight file (ADR-0086 section 2) from the fp32
//! checkpoint in `--checkpoint` (`config.json`, `model.safetensors`): `--token-order` is the
//! token table's row order, `vocab_size` little-endian `u32` token ids by descending frequency
//! in a fixed public text sample; `--act-block` is the activation block size B (default 64).
//! The SHA-256 of the checkpoint and of the order file go into the header; the output is
//! deterministic. It prints one JSON line with the output's size and SHA-256, or one error line
//! on stderr and exits 1.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest, Sha256};
use ulei_infer::bert::Pooling;
use ulei_infer::bert::convert::{self, Options};
use ulei_infer::weights::Checkpoint;

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

/// Lowercase hex SHA-256 of the file at `path`.
fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => hasher.update(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `ulei-garden-embed convert ...`: see the crate docs above.
fn convert(args: impl Iterator<Item = OsString>) -> Result<String, String> {
    let (mut checkpoint, mut order, mut pooling, mut out) = (None, None, None, None);
    let mut block = convert::DEFAULT_BLOCK;
    let mut it = args;
    while let Some(flag) = it.next() {
        let flag = flag.to_string_lossy().into_owned();
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--checkpoint" => checkpoint = Some(PathBuf::from(value)),
            "--token-order" => order = Some(PathBuf::from(value)),
            "--pooling" => {
                pooling =
                    Some(Pooling::parse(&value.to_string_lossy()).map_err(|e| e.to_string())?);
            }
            "--act-block" => {
                block = value
                    .to_string_lossy()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("--act-block must be a count above 0")?;
            }
            "--out" => out = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let checkpoint = checkpoint.ok_or("--checkpoint is required")?;
    let order_path = order.ok_or("--token-order is required")?;
    let pooling = pooling.ok_or("--pooling is required")?;
    let out = out.ok_or("--out is required")?;
    let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());

    let config_path = checkpoint.join("config.json");
    let config = std::fs::read_to_string(&config_path).map_err(|e| at(&config_path, &e))?;
    let model_path = checkpoint.join("model.safetensors");
    let model_sha = sha256_file(&model_path).map_err(|e| at(&model_path, &e))?;
    let order_bytes = std::fs::read(&order_path).map_err(|e| at(&order_path, &e))?;
    let (ids, rest) = order_bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(at(&order_path, &"not a whole number of u32 ids"));
    }
    let row_order: Vec<u32> = ids.iter().map(|b| u32::from_le_bytes(*b)).collect();
    let order_sha = hex(&Sha256::digest(&order_bytes));
    let model = Checkpoint::open(&model_path).map_err(|e| at(&model_path, &e))?;
    let opts = Options {
        pooling,
        act_block: block,
        row_order: &row_order,
        source: serde_json::json!({
            "checkpoint_sha256": model_sha,
            "token_order_sha256": order_sha,
        }),
    };
    // Written beside the target and renamed, so a failed run leaves no partial file there.
    let mut part = out.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let file = File::create(&part).map_err(|e| at(&part, &e))?;
    let written = convert::convert(&model, &config, &opts, BufWriter::new(file))
        .and_then(|n| {
            std::fs::rename(&part, &out)?;
            Ok(n)
        })
        .map_err(|e| {
            let _ = std::fs::remove_file(&part);
            at(&out, &e)
        })?;
    let sha = sha256_file(&out).map_err(|e| at(&out, &e))?;
    Ok(serde_json::json!({"bytes": written, "sha256": sha}).to_string())
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|a| a == "convert") {
        args.next();
        return match convert(args) {
            Ok(line) => {
                let _ = writeln!(io::stdout(), "{line}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "ulei-garden-embed convert: {e}");
                ExitCode::from(1)
            }
        };
    }
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
