// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Paths the file tools refuse to read.

use std::path::Path;

/// Basenames of files holding secrets only, lowercase.
pub(super) const ENV_FILE_BASENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.staging",
    ".envrc",
];

/// The refusal for reading `resolved`, a path with its symlinks resolved, when its name is a
/// secret-bearing environment file.
pub(super) fn read_block_error(resolved: &Path) -> Option<String> {
    let name = resolved.file_name()?.to_string_lossy().to_lowercase();
    ENV_FILE_BASENAMES.contains(&name.as_str()).then(|| {
        format!(
            "Access denied: {} is a secret-bearing environment file and cannot be read to \
             prevent credential leakage. If you need to check the file structure, read \
             .env.example instead. (Defense-in-depth \u{2014} not a security boundary; the \
             terminal tool can still bypass.)",
            resolved.display()
        )
    })
}
