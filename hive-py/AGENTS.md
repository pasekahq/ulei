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
| `hive/` | The package, imported as `hive`: store, API, dispatcher, run liveness probe, heads, runner, gate, judge and its C4G client, schedules, steward, watch. |
| `scripts/hive.py` | The CLI; `hive_steward.py`, `hive_worktree_rotation.py` and the two store migrations `hive_migrate_*.py`. Each puts this directory on `sys.path` itself. |
| `tests/hive/` | The tests. `fake_inbox.py` stands in for the owner-inbox library and `fake_inbox_library/` is the library directory the tests' config names; `fake_gateway.py` is the scripted qb gateway on a unix socket the judge's and the steward's tests and the tests' config use; `golden_c1_packets.json` pins the packets the real library makes, `source_schema.json` the SOURCE schema, which a fresh store extends only by the reform columns, and `source_config_keys.json` the config keys, all recorded at `SOURCE`. |
| `config.example.toml` | Every config key with a synthetic value. |
| `texts/` | Neutral samples of the texts the config names under `[texts]`. |

## Config

Every hive process reads one TOML config: the path its `--config` names, else `HIVE_CONFIG`; there
is no other default. A deployment value (paths, the owner's time zone and actor name, the
environments of heads, gates and scripts, the inbox library, the texts) has no default, and a config
without it is refused with the key's name. `HIVE_DB` overrides `db`. The live config and texts are
the deployment's and are never in the tree; `config.example.toml` lists every key.

## Board

`api.board(conn, now, archive_limit, alive, cgroup_root)` is the one board projection; `hive.py board [--json]
[--archive [--limit N]]` prints it with the process probe `liveness.alive` under `liveness.CGROUP_ROOT`. It
reads the existing card columns, links, active runs and gate claims and stores nothing: the archive is
computed, not flagged. A card has three places: one column (`in_progress`, `waiting`, `done`), the
archive, or off the board.

Off the board, in no column and not in the archive: a `recurring` card (the rule of a schedule or series)
and a `series_run` card without a `followup` link (a run of a schedule or series; `show` of its rule lists
it under `instances`). A card created with `--followup-of` has that link and is placed like any other card,
whatever class it inherited.

Every other card is placed by the first matching row:

| State | Also | Place | Badge | `cause` | Reason fields |
|---|---|---|---|---|---|
| `running`, `review` | a `starting` or `running` run with `vendor` the card's `executor` (an external executor job, not probed) | `waiting` | `external` | `executor` | `reason`: the executor |
| `running`, `review` | a `starting` or `running` run whose process the probe finds alive | `in_progress` | | | |
| `running`, `review` | otherwise, `retry_at` after `now` (backoff) | `waiting` | `external` | `retry` | `until`: `retry_at` |
| `running`, `review` | otherwise (waiting for a worker, panel member or reviewer slot) | `waiting` | `slot` | `queue` | |
| `check` | a `gate_jobs` row (its gate or judge runs) | `in_progress` | | | |
| `check` | otherwise, `retry_at` after `now` (a judge turn failed with a retryable class) | `waiting` | `external` | `retry` | `until`: `retry_at`; `reason`: `tag <tag>` for a tagged card |
| `check` | otherwise (waiting for gate room) | `waiting` | `slot` | `queue` | |
| `done` | finished at or after `now - DONE_WINDOW_S` (2 days) | `done` | | | |
| `done` | finished earlier | archive | | | |
| `cancelled` | | archive | | | |
| `created` | `not_before` after `now` (a hold) | `waiting` | `external` | `hold` | `until`: `not_before` |
| `created` | no hold | `waiting` | `slot` | `queue` | |
| `queued` | `executor_wait` set | `waiting` | `external` | `executor` | `reason`: the executor and its wait reason |
| `queued` | `retry_at` after `now` (backoff or script deferral) | `waiting` | `external` | `retry` | `until`: `retry_at` |
| `queued` | otherwise | `waiting` | `slot` | `queue` | |
| `blocked` | `wait_reason` empty (failed) | `waiting` | `external` | `failure` | `reason`: `block_reason` |
| `blocked` | `wait_reason` `stuck` (rounds in a row ended without a result) | `waiting` | `external` | `stuck` | `reason`: `block_reason` |
| `blocked` | `wait_reason` `process` (the worker exited leaving processes in its unit) | `waiting` | `external` | `process` | `reason`: `block_reason` |
| `blocked` | `wait_reason` `judge` (a judge turn failed with a non-retryable class or answered no verdict) | `waiting` | `external` | `judge` | `reason`: `block_reason`, naming the class and, for a tagged card's failed turn, the tag |
| `blocked` | `wait_reason` `tag` (the launcher refused a tagged head, or no tag-capable vendor may run it) | `waiting` | `external` | `tag` | `reason`: `block_reason`, naming the tag and the refusal class or the vendors |
| `blocked` | `wait_reason` `dependency` | `waiting` | `card` | `dependency` | `cards`: blockers not done or cancelled; `reason`: `block_reason` |
| `blocked` | `wait_reason` `children` (an epic) | `waiting` | `card` | `children` | `cards`: required children not done or cancelled; `reason`: `block_reason` |

Any other combination, such as `blocked` with `wait_reason` `schedule` on a card that is not `recurring`,
raises `ValueError` naming the card. Every entry carries the card's user `tag` (null without one). A probe that finds no unit cgroup counts the run as gone; a missing user
manager cgroup raises, as in the reaper. A finish time is `finished_at`, or `updated_at` for a row finished
without it. `in_progress` and `waiting` are in queue order (priority, then age); `done` and the archive are
newest finish first, the archive cut to `archive_limit`.

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
