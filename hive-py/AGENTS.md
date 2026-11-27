# AGENTS.md: `hive-py/`

The Python task hive (cards, heads, gate, schedules and series, delivery of results), ported
from the secretary repository at the revision in `SOURCE`. Rules: ADR-0068
(`docs/architecture/adr/0068-python-hive-in-hive-py.md`).

Read the root `AGENTS.md` first. Rules specific to this tree:

- This is the one Python tree outside `ci/`, granted to the hive until the Rust hive of step P6
  replaces it. It is no cargo package and shares no code, build or files with `crates/hive`.
- The hive reaches ulei and qb only over their contracts (C1, C4G) with stdlib clients. `module.toml`
  names the contracts it uses; `cargo xtask arch` checks them against `contracts/index.json`. The one
  out-of-tree import is the deployment's owner-inbox library, loaded only by the `drop` adapter
  in `hive/delivery.py` from the directory config `inbox_library` names. Hive code hands events
  to the delivery interface (`delivery.open_adapter`, `delivery.Event`) and never names an
  adapter.
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
| `hive/` | The package, imported as `hive`: store, API, dispatcher, delivery interface, run liveness probe, heads, runner, the wait nudge (`wait_nudge.py`), reminders (`reminder.py`, ADR-0100), the stall watchdog (`watchdog.py`, ADR-0101), gate, judge and its C4G client, schedules, steward, watch, and the read-only snapshots `inspect.py` (ADR-0086). |
| `scripts/hive.py` | The CLI; `hive_steward.py`, `hive_worktree_rotation.py` and the two store migrations `hive_migrate_*.py`. Each puts this directory on `sys.path` itself. |
| `tests/hive/` | The tests. `fake_inbox.py` stands in for the owner-inbox library and `fake_inbox_library/` is the library directory the tests' config names; `fake_c1_inbox.py` is the owner inbox's C1 daemon on a unix socket the `ulei` adapter's tests use, checking every request against C1 and accepting every C1 kind; `fake_delivery.py` is a delivery adapter that keeps the events it is handed; `fake_ulei_tg.py` is the `ulei-tg notify` the tests' config names, recording each call's argv and stdin and answering a receipt or a scripted failure; `fake_source.py` is an outside source of the watchdog (its journal or escalation command), recording each argv and printing a scripted answer; `journal_watch.py` checks the journal contract (below) on every store connection of a test; `fake_gateway.py` is the scripted qb gateway on a unix socket the judge's and the steward's tests and the tests' config use; `golden_c1_packets.json` pins the packets the real library makes, `source_schema.json` the SOURCE schema, which a fresh store extends only by the reform columns, and `source_config_keys.json` the config keys, all recorded at `SOURCE`. |
| `config.example.toml` | Every config key with a synthetic value. |
| `texts/` | Neutral samples of the texts the config names under `[texts]`. |

## Config

Every hive process reads one TOML config: the path its `--config` names, else `HIVE_CONFIG`; there
is no other default. A deployment value (paths, the owner's time zone and actor name, the
environments of heads, gates and scripts, the delivery adapter and its keys, the texts) has no
default, and a config without it is refused with the key's name. `delivery` names the adapter of
`hive/delivery.py`, and any other value is refused at load. `drop` publishes through the inbox
library and reads `inbox` and `inbox_library`; `ulei` is the C1 client of the owner inbox and reads
`inbox_socket` and `inbox_producer_key_file`, a 32-byte key file open to no group or other user.
Only the selected adapter's keys are required. `[reminders]` is required: `tg_binary` and `tg_config` have no default,
its timings do (Reminders, below). `[watchdog]` is optional and checked at load: every key has a default but its three
outside sources, without which their condition is off (Watchdog, below). `[head_wait_nudge]` (`interval_s`, `text`) has defaults (1200 s, an English text) and is checked at load:
a non-negative interval, 0 for off, and a text naming no placeholder but `{minutes}` and `{command}` (`hive/LIVE_STEER.md`, Wait nudge). `HIVE_DB` overrides `db`. The live config and texts
are the deployment's and are never in the tree; `config.example.toml` lists every key.

## Snapshots (ADR-0086)

