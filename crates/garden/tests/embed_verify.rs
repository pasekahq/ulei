//! The content digest of the encoder engine's weight file (ADR-0087 E6c) on the tiny checkpoint
//! of `tiny_bert`, converted at run time: `convert` seals it, `ulei-garden-embed verify` passes
//! the file as written and finds every changed, cut or added byte (a blob, the tokenizer section,
//! the header's `meta`, the preamble) as a corrupted model file with both digests or lengths,
//! and refuses an older format as such; no model, no network.

#![allow(missing_docs, clippy::unwrap_used)]

mod helper_bin;
mod tiny_bert;

use std::fs;
use std::path::{Path, PathBuf};

use helper_bin::embed;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tiny_bert::{blob_at, convert, find, fixture, hex, verify};
use ulei_garden::embed::check::{self, Verdict};
use ulei_garden::embed::digest::{self, VerifyError};
use ulei_garden::embed::{ENGINE_FILE, ONNX_FILE};
use ulei_garden::serve::EmbedderSettings;
use ulei_infer::weights::file::{DIGEST, WeightFile};

/// Stdout and stderr of `verify` on `path`, with its exit code.
fn run(path: &Path) -> (Option<i32>, String, String) {
    let out = verify(&[path]);
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// The SHA-256 of `bytes` with the digest's bytes read as zeros.
fn content(bytes: &[u8]) -> String {
    let mut b = bytes.to_vec();
    b[DIGEST].fill(0);
    hex(&Sha256::digest(&b))
}

#[test]
fn convert_seals_the_content_digest_and_verify_passes_the_file_as_written() {
    let dir = fixture("verify-ok");
    let out = convert(&dir.0, "w.uiw", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line: Value = serde_json::from_slice(&out.stdout).unwrap();
    let path = dir.0.join("w.uiw");
    let bytes = fs::read(&path).unwrap();
    // The digest in the preamble is the SHA-256 of the file with its own bytes zero; the
    // converter prints it and the file's plain SHA-256.
    let sealed = hex(&bytes[DIGEST]);
    assert_eq!(sealed, content(&bytes));
    assert_eq!(line["content_sha256"], sealed);
    assert_eq!(line["sha256"], hex(&Sha256::digest(&bytes)));
    assert_ne!(line["sha256"], line["content_sha256"]);
    assert_eq!(
        u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        bytes.len() as u64
    );
    let file = WeightFile::read(bytes.as_slice()).unwrap();
    assert_eq!(file.digest().map(|d| hex(d)), Some(sealed.clone()));
    assert_eq!(file.meta()["format"], "ulei-infer/bert-int8/4");

    let (code, stdout, stderr) = run(&path);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "OK {}: content sha256 {sealed}, {} bytes, ulei-infer/bert-int8/4\n",
            path.display(),
            bytes.len()
        )
    );
    assert_eq!(stderr, "");
    let v = digest::verify(&path).unwrap();
    assert_eq!((v.sha256, v.bytes), (sealed, bytes.len() as u64));
    // Deterministic: a second conversion gives the same bytes.
    assert!(convert(&dir.0, "again.uiw", &[]).status.success());
    assert_eq!(fs::read(dir.0.join("again.uiw")).unwrap(), bytes);
}

