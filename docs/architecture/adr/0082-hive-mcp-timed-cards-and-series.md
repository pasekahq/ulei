# ADR-0082: Timed cards and series through the hive in `hive_create`

Status: accepted
Date: 2026-11-26
Amends: ADR-0071 (section 4, the flags `hive_create` leaves out; section 5, the argv).
Affects: crates/hive-mcp, contract:C2, .github/workflows/ci.yml

## Context

ADR-0071 left every timing flag of `hive.py create` out of `hive_create`: `--not-before`,
`--recheck-*`, `--every`, `--cron`, `--tz`, `--until` and `--max-runs` were refused as unknown
fields. On the old runtime the secretary makes its reminders ("напомни завтра в 9") and its
watches ("следи каждые 15 минут до вечера") with exactly these flags, through the hive's CLI. On
ulei the secretary is `ulei-engine`, it reaches the hive only through `ulei-hive-mcp`, so it has
neither: a blocker of the cutover.

ADR-0016 records that ulei has no cron: the engine has no `cronjob` tool, no scheduler, no job
store. That stays so. The hive (`hive-py/`, ADR-0068) already is the owner's scheduler: its
dispatcher holds a card until `not_before`, creates one instance of a recurring card per period
of `every` or of a `cron` expression in a zone, rechecks a done card's acceptance, and sends
every result to the owner inbox, which the engine consumes. Exposing the flags moves no timing
into ulei; the engine only asks the hive for a card with a time on it.

## Decision

### 1. The fields (C2 0.3.0, additive)

`HiveCreateInput` gains nine optional fields, each passed as one `--flag=value` word:

| Field | Flag | Shape |
|---|---|---|
| `not_before` | `--not-before` | RFC 3339 time with an offset |
| `every` | `--every` | `^[1-9][0-9]{0,6}[smhd]$`, at most 366 days |
| `cron` | `--cron` | five fields of `A-Z a-z 0-9 * / , ? # -` joined by single spaces, at most 100 characters |
| `tz` | `--tz` | `^[A-Za-z][A-Za-z0-9_+-]{0,31}(/[A-Za-z0-9_+-]{1,32}){0,2}$` |
| `until` | `--until` | RFC 3339 time with an offset |
| `max_runs` | `--max-runs` | integer 1 to 10000 |
| `recheck_every` | `--recheck-every` | as `every` |
| `recheck_until` | `--recheck-until` | RFC 3339 time with an offset |
| `recheck_max` | `--recheck-max` | integer 1 to 1000 |

An RFC 3339 time here is `YYYY-MM-DDTHH:MM:SS[.f]{,6}(Z|±HH:MM)` with every field in range: a
subset of what the hive's `parse_timestamp` (`datetime.fromisoformat`, offset required) takes,
so whatever the server lets through the CLI parses to the same instant. A time without an offset
is refused, as the hive refuses it. The upper bounds are ours: the hive has none, and a model
must not ask for a million cards.

`HiveCreateOutput` gains the optional `card_class` and `owner_visible` of the CLI's JSON line,
so the model can tell a series (`series_run`, whose own card waits in `blocked`) from a task.

Still refused as unknown fields, unchanged from ADR-0071: `deliver` (every result goes to the
owner inbox, the hive's default, which is how it returns to the secretary), `script`,
`script_timeout`, `executor`, `assignee`, `on_unavailable`, `model`, `effort`, `panel`, `tag`,
`misfire`, `start_window`, `overlap` (the hive's defaults `skip` apply), `body_file`,
`deny_tools`, `type`, `priority`, `gate_timeout`, `blocked_by`, `epic`, `parent`, `optional`,
`followup_of`, `wait`, `created_by`.

### 2. The checks, mirrored from the hive

`ulei-hive-mcp` checks before it runs anything, so the model gets a typed refusal rather than an
argparse usage dump or a traceback; it mirrors the CLI's argparse groups and `api.create`'s
`normalize_series` and `normalize_recheck_policy`:

- `every` and `cron` exclude each other (argparse group); `tz` needs `cron`; `until` and
  `max_runs` need `every` or `cron` ("schedule flags apply only to a recurring card"); `every`
  needs `until` or `max_runs` ("a recurring card requires max_runs or until"), `cron` may run
  unbounded.
- `recheck_until` and `recheck_max` need `recheck_every`; `recheck_every` needs one of them; a
  series takes no recheck.
- A series is always class `series_run` (the hive refuses another class for a recurring card):
  the server passes `--kind=series_run` and refuses an explicit `kind` with `every` or `cron`.
- `not_before` and `hold` exclude each other (one argparse group in the hive); `hold` with
  `every` or `cron` is refused too, as `--hold` is a `not_before` ten years ahead that would
  silently push the series past its `until`.
- `until` and `recheck_until` must lie after `not_before`. Whether a time is in the future,
  whether the zone exists and whether the cron fields mean a schedule (`cronsim`) the hive
  decides; its `hive: card rejected: <text>` comes back as the tool error `card rejected: <text>`.

### 3. `hive_mcp.hold`

With `hive_mcp.hold = true` (ADR-0071 section 7) every engine card is a `--hold` card that waits
for the owner's `hive.py release`. The hive's `--hold` and `--not-before` are one mutually
exclusive group, and a held series would never start on time, so under that setting a create
with `not_before`, `every` or `cron` is refused with a text that names `hive_mcp.hold`; the owner
turns it off to let the secretary schedule. A recheck policy stays allowed: it starts when the
held card is done.

### 4. Delivery and visibility

Nothing about where results go changes: the hive's default destination `owner-inbox` holds for
every card the engine makes. A one-off timed card (`owner_task`) reports its result; the cards
of a series (`series_run`) report only a result marked `[NEWS]` or `[SERIES-STOPPED]` unless the
create sets `owner_visible: true` (the hive's rule). The tool description says both, with the
two worked shapes "remind me tomorrow at 9" (`not_before` with its offset, `acceptance` `true`)
and "check every 15 minutes until evening" (`every` with `until`, or `cron` with `tz`).
`hive_cancel` on a series' own card ends it, as on any card.

### 5. Tests

Unit tests: the argv of each timing field as one word, an option-like `cron` staying a value,
each refusal and each exclusion, RFC 3339 parsing against Python's epoch values, the
`hive_mcp.hold` refusal. `tests/real_hive.rs` creates, on the real CLI, a reminder with
`not_before` (stored instant checked), an `every` series and a `cron` series with `tz` and
`max_runs`, and checks the hive's own refusals (an `until` in the past, an unknown zone) come
back with its text. The cron series needs `cronsim`, the hive's one runtime package, so the CI
`test` job puts a venv of `hive-py/requirements.lock` first on `PATH`; outside CI the cron part
is skipped with a note when the package is missing.

## Consequences

- The secretary on ulei makes reminders and watches as on the old runtime, through the hive.
- ADR-0016's "ulei has no cron" holds: no timer, job store or schedule lives in ulei.
- The hive's timing flags and their parsing are now part of ulei's dependency on its CLI; a
  change to `parse_timestamp`, `parse_duration` or the create checks in a carry must keep
  `crates/hive-mcp/tests/real_hive.rs` green.