`inspect.card`, `inspect.health`, `inspect.find` and `inspect.read_file` are the read-only views behind `hive.py
inspect CARD`, `health`, `list` with a filter and `file CARD PATH`, and the MCP tools `hive_inspect`, `hive_health`,
`hive_list` and `hive_file`. They write nothing, signal nothing and stream nothing: one store read per call, the
unit cgroups of the user manager and `/proc` through `inspect.Probe` (tests hand a fake root), at most three `gh ...
--json` reads of the pull requests and workflow runs the heads' command lines name (`--no-github` skips them), and
the owner inbox through the delivery adapter's `status(event_id)`. Every text is capped (`inspect.REPORT_CHARS`,
`OUTPUT_CHARS`, `STEER_CHARS`, `FILE_CHARS`); `health` shows the config's limits, never an environment, a path, a
command or a text. `read_file` stays in the card's workspace or in the results files the card's own text names: no
`..`, no symlink, no secret-looking name (`inspect.SECRET_NAME`). A new field goes into the JSON under the same
`inspect.VERSION`; a changed or removed one bumps it.

## Board

`api.board(conn, now, archive_limit, alive, cgroup_root)` is the one board projection; `hive.py board [--json]
[--archive [--limit N]]` prints it with the process probe `liveness.alive` under `liveness.CGROUP_ROOT`. It
reads the existing card columns, links, active runs and gate claims and stores nothing: the archive is
computed, not flagged. A card has three places: one column (`in_progress`, `waiting`, `done`), the
archive, or off the board.

It finds its cards through the indexes `cards_board` and `links_followup` (`store.BOARD_INDEXES`) and reads only those
rows by key, never a scan of `cards` (`api.BOARD_CARDS`, `tests/hive/bench_board.py`).

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
| `blocked` | `wait_reason` `disputed` (the worker or the judge disputes the task itself; its author decides with `resolve`) | `waiting` | `external` | `disputed` | `reason`: `block_reason`, `disputed:` and who disputes what |
| `blocked` | `wait_reason` `dependency` | `waiting` | `card` | `dependency` | `cards`: blockers not done or cancelled; `reason`: `block_reason` |
| `blocked` | `wait_reason` `children` (an epic) | `waiting` | `card` | `children` | `cards`: required children not done or cancelled; `reason`: `block_reason` |
| `blocked` | `wait_reason` `reminder` (a due reminder) | `waiting` | `external` | `reminder` | `reason`: the reminder's state, `due` or `claimed` |

Any other combination, such as `blocked` with `wait_reason` `schedule` on a card that is not `recurring`,
raises `ValueError` naming the card. Every entry carries the card's user `tag` (null without one). A probe that finds no unit cgroup counts the run as gone; a missing user
manager cgroup raises, as in the reaper. A finish time is `finished_at`, or `updated_at` for a row finished
without it.

Every `waiting` entry also carries `waiting_since`: when the card entered the state it waits in (a `blocked` card:
its `wait_reason`), the `at` of its latest event of the kinds below (`api.WAITING_SINCE_EVENTS`), else its
`updated_at`, clamped to `[created_at, now]`. Only these cards read their events, one indexed query each
(`events_card`). A `running`, `review` or `check` card waits when it is not in progress, and its kinds start or end
its claim, run or gate job, so the latest marks when it last stopped being in progress. Notes written while a card
stays where it is (`steer`, `tagged`, `assigned`, `executor_wait`, `panel_wait`, `budget_wait`, `launch_error`, ...)
do not count.

