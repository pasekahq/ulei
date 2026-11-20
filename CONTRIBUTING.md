# Contributing to ulei

Thanks for helping. The organization-wide guide is
[pasekahq/.github CONTRIBUTING](https://github.com/pasekahq/.github/blob/main/CONTRIBUTING.md);
this file adds what is specific to `ulei`. Coding agents (and humans) also follow
[AGENTS.md](AGENTS.md): module boundaries, ADR-first for new crates, dependencies, pins and
contracts, budgets.

## Before you start
- Bugs and feature ideas: open an issue first. New crates, dependencies, pins or contract changes
  need an ADR in `docs/architecture/adr/` before code (`cargo xtask adr` enforces it).
- Security issues: never in public issues, see the
  [security policy](https://github.com/pasekahq/.github/blob/main/SECURITY.md).

## Checks
```sh
python3 -m ci.broad_check     # fmt, clippy -D warnings, tests, cargo xtask check
cargo deny check advisories bans licenses sources
```
CI also runs gitleaks, MSRV, budgets, contract and kill tests. `main` is protected: every change
lands through a pull request with green CI.

## Sign-off (DCO), no CLA
Every commit must be signed off under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):

```
Signed-off-by: Your Name <you@example.com>
```

Use `git commit -s` (or `git rebase --signoff main` for commits you already made). The sign-off
must match the commit author. The `dco` check enforces it on pull requests.

## License
`ulei` is licensed under [AGPL-3.0-only](LICENSE). By signing off you certify that you have the
right to submit your contribution under that license, and you license it under the same terms.
