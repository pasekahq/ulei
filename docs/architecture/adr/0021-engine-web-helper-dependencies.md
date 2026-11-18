# ADR-0021: `ureq` and `ring` in `crates/engine` behind the `web` feature for the `ulei-web` helper

Status: accepted
Date: 2026-11-18
Affects: crates/engine, dep:ureq, dep:ring

## Context

ADR-0015 decided the HTTPS client of the web tools: `ureq` 3 with `rustls`, the `ring`
provider and `webpki-roots`, in a `ulei-web` helper that is a second `[[bin]]` of
`crates/engine`, so that no TLS code enters `ulei-engine`. The ADR gate counts only ADRs added
in the change that adds a dependency; this record is that ADR for `crates/engine`.

## Decision

`crates/engine/Cargo.toml` gets two optional dependencies and two features:

| Item | Value |
|---|---|
| `ureq` | `version = "3"`, `default-features = false`, `features = ["rustls"]`, optional; brings `rustls`, `ring` and `webpki-roots` as ADR-0015 section 1 lists them |
| `ring` | `version = "0.17"`, `default-features = false`, optional; the helper computes the SHA-256 of a stored page's file name with it (`_store_full_text` of the reference), from the crate that already provides the TLS crypto |
| Feature `web` | `dep:ureq`, `dep:ring`; the `ulei-web` bin has `required-features = ["web"]`, and the helper's request handling and result shaping (`src/web.rs`, `src/web/`) compile only with it |
| Feature `web-loopback-http` | Test switch: implies `web`; the helper also takes an `http://` Exa base URL whose host is a loopback address literal, and `ureq` is not limited to `https`. Release packaging never enables it. |

Builds:

- `ulei-engine` and the budgets job's engine build use no feature, so neither crate is compiled
  for them. CI checks the engine's normal dependency graph for `ureq`, `rustls`, `ring` and
  `webpki-roots`, and `ci/budgets.sh` checks the engine binary for `rustls` code.
- Release packaging builds `--features web --bin ulei-web`.
- The `clippy`, `test` and `msrv` jobs and `ci/broad_check.py` build `--all-features`, so the
  helper is linted, compiled at the MSRV and tested. The tests of `tests/web.rs` need the test
  switch (`required-features = ["web-loopback-http"]`) and run the helper against a local fake
  Exa on loopback plain http.
- The budgets job builds the helper twice: the shipped `web` build for the size row and the
  `web-loopback-http` build, in its own target directory, for the RSS row of ADR-0015
  section 2, which `ci/web_rss.py` measures against a loopback fake Exa.

No licence is added: `deny.toml` already allows the ISC, BSD-3-Clause and
CDLA-Permissive-2.0 licences of this stack (ADR-0015 section 1). `Cargo.lock` gains no
package, since `crates/channels` already locks the same versions.

## Consequences

- `ulei-engine` stays at its ADR-0010 budgets with no TLS code; `ulei-web` has its own budget
  row in `ci/budgets.sh`.
- A plain `cargo test` skips the helper and its tests; the repository's checks always pass
  `--all-features`.