| State | Events |
|---|---|
| `created` | `created`, `dependency_cleared` |
| `queued` | `queued`, `retry`, `deferred`, `assignee_provider_wait`, `provider_outage_wait`, `provider_failover`, `external_unsent`, `steer_handoff`, `steer_round`, `process_ended`, `dispute_resolved` |
| `running` | `claimed`, `run_reaped`, `run_finished`, `panel_report`, `retry` |
| `review` | `review_requested`, `claimed`, `run_reaped`, `run_finished` |
| `check` | `run_finished`, `panel_synthesized`, `children_done`, `gate`, `judge`, and `process_wait` (a card parked on its head's processes after an exit 0 goes on to `check` without an event) |
| `blocked`, `wait_reason` empty, `stuck`, `judge`, `tag`, `disputed` | `blocked` |
| `blocked`, `process` | `process_wait` |
| `blocked`, `dependency` | `dependency_wait` |
| `blocked`, `children` | `children_wait` |
| `blocked`, `reminder` | `reminder_due` |

`in_progress` is in queue order (priority, then age); `waiting` is longest waiting first (`waiting_since`, then
`created_at`, then id); `done` and the archive are newest finish first, the archive cut to `archive_limit`. The
text board prints each waiting entry's time since `waiting_since` (`for 3h`).

## Reminders

A card created with `--reminder TEXT` (`api.create(reminder=...)`) is a reminder (ADR-0100): the text, verbatim, not
blank and at most 32 KiB, is in `cards.reminder`. It needs `--not-before`, `--hold` or a schedule and runs no head, so
it takes no script, executor, model, effort, panel, assignee, review, judge acceptance, recheck, dependency, epic or
parent; its acceptance is optional. Each instance of a recurring reminder copies the text and is a reminder.

At `not_before` the dispatcher writes, in one transaction, the reminder's row in table `reminders` (state `due`), the
card's move to `blocked` with `wait_reason` `reminder`, the event `reminder_due` and one outbox event of kind
`reminder_due` (`hive:<card>:reminder_due`) whose payload carries `text`; it starts no head. Then:

- `hive.py reminder claim CARD` makes a `due` reminder `claimed`; `reminder send CARD [--text TEXT]` sends a claimed one
  through `ulei-tg notify` (the original without `--text`) and prints the receipt; `reminder withdraw CARD --reason
  TEXT` makes a claimed one `withdrawn` and cancels its card, sending nothing. The actor (`--by`) is `secretary` or
  `owner_actor`. Each verb holds the dispatcher's lock file; a refusal (not a reminder, not due, wrong state, card no
  longer waiting) is one stderr line and exit 2, a failed send exit 1.
- The fallback: every tick, a `due` reminder `[reminders] unclaimed_s` (180) after it fell due, or a `claimed` one
  `claim_silence_s` (300) after its claim, is sent with its original text, `[tg_binary, "notify", "--config",
  tg_config]` with the text on stdin; once delivered the secretary gets a `reminder_fallback` event naming the text,
  why, and the receipt.
- Delivered means a receipt with a message id: the row keeps who sent what and the receipt, the card is `done` with the
  text as its result, and no `task_result` is written. A failed send (any non-zero exit, a timeout after
  `notify_timeout_s`, exit 0 without a receipt) keeps the state, counts an attempt and is retried by the fallback after
  `retry_s`; the `max_attempts`-th blocks the card as a failure (state `failed`, a `task_blocked` event).
- A send that reached Telegram but was not recorded (a crash in between) is sent again: a reminder may come twice,
  never not at all. A cancelled reminder card is never sent.

`hive.py show CARD` prints the reminder's state and text; `api.get` returns its row as `reminder_state`.

The secretary reaches the verbs through `ulei-hive-mcp`'s `hive_reminder` and creates a reminder with `hive_create`'s
`reminder` (ADR-0101). The kinds `reminder_due`, `reminder_fallback` and `watchdog_alert` need the `ulei` delivery
adapter: the `drop` adapter's older inbox library refuses them, and then only the fallback reaches the owner.

## Watchdog

`hive.py watchdog` (`watchdog.Watchdog.run`, ADR-0101) is one run of the stall watchdog, a oneshot outside the
dispatcher that the user timer `ulei-hive-watchdog.timer` starts every 5 minutes. It calls no model. A run:

1. sends the overdue alerts: the reminders' fallback (Reminders, above) over the open reminders the actor `watchdog`
   created, as `watchdog`, under the dispatcher's lock file tried every second for `lock_wait_s` (150) and without it
   past that (a dispatcher stuck in a tick);
