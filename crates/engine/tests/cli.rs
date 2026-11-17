//! The built `ulei-engine` binary: `--version` succeeds, anything else is a usage error.

use std::process::Command;

#[test]
fn version_succeeds_and_other_invocations_print_usage() {
    let bin = env!("CARGO_BIN_EXE_ulei-engine");

    let out = Command::new(bin).arg("--version").output().expect("run");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("ulei-engine {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");

    for args in [&[][..], &["--help"], &["run"], &["--version", "extra"]] {
        let out = Command::new(bin).args(args).output().expect("run");
        assert!(!out.status.success(), "{args:?} must fail");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "{args:?} wrote stdout"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.starts_with("usage: ulei-engine") && err.lines().count() == 1,
            "{args:?}: {err:?}"
        );
    }
}
