//! The embedder helper `ulei-garden-embed`, a binary of its own package (ADR-0104), which a test
//! run of the workspace builds beside this package's `ulei-garden`.

use std::path::PathBuf;
use std::sync::LazyLock;

static EMBED: LazyLock<String> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_ulei-garden")).with_file_name("ulei-garden-embed");
    assert!(
        path.is_file(),
        "{} is not built: run the workspace's tests, or first `cargo build -p ulei-garden-embed` \
         with this run's profile, target and features",
        path.display()
    );
    path.to_string_lossy().into_owned()
});

/// The helper's path; panics when it is not built.
pub(crate) fn embed() -> &'static str {
    &EMBED
}