2. checks each condition and makes one alert per de-duplication key that holds and is past its condition's
   `cooldown_s`: `queue_stalled` (cards that may launch waited `stall_s`, 900, while the dispatcher has not ticked or
   no head started for as long), `vendor_cooldown:<vendor>`, `qb_limit:<provider>` (`qb_usage_pct`, 90, of the weekly
   limit by `heads.weekly_left`, of `qb_providers` or all), `series_superseded:<series>` (the last `superseded_runs`,
   3, ended instances cancelled as superseded), `escalation_unread:<id>` (`escalations_command`'s JSON list of
   `{"id", "created_at", "read"}`, unread `escalation_age_s`, 1800), `bridge_token_refused` (a line of
   `journal_command --unit=<bridge_unit> --since=@T --output=json --no-pager --quiet` within `bridge_window_s`
   matching `bridge_pattern`) and `wait_nudge_repeat:<card>:<pid>` (`nudge_repeats`, 3, `wait_nudge` events in a row
   on one pid of a running card; once per card and pid);
3. publishes its own unpublished outbox rows through the delivery adapter.

An alert is a reminder card of the actor `watchdog` (class `system`, title `watchdog: <condition>`, idempotency key
`watchdog:<key>:<n>`), due at once with the outbox event `hive:<card>:watchdog_alert` whose payload carries the text,
`condition`, `key` and `facts`; the meta row `watchdog:<key>` keeps the last alert of the key. The text is a fixed
template of the facts, never model output. The secretary claims, sends and withdraws it as a reminder; the dispatcher's
tick sends it as an overdue reminder too. The summary `{sent, lock, fired, published, errors}` is one JSON line on
stdout; a source that fails is one stderr line each, the other conditions still run, and the exit is 1.

## Disputes

A dispute is about the task itself (its premise, an artifact it requires, its acceptance), not about the work, and
it is never a failure. The worker raises one with a report whose first line has the status `disputed`
(`RESULT: disputed ...`, `dispatcher.DISPUTED_RE`); the judge with `VERDICT: disputed` and its reason. A round whose
worker disputes is not reworked: a failed acceptance goes to the author without a judge turn, and a judge `rework`
of it counts as `disputed` (a judge `done` still finishes the card: the result counts, not the artifacts).
`Dispatcher.dispute` writes the event `disputed` (round, by, reason) and blocks the card with `wait_reason`
`disputed`; the secretary gets one `task_blocked` event `hive:<card>:disputed:<round>` with `disputed`
(round, by, reason, author) and a note (`dispatcher.DISPUTE_NOTE`), always through the inbox. Only the card's
author, the secretary or the owner actor can run `resolve`: `hive.py resolve CARD accept [TEXT]` finishes it `done`,
`hive.py resolve CARD rework TEXT` queues the next round with TEXT as its feedback and spends no failure (event
`dispute_resolved`); `cancel` drops it. `release` refuses a disputed card.

Who decides a dispute: the card's author, usually the secretary. The worker and the judge are the two sides and
never decide it; neither has the context in which the card was written. The author reads the evidence in
`disputed` and picks one of three: `resolve accept` (the worker's result stands), `resolve rework TEXT` (a new
round with corrected instructions) or `cancel` (the card was not worth doing). The author asks the owner only
when the decision needs knowledge that only the owner has, and then in one sentence; every other dispute is
settled without the owner.

## Journal contract

Readers outside the hive follow `hive.db` incrementally. A dashboard importer, for example, opens it read-only every
minute and reads only rows past its last `rowid`/`id` marks, the runs that are open or not yet handled, and the cards
it last saw outside `done`/`cancelled`. That is enough only while hive code keeps these rules:

- `events` are append-only: never updated or deleted. Cards are never deleted. A gate row is never updated in
  place; a retry replaces it with `INSERT OR REPLACE` and so gets a new id.
- Any change to a `done` or `cancelled` card writes an event on that card in the same transaction. So does any
  change or removal of a link, on its `card_id`, and any change to a run that has ended and been handled.
- A new card, link, run, gate or event needs no event of its own.

`tests/hive/journal_watch.py` checks this on every store connection of every test. It adds TEMP triggers to the
connection and wraps `store.tx`; writes made by test code that arranges a board are skipped. A change that breaks
a rule fails the test that made it. Either journal the change, or name the new exception here and in
`journal_watch.py`, and tell the readers.

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