#[test]
fn verify_finds_every_changed_cut_or_added_byte_as_a_corrupted_model_file() {
    let dir = fixture("verify-bad");
    assert!(convert(&dir.0, "w.uiw", &[]).status.success());
    let good = fs::read(dir.0.join("w.uiw")).unwrap();
    let sealed = hex(&good[DIGEST]);
    let path = dir.0.join("bad.uiw");
    let check = |bytes: &[u8], want: &str| {
        fs::write(&path, bytes).unwrap();
        let (code, stdout, stderr) = run(&path);
        assert_eq!(code, Some(3), "{want}: {stdout}{stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.starts_with(&format!(
                "ulei-garden-embed verify: {}: model file corrupted: ",
                path.display()
            )),
            "{stderr}"
        );
        assert!(stderr.contains(want), "{stderr:?} lacks {want:?}");
        assert_eq!(stderr.lines().count(), 1);
    };
    let flipped = |at: usize| {
        let mut b = good.clone();
        b[at] ^= 0x01;
        b
    };
    let mismatch = |b: &[u8]| {
        format!(
            "content SHA-256 is {}, the preamble says {sealed}",
            content(b)
        )
    };

    // A byte of a weight blob, of the token table, of the tokenizer section.
    for name in [
        "encoder.layer.0.attention.self.query.weight",
        "embeddings.word_embeddings.weight",
        "tokenizer.pieces.bytes",
        "tokenizer.pieces.scores",
    ] {
        let b = flipped(blob_at(&good, name) + 1);
        // The helper's start would take it: its layout still holds.
        assert!(WeightFile::read(b.as_slice()).is_ok(), "{name}");
        check(&b, &mismatch(&b));
    }
    // A byte of the header's meta that leaves it valid JSON: the passage prefix.
    let b = flipped(find(&good, b"passage: ").unwrap() + 3);
    assert_eq!(
        WeightFile::read(b.as_slice()).unwrap().meta()["prefixes"]["passage"],
        "pasrage: "
    );
    check(&b, &mismatch(&b));
    // The padding between blobs and the reserved bytes of the preamble.
    // (A blob of 4 f32, 48 bytes of padding after it.)
    let b = flipped(blob_at(&good, "embeddings.LayerNorm.weight") + 20);
    check(&b, &mismatch(&b));
    check(&flipped(12), "reserved preamble field is not zero");
    // The digest itself.
    let mut b = good.clone();
    b[DIGEST.start + 5] ^= 0x80;
    check(&b, &format!("the preamble says {}", hex(&b[DIGEST])));
    // A digest never sealed (a conversion cut short before the seal).
    let mut b = good.clone();
    b[DIGEST].fill(0);
    check(&b, "no content digest in the preamble");

    // Truncated: by one byte, to half, to past the preamble, into the preamble, to nothing.
    for k in [good.len() - 1, good.len() / 2, 100] {
        check(
            &good[..k],
            &format!("the file is {k} bytes, its preamble says {}", good.len()),
        );
    }
    check(&good[..40], "truncated file of 40 bytes");
    check(&[], "truncated file of 0 bytes");
    // Garbage appended.
    let mut b = good.clone();
    b.extend_from_slice(b"garbage");
    check(
        &b,
        &format!(
            "the file is {} bytes, its preamble says {}",
            good.len() + 7,
            good.len()
        ),
    );
    // Not a weight file at all.
    check(&[7u8; 4096], "not a weight file (bad magic)");

    // Each of them is the library's Corrupt too; the good file passes again.
    fs::write(&path, &good[..good.len() - 1]).unwrap();
    assert!(matches!(
        digest::verify(&path),
        Err(VerifyError::Corrupt(_))
    ));
    fs::write(&path, &good).unwrap();
    assert_eq!(run(&path).0, Some(0));
}

#[test]
fn verify_refuses_an_older_format_unreadable_paths_and_bad_usage_on_their_own_terms() {
    let dir = fixture("verify-other");
    assert!(convert(&dir.0, "w.uiw", &[]).status.success());
    let good = fs::read(dir.0.join("w.uiw")).unwrap();
    let path = dir.0.join("old.uiw");
    // A file of format version 2 (ADR-0087 E6, before the digest): convert the model again.
    let mut old = good.clone();
    old[8] = 2;
    fs::write(&path, &old).unwrap();
    let (code, _, stderr) = run(&path);
    assert_eq!(code, Some(1));
    assert!(!stderr.contains("corrupted"), "{stderr}");
    assert!(
        stderr.trim_end().ends_with("convert the model again"),
        "{stderr}"
    );
    assert!(matches!(digest::verify(&path), Err(VerifyError::Format(_))));
    // A missing file and a directory: the error reading it.
    let (code, _, stderr) = run(&dir.0.join("missing.uiw"));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("No such file"), "{stderr}");
    assert!(!stderr.contains("corrupted"), "{stderr}");
    assert_eq!(run(&dir.0).0, Some(1));
    // No file, two files: usage, exit 2.
    for args in [&[][..], &[path.as_path(), path.as_path()]] {
        let out = verify(args);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "ulei-garden-embed verify: usage: ulei-garden-embed verify FILE\n"
        );
    }
}

