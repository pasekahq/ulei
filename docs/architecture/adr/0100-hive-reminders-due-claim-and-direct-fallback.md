# ADR-0100: Hive reminders as `reminder_due` events with a claim and a direct `ulei-tg notify` fallback (C1 1.5.0)

Status: accepted
Date: 2026-11-27
Affects: hive-py, contract:C1

## Context

A reminder today is a hive card with `not_before` (ADR-0082). At that time the dispatcher launches a
head, the head writes a report, the report reaches the owner inbox as `task_result`, and an engine
turn of the secretary tells the owner. The owner hears nothing when any of the three fails: no head
slot, a failed head, an inbox event that ends `uncertain`, an engine turn that fails. A reminder
needs no model at all: its text is known when it is created. ADR-0097 gives a path to the owner
that needs only the Bot API, `ulei-tg notify`, and names the hive as one of its callers.

## Decision

### 1. Which cards are reminders

A card created with a reminder text is a reminder: `hive.py create --reminder TEXT`, `api.create(...,
reminder=TEXT)`. The text is stored verbatim in the new nullable column `cards.reminder`. It must not
be blank and is at most 32 KiB of UTF-8 (ADR-0097's `notify::MAX_TEXT_BYTES`). A reminder needs
`--not-before` (or `--hold`) or a schedule (`--every`, `--cron`). It runs no head, so it takes no
script, executor, model, effort, panel, assignee, review, judge acceptance, recheck policy,
dependency, epic or parent; its acceptance is optional and defaults to `true`, which nothing runs.

A recurring reminder is a recurring card (class `series_run`) with the text. Each instance the
dispatcher creates for a period copies `reminder` with the other card fields, so every instance is a
reminder; the recurring card itself is the rule and is never due.

### 2. States and transitions

| State | Card | Entered by | Leaves to |
|---|---|---|---|
| (none) | `created`, `not_before` ahead | `create` | `due` |
| `due` | `blocked`, `wait_reason` `reminder` | the dispatcher at `not_before` | `claimed`, `delivered`, `failed` |
| `claimed` | the same | `reminder claim` | `delivered`, `withdrawn`, `failed` |
| `delivered` | `done`, `result` the text sent | a send with a receipt | |
| `withdrawn` | `cancelled`, `result` the reason | `reminder withdraw` | |
| `failed` | `blocked` (a failure) | the `max_attempts`-th failed send | |

- At `not_before` the dispatcher writes, in one transaction, the reminder's row in state `due`, the
  card's move to `blocked` with `wait_reason` `reminder`, the event `reminder_due` on the card and
  one outbox row `hive:<card>:reminder_due` of kind `reminder_due`. It starts no head. A card with
  `not_before` and no reminder text is queued for a head as before.
- A failed send (section 6) keeps the state, adds one to `attempts` and records the error and
  `retry_at`; the `max_attempts`-th moves the reminder to `failed` and blocks the card through the
  dispatcher's `finish`, which sends the secretary its usual `task_blocked` event.
- `delivered` is written only with a receipt that names at least one chat and one message id: the
  row keeps who sent it (`sent_by`), the text sent, the receipt as JSON and the time; the event
  `reminder_delivered` carries the receipt. `withdrawn` keeps the reason and sends nothing. Neither
  writes a `task_result`: the secretary acted itself, and a fallback send has its own event.
- `hive.py cancel` of a due or claimed reminder cancels the card as any other card; the hive never
  sends a reminder whose card is not `blocked` with `wait_reason` `reminder`.
- On the board a due or claimed reminder waits with badge `external`, cause `reminder` and the state
  as its reason, waiting since its `reminder_due` event.

### 3. Where they persist

The hive store gains the column `cards.reminder TEXT` (added by `ALTER TABLE ... ADD COLUMN` on
connect) and the table `reminders`, one row per reminder that fell due, keyed by its card, with
`state`, `due_at`, `claimed_at`, `claimed_by`, `attempts`, `retry_at`, `error`, `sent_by`,
`sent_text`, `receipt`, `sent_at`, `reason` and `updated_at`, and the partial index `reminders_open`
on the open states that the dispatcher reads every tick. Both are created with `IF NOT EXISTS`, so
the migration is additive: an existing store gets them on its next connection and no row changes. A
reminder's every state change writes an event on its card in the same transaction (the journal
contract of `hive-py/AGENTS.md`).

### 4. The verbs

`hive.py reminder claim|send|withdraw CARD [--by ACTOR]`, the actor `secretary` (the default) or
the config's `owner_actor`; any other actor is refused. Each runs under the dispatcher's lock file,
as `cancel` does, so a verb and a tick never act on one reminder at once.

