# AGENTS.md: `hive-py/`

The Python task hive (cards, heads, gate, schedules and series, delivery of results), ported
from the secretary repository at the revision in `SOURCE`. Rules: ADR-0068
(`docs/architecture/adr/0068-python-hive-in-hive-py.md`).

Read the root `AGENTS.md` first. Rules specific to this tree:

- This is the one Python tree outside `ci/`, granted to the hive until the Rust hive of step P6
  replaces it. It is no cargo package and shares no code, build or files with `crates/hive`.
- The hive reaches ulei and qb only over their contracts (C1, C4G). `module.toml` names the
  contracts it uses; `cargo xtask arch` checks them against `contracts/index.json`. The one
  out-of-tree import is the deployment's owner-inbox library, loaded only by
  `hive/inbox_library.py` from the directory config `inbox_library` names.
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

## Layout

| Path | What |
|---|---|
| `hive/` | The package, imported as `hive`: store, API, dispatcher, heads, runner, gate, schedules, steward, watch. |
| `scripts/hive.py` | The CLI; `hive_steward.py`, `hive_worktree_rotation.py` and the two store migrations `hive_migrate_*.py`. Each puts this directory on `sys.path` itself. |
| `tests/hive/` | The tests. `fake_inbox.py` stands in for the owner-inbox library and `fake_inbox_library/` is the library directory the tests' config names; `golden_c1_packets.json` pins the packets the real library makes, `source_schema.json` the schema a fresh store creates and `source_config_keys.json` the config keys, all recorded at `SOURCE`. |
| `config.example.toml` | Every config key with a synthetic value. |
| `texts/` | Neutral samples of the texts the config names under `[texts]`. |

## Config

Every hive process reads one TOML config: the path its `--config` names, else `HIVE_CONFIG`; there
is no other default. A deployment value (paths, the owner's time zone and actor name, the
environments of heads, gates and scripts, the inbox library, the texts) has no default, and a config
without it is refused with the key's name. `HIVE_DB` overrides `db`. The live config and texts are
the deployment's and are never in the tree; `config.example.toml` lists every key.

## Local check

CPython 3.12, from the repository root, the same installs and pytest call as the CI job `hive-py`:

```sh
venv="${TMPDIR:-/tmp}/hive-py-venv"
python3.12 -m venv "$venv"
"$venv/bin/pip" install --require-hashes --no-deps -r hive-py/requirements.lock
"$venv/bin/pip" install --require-hashes --no-deps -r hive-py/requirements-test.lock
"$venv/bin/python" -m pytest hive-py/tests -q -p no:cacheprovider
```

The tests write their own config under their temporary directory; they need no `HIVE_CONFIG`.
`python3 -m ci.broad_check` does not run this suite.
