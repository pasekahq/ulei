//! `ulei-garden-embed`: the embedder helper of `ulei-garden serve` (ADR-0047 sections 1 and 2).
//!
//! ```text
//! ulei-garden-embed [--embedder auto|native|onnx|fake] --model-dir DIR [--ort-lib LIB]
//!                   [--threads N] [--onnx-pooling cls|mean]
//!                   [--onnx-query-prefix TEXT] [--onnx-passage-prefix TEXT]
//! ulei-garden-embed convert --checkpoint DIR --token-order FILE --pooling mean|cls
//!                           --query-prefix TEXT --passage-prefix TEXT
//!                           [--act-block N] [--tokenizer FILE]
//!                           [--tokenizer-pipeline whitespace-split|collapse-spaces]
//!                           [--max-tokens N] --out FILE
//! ulei-garden-embed verify FILE
//! ```
//! The backend is picked by `--embedder`, by default `auto`: by the model file in `--model-dir`
//! (ADR-0087 E6). A directory with the encoder engine's weight file `model.ulei-infer` runs
//! `native`, one with `onnx/model_quantized.onnx` runs `onnx`, one with both is refused.
//! `native` runs the encoder engine of `ulei-infer` on that weight file, one text at a time on
//! the main thread, with the pooling, tokenizer and query and passage prefixes of its header,
//! and exists only in a build with the `native` feature; it ignores `--ort-lib` and `--threads`.
//! Its identity is the content digest sealed in the weight file's preamble (ADR-0087 E6c: the
//! SHA-256 of the whole file with the digest's own bytes read as zeros), which covers the
//! tokenizer, in both digests of the ready line; the start reads no blob to check it (only the
//! preamble, the file's length, the header and the tensor table: a file that fails those is
//! refused as "model file corrupted"), `verify` does. `onnx` runs bge-m3 through the ONNX Runtime library `--ort-lib` on
//! `--threads` intra-op threads and exists only in a build with the `onnx` feature; the
//! `--onnx-*` flags (default CLS pooling, no prefixes) run an e5 export instead, to compare
//! it with `native`; the daemon passes none of them. `fake` is the crate's deterministic
//! `HashEmbedder` and ignores the rest. `onnx` and `fake` take the SHA-256 of the two
//! artifacts in `--model-dir` into the model's identity. Speaks `ulei_garden::embed::pipe` on
//! stdin and stdout until stdin ends; a failure to start is the pipe's refusal line and exit 1.
//!
//! `convert` writes the encoder engine's int8 weight file (ADR-0087 section 2) from the fp32
//! checkpoint in `--checkpoint` (`config.json`, `model.safetensors`): `--token-order` is the
//! token table's row order, `vocab_size` little-endian `u32` token ids by descending frequency
//! in a fixed public text sample; `--act-block` is the activation block size B (default 32);
//! `--query-prefix` and `--passage-prefix` are the texts the model wants before a query and a
//! passage (`query: ` and `passage: ` for e5, empty for bge-m3), required so none is left out
//! by mistake. The tokenizer section comes from `--tokenizer` (default `tokenizer.json` in the
//! checkpoint directory): `--tokenizer-pipeline` picks the reference it reproduces,
//! `whitespace-split` (the default: the `transformers` tokenizer class, which the golden dumps
//! use) or `collapse-spaces` (the file's own pipeline, which the ONNX helper's tokenizer runs);
//! `--max-tokens` is the truncation length (default 512, ADR-0087 E2). The SHA-256 of the
//! checkpoint, of the order file and of the tokenizer file go into the header; the output is
//! deterministic. The content digest is hashed while the file is written and sealed into its
//! preamble before the file is renamed into place. It prints one JSON line with the output's
//! size, its content digest (`content_sha256`, the helper's identity) and the SHA-256 of the
//! file as `sha256sum` sees it (`sha256`), or one error line on stderr and exits 1.
//!
//! `verify` hashes the whole weight file `FILE` and compares the result with the digest sealed
//! in its preamble, after the checks of the helper's start. On a match it prints one line
//! `OK FILE: content sha256 <hex>, <bytes> bytes, <format>` and exits 0. Otherwise it prints one
//! error line on stderr and exits 1: "model file corrupted: ..." for a file whose bytes are not
//! the converter's (a digest mismatch names both digests; a truncated or longer file both
//! lengths), the format refusal ("convert the model again") for a file of an older format, or
//! the error reading it; a usage error exits 2.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest, Sha256};
use ulei_infer::bert::convert::{self, Options};
use ulei_infer::bert::{Pooling, Prefixes};
use ulei_infer::text::Pipeline;
use ulei_infer::weights::Checkpoint;

