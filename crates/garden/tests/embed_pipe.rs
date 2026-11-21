//! The embedder helper `ulei-garden-embed --embedder fake` behind `ulei_garden::embed::pipe`,
//! over synthetic artifact files; no model, no network.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ulei_garden::embed::pipe::{Helper, Identity};
use ulei_garden::embed::{Embedder, HashEmbedder, ONNX_FILE, TOKENIZER_FILE};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");
/// SHA-256 of `abc`.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
/// SHA-256 of the empty file.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A model directory holding `abc` as the model and an empty tokenizer, removed on drop.
struct ModelDir(PathBuf);

impl ModelDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ulei-garden-embed-pipe-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(ONNX_FILE).parent().unwrap()).unwrap();
        fs::write(dir.join(ONNX_FILE), "abc").unwrap();
        fs::write(dir.join(TOKENIZER_FILE), "").unwrap();
        Self(dir)
    }
}

impl Drop for ModelDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_ready_line_carries_both_artifact_digests_and_vectors_cross_the_pipe_bit_for_bit() {
    let dir = ModelDir::new();
    let mut command = Command::new(EMBED);
    command
        .args(["--embedder", "fake", "--model-dir"])
        .arg(&dir.0);
    let helper = Helper::start(command, Duration::from_secs(30)).unwrap();
    let reference = HashEmbedder::new(256);
    assert_eq!(
        helper.identity(),
        &Identity {
            name: reference.model_id().to_owned(),
            model_sha256: ABC.to_owned(),
            tokenizer_sha256: EMPTY.to_owned(),
            dim: 256,
        }
    );
    assert_eq!(
        helper.model_id(),
        format!("{}/{ABC}/{EMPTY}", reference.model_id())
    );
    assert_eq!(helper.dim(), 256);

    let query = "Bram Sample sees the dentist on Fridays";
    assert_eq!(
        helper.embed_query(query).unwrap(),
        reference.embed_query(query).unwrap()
    );
    let passages = [
        "Cafe Lumen has a green door.",
        "Fogtown expects drizzle.",
        "",
    ];
    assert_eq!(
        helper.embed_passages(&passages).unwrap(),
        reference.embed_passages(&passages).unwrap()
    );
}
