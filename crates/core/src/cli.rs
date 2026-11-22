//! Argument parsing shared by the core's binaries.

use std::ffi::OsString;
use std::path::PathBuf;

/// The values of the flags `names` in that order, each given at most once; `None` for any
/// other argument.
pub fn flags<const N: usize>(
    mut args: impl Iterator<Item = OsString>,
    names: [&str; N],
) -> Option<[Option<PathBuf>; N]> {
    let mut values: [Option<PathBuf>; N] = std::array::from_fn(|_| None);
    while let Some(flag) = args.next() {
        let slot = names.iter().position(|name| flag.to_str() == Some(name))?;
        if values[slot].replace(PathBuf::from(args.next()?)).is_some() {
            return None;
        }
    }
    Some(values)
}