- `claim`: a `due` reminder becomes `claimed` by the actor; this stops the unclaimed fallback.
- `send [--text TEXT]`: a `claimed` reminder is sent through `ulei-tg notify` (section 6), the
  original text without `--text`, else TEXT under the rule of section 1. Exit 0 prints the receipt;
  a failed send is a failed attempt, one stderr line and exit 1, and the reminder stays `claimed`.
- `withdraw --reason TEXT`: a `claimed` reminder becomes `withdrawn` with the non-blank reason.

A card that is not a reminder, a reminder not yet due or in another state, or a card that is no
longer waiting as a reminder is refused with one stderr line and exit 2, and nothing changes. The
dispatcher's fallback is the only other sender. The secretary's tools for these verbs come with
`ulei-hive-mcp`.

### 5. Timeouts and the retry rule

The table `[reminders]` of the hive config; it is required, since its two paths are deployment
values without a default:

| Key | Default | Meaning |
|---|---|---|
| `tg_binary` | none | the `ulei-tg` binary |
| `tg_config` | none | the path passed as `ulei-tg notify --config` |
| `unclaimed_s` | 180 | a `due` reminder not claimed this long after `due_at` is sent by the fallback |
| `claim_silence_s` | 300 | a `claimed` reminder neither sent nor withdrawn this long after `claimed_at` is sent by the fallback |
| `retry_s` | 60 | after a failed send, the fallback tries again no earlier than this |
| `max_attempts` | 20 | the failed sends after which the reminder is `failed` |
| `notify_timeout_s` | 120 | one `ulei-tg notify` run; past it the run is killed and counts as failed |

`claim_silence_s` is 300 s: one engine turn of the secretary that claims and then sends fits in it,
and a claim whose turn died still reaches the owner within `unclaimed_s` plus 5 minutes of
`not_before`. Every tick, the dispatcher sends each open reminder past its deadline and past its
`retry_at`. Every send failure (exit 1, 2 or 3, a timeout, a missing binary, an exit 0 without a
receipt) is retried this way, whoever made it, until `max_attempts`; the secretary may retry a
claimed reminder's `send` at once.

### 6. How the hive calls `ulei-tg notify`

`[tg_binary, "notify", "--config", tg_config]` with the text on stdin, never on argv, and the
environment of the calling process; exit 0 with the receipt line on stdout is a send. The fallback
sends the original text, never an edit.

### 7. The crash window, and which side it errs on

A send runs outside a store transaction: the hive reads the reminder, runs `ulei-tg notify`, then
records the receipt. A crash or kill between Telegram's answer and that record leaves the reminder
open, and the next tick sends it again: the owner may get the reminder twice, and never loses it.
The same holds for exit 1 and a timeout, after which messages may have gone out (ADR-0097 section 4).
The hive errs on the side of a duplicate.

### 8. The C1 kinds (C1 1.5.0, additive)

`Kind` gains `reminder_due` and `reminder_fallback`. Both payloads are JSON objects with `source`
`hive`, `card_id`, `title`, `next_actor` `secretary`, `how_to_read` and a fixed hive `note`; the
reminder text is the stored text verbatim, and no field holds model output.

- `reminder_due`, id `hive:<card>:reminder_due`: `text`, `due_at`, `series` and `period` (null for a
  one-off reminder), `unclaimed_s`, `claim_silence_s`. The note names the three verbs and the
  fallback.
- `reminder_fallback`, id `hive:<card>:reminder_fallback`, a context event after the fallback
  delivered: `text` (exactly what was sent), `why` (`unclaimed` or `claim_silent`), `due_at`,
  `claimed_at` (null when unclaimed), `sent_at`, `receipt`. The note says the owner already has it
  and it must not be sent again.

The hive's publish sends both to the owner inbox whatever the card's `deliver`. An inbox that
refuses the kinds (an inbox library older than C1 1.5.0 under the `drop` adapter) keeps the outbox
row with its error; the fallback still delivers the reminder after `unclaimed_s`.

## Consequences

- A reminder reaches the owner when the hive and Telegram work, whether or not a head, the inbox or
  the engine does; the secretary still words it when it is alive.
- The hive now depends on the `ulei-tg` binary and the `ulei.toml` it reads; a config without
  `[reminders]` is refused at load.
- Reminder cards created before this change ran heads and keep doing so; they are not converted.
- Until `ulei-hive-mcp` exposes the verbs and the secretary's prompt names the kinds, a `reminder_due`
  reaches the secretary with the default policy and the fallback delivers the text after 180 s.
