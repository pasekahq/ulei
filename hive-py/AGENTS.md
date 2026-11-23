# AGENTS.md: `hive-py/`

The Python task hive (cards, heads, gate, schedules and series, delivery of results), ported
from the secretary repository at the revision in `SOURCE`. Rules: ADR-0068
(`docs/architecture/adr/0068-python-hive-in-hive-py.md`).

Read the root `AGENTS.md` first. Rules specific to this tree:

- This is the one Python tree outside `ci/`, granted to the hive until the Rust hive of step P6
  replaces it. It is no cargo package and shares no code, build or files with `crates/hive`.
- The hive reaches ulei and qb only over their contracts (C1, C4G). `module.toml` names the
  contracts it uses; `cargo xtask arch` checks them against `contracts/index.json`.
- Packages: `requirements.lock` (runtime, the only lock the live host installs) and
  `requirements-test.lock` (tests only), each pin with its sha256 hash and its license id. A new
  package needs an ADR naming `hive-py` and `dep:py:<name>`, and its license must be in the
  allow list of `deny.toml`.
- No personal data (ADR-0068 section 4): no home paths, host names, user, chat or card ids, unit
  names or dates in code, comments or tests; tests use synthetic values under their temporary
  directory. `cargo xtask arch` scans `**/*.py` and `**/*.toml` here for personal paths, the old
  runtime's name, model-vendor hosts and credential paths.
- Until the cutover, changes to the moved files arrive as carries from the secretary repository
  with `SOURCE` advanced in the same commit (ADR-0068 section 6).
- `tests/hive/fake_inbox.py` stands in for the owner-inbox library; `tests/hive/golden_c1_packets.json`
  pins the packets the real library makes, recorded at `SOURCE`.

## Local check

CPython 3.12, from the repository root, the same installs and pytest call as the CI job `hive-py`:

```sh
venv="${TMPDIR:-/tmp}/hive-py-venv"
python3.12 -m venv "$venv"
"$venv/bin/pip" install --require-hashes --no-deps -r hive-py/requirements.lock
"$venv/bin/pip" install --require-hashes --no-deps -r hive-py/requirements-test.lock
"$venv/bin/python" -m pytest hive-py/tests -q -p no:cacheprovider
```

`python3 -m ci.broad_check` does not run this suite.