use ulei_garden::embed::digest::{self, Hashing};
use ulei_garden::embed::pipe::{self, Identity};
use ulei_garden::embed::{ENGINE_FILE, Embedder, HashEmbedder, ONNX_FILE};

/// Buckets of the fake.
const FAKE_DIM: usize = 256;

struct Args {
    embedder: String,
    model_dir: PathBuf,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    ort_lib: Option<PathBuf>,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    threads: usize,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    onnx_pooling: Pooling,
    #[cfg_attr(not(feature = "onnx"), expect(dead_code))]
    onnx_prefixes: Prefixes,
}

fn parse_args() -> Result<Args, String> {
    let mut embedder = "auto".to_owned();
    let mut model_dir = None;
    let mut ort_lib = None;
    let mut threads = 2;
    let mut onnx_pooling = Pooling::Cls;
    let mut onnx_prefixes = Prefixes::default();
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
            "--onnx-pooling" => {
                onnx_pooling =
                    Pooling::parse(&value.to_string_lossy()).map_err(|e| e.to_string())?;
            }
            "--onnx-query-prefix" => onnx_prefixes.query = utf8(&flag, value)?,
            "--onnx-passage-prefix" => onnx_prefixes.passage = utf8(&flag, value)?,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(Args {
        embedder,
        model_dir: model_dir.ok_or("--model-dir is required")?,
        ort_lib,
        threads,
        onnx_pooling,
        onnx_prefixes,
    })
}

fn utf8(flag: &str, value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("{flag} must be UTF-8"))
}

/// The backend of `--embedder`, `auto` resolved by the model file in the model directory.
fn backend(args: &Args) -> Result<&str, String> {
    if args.embedder != "auto" {
        return Ok(args.embedder.as_str());
    }
    let engine = args.model_dir.join(ENGINE_FILE).exists();
    let onnx = args.model_dir.join(ONNX_FILE).exists();
    match (engine, onnx) {
        (true, true) => Err(format!(
            "{}: holds both {ENGINE_FILE} and {ONNX_FILE}; keep the one to run",
            args.model_dir.display()
        )),
        (true, false) => Ok("native"),
        (false, true) => Ok("onnx"),
        (false, false) => Err(format!(
            "{}: holds neither {ENGINE_FILE} nor {ONNX_FILE}",
            args.model_dir.display()
        )),
    }
}

/// The embedder of `args` and its identity.
fn load(args: &Args) -> Result<(Box<dyn Embedder>, Identity), String> {
    let artifacts = |embedder: Box<dyn Embedder>| {
        let identity = Identity::of(embedder.as_ref(), &args.model_dir)?;
        Ok((embedder, identity))
    };
    match backend(args)? {
        "fake" => artifacts(Box::new(HashEmbedder::new(FAKE_DIM))),
        "onnx" => artifacts(onnx(args)?),
        "native" => native(args),
        other => Err(format!(
            "--embedder {other:?}: expected auto, native, onnx or fake"
        )),
    }
}

#[cfg(feature = "native")]
fn native(args: &Args) -> Result<(Box<dyn Embedder>, Identity), String> {
    let embedder =
        ulei_garden::embed::native::NativeEmbedder::load(&args.model_dir.join(ENGINE_FILE))
            .map_err(|e| e.to_string())?;
    let identity = Identity {
        name: embedder.model_id().to_owned(),
        model_sha256: embedder.sha256().to_owned(),
        tokenizer_sha256: embedder.sha256().to_owned(),
        dim: embedder.dim(),
    };
    Ok((Box::new(embedder), identity))
}

#[cfg(not(feature = "native"))]
fn native(_: &Args) -> Result<(Box<dyn Embedder>, Identity), String> {
    Err("--embedder native: this build has no native feature".into())
}