#[test]
fn the_daemons_check_tells_a_good_a_corrupted_and_an_undecided_file_apart_by_the_exit_code() {
    let dir = fixture("verify-check");
    assert!(convert(&dir.0, "w.uiw", &[]).status.success());
    let good = fs::read(dir.0.join("w.uiw")).unwrap();
    let sealed = hex(&good[DIGEST]);
    let path = dir.0.join("w.uiw");
    let helper = Path::new(embed());
    let deadline = std::time::Duration::from_secs(30);
    assert_eq!(
        check::verify(helper, &path, deadline),
        Verdict::Good(sealed.clone())
    );
    let mut b = good.clone();
    b[blob_at(&good, "encoder.layer.0.attention.self.query.weight") + 1] ^= 1;
    fs::write(&path, &b).unwrap();
    let Verdict::Corrupted(reason) = check::verify(helper, &path, deadline) else {
        panic!("not corrupted");
    };
    assert!(
        reason.starts_with(&format!(
            "model file corrupted: {}: content SHA-256 is ",
            path.display()
        )),
        "{reason}"
    );
    assert!(
        reason.ends_with(&format!("the preamble says {sealed}")),
        "{reason}"
    );
    // An older format, a missing file: no helper can start on it, so the file is unusable
    // (ADR-0087 E12a, F5), not a check without a verdict.
    let mut old = good.clone();
    old[8] = 2;
    fs::write(&path, &old).unwrap();
    assert!(matches!(
        check::verify(helper, &path, deadline),
        Verdict::Unusable(r) if r.starts_with("model file unusable: ") && r.contains("convert the model again")
    ));
    assert!(matches!(
        check::verify(helper, &dir.0.join("missing.uiw"), deadline),
        Verdict::Unusable(_)
    ));
    // A check that hangs is killed at its deadline.
    let hang = dir.0.join("hang.sh");
    // Written by a child process, so no descriptor of it is open here when another test thread
    // forks (ETXTBSY at its exec).
    let wrote = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "printf '#!/bin/sh\\nexec sleep 30\\n' > \"$1\" && chmod 700 \"$1\"",
            "sh",
        ])
        .arg(&hang)
        .status()
        .unwrap();
    assert!(wrote.success());
    let since = std::time::Instant::now();
    assert!(matches!(
        check::verify(&hang, &path, std::time::Duration::from_millis(300)),
        Verdict::Failed(r) if r.contains("no answer within")
    ));
    assert!(since.elapsed() < std::time::Duration::from_secs(10));
}

/// The daemon decides at every check, from the model directory as it is then, whether there is
/// a weight file of the encoder engine to verify, not once at its start (review E13r, finding
/// 9): a directory with only the ONNX model needs no check, the engine's file put in its place
/// is verified by the same check, and a directory with neither is unusable.
#[test]
fn the_daemons_check_follows_the_model_directory_at_every_run() {
    let dir = fixture("verify-follow");
    assert!(convert(&dir.0, "w.uiw", &[]).status.success());
    let model_dir = dir.0.join("model");
    let onnx = model_dir.join(ONNX_FILE);
    fs::create_dir_all(onnx.parent().unwrap()).unwrap();
    fs::write(&onnx, "synthetic onnx model").unwrap();
    let settings = EmbedderSettings {
        helper: PathBuf::from(embed()),
        model_dir: model_dir.clone(),
        ort_library: dir.0.join("no-onnxruntime.so"),
        threads: 1,
        load_timeout: std::time::Duration::from_secs(30),
    };
    let check = settings.check();
    assert_eq!(check(), Verdict::NotApplicable, "the ONNX backend");

    fs::remove_file(&onnx).unwrap();
    fs::copy(dir.0.join("w.uiw"), model_dir.join(ENGINE_FILE)).unwrap();
    let sealed = hex(&fs::read(dir.0.join("w.uiw")).unwrap()[DIGEST]);
    assert_eq!(check(), Verdict::Good(sealed), "the engine's file now");

    fs::remove_file(model_dir.join(ENGINE_FILE)).unwrap();
    assert!(
        matches!(check(), Verdict::Unusable(_)),
        "neither model: no helper can start"
    );
}