#[cfg(feature = "onnx")]
fn onnx(args: &Args) -> Result<Box<dyn Embedder>, String> {
    let lib = args.ort_lib.as_deref().ok_or("--ort-lib is required")?;
    ulei_garden::embed::onnx::OnnxEmbedder::load_with(
        &args.model_dir,
        lib,
        args.threads,
        args.onnx_pooling,
        args.onnx_prefixes.clone(),
    )
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
    let mut tokenizer = None;
    let (mut query_prefix, mut passage_prefix) = (None, None);
    let mut pipeline = Pipeline::WhitespaceSplit;
    let mut max_tokens = convert::DEFAULT_MAX_TOKENS;
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
            "--tokenizer" => tokenizer = Some(PathBuf::from(value)),
            "--query-prefix" => query_prefix = Some(utf8(&flag, value)?),
            "--passage-prefix" => passage_prefix = Some(utf8(&flag, value)?),
            "--tokenizer-pipeline" => {
                pipeline = Pipeline::parse(&value.to_string_lossy()).map_err(|e| e.to_string())?;
            }
            "--max-tokens" => {
                max_tokens = value
                    .to_string_lossy()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 2)
                    .ok_or("--max-tokens must be a count of at least 2")?;
            }
            "--out" => out = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let checkpoint = checkpoint.ok_or("--checkpoint is required")?;
    let order_path = order.ok_or("--token-order is required")?;
    let pooling = pooling.ok_or("--pooling is required")?;
    let prefixes = Prefixes {
        query: query_prefix.ok_or("--query-prefix is required (empty for none)")?,
        passage: passage_prefix.ok_or("--passage-prefix is required (empty for none)")?,
    };
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
    let tokenizer_path = tokenizer.unwrap_or_else(|| checkpoint.join("tokenizer.json"));
    let tokenizer_bytes = std::fs::read(&tokenizer_path).map_err(|e| at(&tokenizer_path, &e))?;
    let tokenizer_sha = hex(&Sha256::digest(&tokenizer_bytes));
    let tokenizer_json = String::from_utf8(tokenizer_bytes).map_err(|e| at(&tokenizer_path, &e))?;
    let model = Checkpoint::open(&model_path).map_err(|e| at(&model_path, &e))?;
    let opts = Options {
        pooling,
        prefixes,
        act_block: block,
        row_order: &row_order,
        source: serde_json::json!({
            "checkpoint_sha256": model_sha,
            "token_order_sha256": order_sha,
            "tokenizer_sha256": tokenizer_sha,
        }),
        tokenizer_json: &tokenizer_json,
        pipeline,
        max_tokens,
    };
    let (written, content) = write_sealed(&out, |w| convert::convert(&model, &config, &opts, w))
        .map_err(|e| at(&out, &e))?;
    let sha = sha256_file(&out).map_err(|e| at(&out, &e))?;
    Ok(serde_json::json!({
        "bytes": written,
        "content_sha256": hex(&content),
        "sha256": sha,
    })
    .to_string())
}

/// Writes a weight file with `write` beside `out` (`out.part`), hashing it as it is written,
/// seals its content digest into it and renames it to `out`, so a failed run leaves no partial
/// file there; returns the bytes written and the digest.
fn write_sealed(
    out: &Path,
    write: impl FnOnce(&mut Hashing<BufWriter<File>>) -> ulei_infer::Result<u64>,
) -> Result<(u64, [u8; 32]), String> {
    let mut part = out.to_path_buf().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let file = File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let sealed = || -> Result<(u64, [u8; 32]), String> {
        let mut w = Hashing::new(BufWriter::new(file));
        let n = write(&mut w).map_err(|e| e.to_string())?;
        let (content, w) = w.finish();
        let mut file = w.into_inner().map_err(|e| e.error().to_string())?;
        digest::seal(&mut file, &content).map_err(|e| e.to_string())?;
        std::fs::rename(&part, out).map_err(|e| e.to_string())?;
        Ok((n, content))
    };
    sealed().inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })
}

/// `ulei-garden-embed verify FILE`: see the crate docs above. `Err` holds the exit code and the
/// line for stderr.
fn verify(args: impl Iterator<Item = OsString>) -> Result<String, (u8, String)> {
    let args: Vec<OsString> = args.collect();
    let [path] = args.as_slice() else {
        return Err((2, "usage: ulei-garden-embed verify FILE".into()));
    };
    let path = Path::new(path);
    match digest::verify(path) {
        Ok(v) => Ok(format!(
            "OK {}: content sha256 {}, {} bytes, {}",
            path.display(),
            v.sha256,
            v.bytes,
            v.format
        )),
        Err(e) => Err((1, format!("{}: {e}", path.display()))),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|a| a == "verify") {
        args.next();
        return match verify(args) {
            Ok(line) => {
                let _ = writeln!(io::stdout(), "{line}");
                ExitCode::SUCCESS
            }
            Err((code, e)) => {
                let _ = writeln!(io::stderr(), "ulei-garden-embed verify: {e}");
                ExitCode::from(code)
            }
        };
    }
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
    let ready = parse_args().and_then(|args| load(&args));
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
