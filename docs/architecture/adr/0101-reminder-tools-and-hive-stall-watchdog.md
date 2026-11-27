# ADR-0101: The secretary's reminder tools and policies, and the hive's stall watchdog with a direct fallback (C1 1.6.0, C2 0.6.0)

Status: accepted
Date: 2026-11-27
Affects: hive-py, crates/hive-mcp, crates/core, contract:C1, contract:C2
Amends: ADR-0071 section 5 (the tool table), ADR-0100 (consequences)

## Context

ADR-0100 made a reminder a `reminder_due` event with the verbs `hive.py reminder claim|send|withdraw`
and a direct fallback through `ulei-tg notify`, but the secretary had neither tools for the verbs
nor a policy for the two kinds: a `reminder_due` met the task policy, and the fallback sent every
reminder after 180 s.

The hive can also stall without a word to the owner: the dispatcher stops ticking or hangs, a
vendor is in its provider cooldown, qb's weekly limit runs out, a series supersedes every run
before it starts, a head waits on a command that hung, an escalation of the outside pipeline goes
unread, the MCP bridge can no longer refresh its token. Each is visible in the store, in qb or in a
journal, and none reaches the owner by itself.

## Decision

### 1. The secretary's tools (C2 0.6.0, additive)

- `hive_create` takes `reminder`: the text the owner gets, verbatim (no trimming), not blank, at
  most 32768 bytes of UTF-8 (the hive's `REMINDER_MAX_BYTES`) and no NUL, which no argv word holds.
  It goes as `--reminder=<text>`. The server refuses it with `accept_review`, `review` or a recheck
  (a reminder runs no head; `acceptance` stays required by C2's `oneOf`, `true`) and without
  `not_before`, `every`, `cron` or `hold`.
- `hive_reminder` (`HiveReminderInput`: `card_id`, `action` one of `claim`, `send`, `withdraw`, the
  optional `text` and `reason`) answered by `Ack` with the CLI's line:

| Action | argv after `hive_command` | Fields |
|---|---|---|
| `claim` | `reminder claim --by=<actor> -- <id>` | no `text`, no `reason` |
| `send` | `reminder send --by=<actor> [--text=<text>] -- <id>` | `text` optional, under the rule of `reminder`; no `reason` |
| `withdraw` | `reminder withdraw --by=<actor> --reason=<reason> -- <id>` | `reason` required, 1 to 1000 characters, not blank; no `text` |

  The actor is the server's, never an input (ADR-0071 section 7). Refusals are typed by their
  first words: a field rule of the table is refused before anything runs; the hive's refusal
  (exit 2: not a reminder, not due yet, in another state, no longer waiting, another actor) is the
  tool error `reminder <action> refused: <why>`, and a failed send (exit 1) `reminder send failed:
  <why>`, after which the reminder stays claimed and the fallback retries it. `send` answers the
  CLI's JSON line with the Telegram receipt.
- `hive_mcp.timeout` should exceed `[reminders] notify_timeout_s` of the hive: a send the server
  kills is recorded by nobody, stays claimed and goes out again by the fallback, possibly twice
  (ADR-0100 section 7).

### 2. The secretary's policies (`crates/core/src/prompt.rs`)

The reference builds the model text with two policies, weekly and task; it has no reminder kinds,
so these get policies of their own after the same pattern (header, opening, policy, the reference's
`RULES`, the event tail):

- `reminder_due` and `watchdog_alert`: `text` is what the owner is to get; decide with the
  `hive_reminder` tool (claim, then send as is, send edited, or withdraw with a reason); the tool's
  send is the only send, so the turn's own reply never posts the text, a paraphrase or a
  confirmation and no other tool sends it; after the tool answers, also on a refusal or a failed
  send, the turn returns `NO_REPLY_DONE`; doing nothing lets the hive send the original itself.
- `reminder_fallback`: the hive already delivered the text directly; keep it as context, send and
  repeat nothing, return `NO_REPLY_DONE`.

The weekly and task texts are unchanged and every other kind keeps the task policy.

### 3. Where the watchdog runs

A oneshot outside the dispatcher: `hive.py watchdog`, started every 5 minutes by the user timer
`ulei-hive-watchdog.timer`. A dispatcher that died or hangs inside a tick is a case the watchdog
exists for, so it shares no process, loop or lock-holding code path with it. It calls no model and
is quiet: a healthy hive makes no alert and no send; its one stdout line is the run's JSON summary
for the unit's journal.

### 4. The conditions

The table `[watchdog]` of the hive config (`hive/watchdog.py`), every key with a default but the
three outside sources, without which their condition is off. Each condition that holds makes one
alert per de-duplication key; the same key alerts again only after its cooldown, measured from the
last alert.

| Condition | Holds when | Keys (default) | De-duplication key | Cooldown (`cooldown_s.<condition>`) |
|---|---|---|---|---|
| `queue_stalled` | cards that may launch (`queued` without a `retry_at` ahead or an executor wait, or `created` past their `not_before`) have waited `stall_s`, and the dispatcher has not ticked for `stall_s` or no head runs and none started within `stall_s` | `stall_s` (900) | `queue_stalled` | 3600 s |
| `vendor_cooldown` | the dispatcher's meta `provider_cooldown:<vendor>` lasts past now | | `vendor_cooldown:<vendor>` | 3600 s |
| `qb_limit` | a provider of qb's weekly limits (`heads.weekly_left`, the dispatcher's `limits_command` and cache) has used `qb_usage_pct` or more, exhausted included | `qb_usage_pct` (90), `qb_providers` (the owner loop's providers; absent: all) | `qb_limit:<provider>` | 21600 s |
| `series_superseded` | the last `superseded_runs` ended instances of an active series were each cancelled by the actor `superseded` | `superseded_runs` (3) | `series_superseded:<series>` | 86400 s |
| `escalation_unread` | an escalation that `escalations_command` lists unread is `escalation_age_s` old | `escalations_command` (none), `escalation_age_s` (1800) | `escalation_unread:<id>` | 86400 s |
| `bridge_token_refused` | a line of the unit `bridge_unit`'s journal within `bridge_window_s` matches `bridge_pattern` | `bridge_unit` (none), `bridge_pattern` (`token refresh refused`), `bridge_window_s` (600), `journal_command` (`journalctl --user`) | `bridge_token_refused` | 3600 s |
| `wait_nudge_repeat` | a running card has `nudge_repeats` or more `wait_nudge` events in a row on one pid; one or two, or nudges alternating between pids, are no streak | `nudge_repeats` (3) | `wait_nudge_repeat:<card>:<pid>` | none: once per card and pid |

The outside sources are commands the config names, so no host, unit or path is in code:

- `escalations_command` prints a JSON list of `{"id": string, "created_at": unix seconds, "read":
  boolean}`; the deployment wraps its escalation store to print it.
- The journal is `journal_command --unit=<bridge_unit> --since=@<now - bridge_window_s>
  --output=json --no-pager --quiet`, one JSON entry per line, whose `MESSAGE` the pattern searches.

A source that fails to start, exits non-zero, times out after `command_timeout_s` (30) or answers
outside its shape is one error of the run: the run checks every other condition, prints the error
on stderr and exits 1, so the unit shows failed.

### 5. The alert record

An alert is a reminder card (ADR-0100) of the actor `watchdog`: class `system`, title `watchdog:
<condition>`, source `watchdog:<condition>`, goal and reminder the alert text, idempotency key
`watchdog:<de-duplication key>:<n>`, `n` counting that key's alerts. The run that finds the
condition makes it due at once: the reminder row in state `due`, the card `blocked` with wait
`reminder`, the event `reminder_due` and the outbox row `hive:<card>:watchdog_alert`, in one
transaction. Then the meta row `watchdog:<de-duplication key>` records the time, `n`, the card
and the condition; a run that dies between the two finds the card again by its idempotency key and
only writes the mark. No table is added.

The text is a fixed English template per condition filled from the facts: counts, minutes, ids,
pids and provider names. No model output, card title, command line, journal line or escalation
text enters it.

### 6. The fallback without the dispatcher

Each watchdog run first runs ADR-0100's fallback over the open reminders the actor `watchdog`
created, as the actor `watchdog`; the dispatcher's tick runs the same fallback over every reminder,
alerts included, as `dispatcher`. So an alert nobody claims within `unclaimed_s` (180 s), or whose
claim stays silent for `claim_silence_s` (300 s), goes out as its original text through `ulei-tg
notify`, followed by the context event `reminder_fallback`; it is delivered only with a Telegram
receipt, a failed send is retried after `retry_s` and never marked delivered, and a withdrawn or
cancelled alert is never sent. With the dispatcher ticking the send comes at the deadline; with
the dispatcher stalled, at the first watchdog run past it, at most one timer period later.

The run takes the dispatcher's lock file for the fallback, trying every second for `lock_wait_s`
(150, longer than one `notify_timeout_s`), so neither a tick nor a secretary's verb sends the
same alert at once. A dispatcher stuck inside a tick holds the lock forever: past the wait the run
sends without it and errs on the side of a duplicate, as ADR-0100 section 7 does.

The run then publishes its own unpublished outbox rows (the alerts, their `reminder_fallback`
events and a failed alert's `task_blocked`) through the configured delivery adapter, without
waiting for the dispatcher's publish. A refused publish keeps the row with its error, and the
fallback still sends the text.

### 7. The C1 kind `watchdog_alert` (C1 1.6.0, additive)

`Kind` gains `watchdog_alert`, id `hive:<card>:watchdog_alert`. Its payload holds `source` `hive`,
`card_id`, `title`, `text` (the alert text), `due_at`, `unclaimed_s`, `claim_silence_s`,
`condition`, `key` (the de-duplication key), `facts` (an object of numbers, ids and booleans of
that condition), a fixed `note` naming the verbs and the fallback, `next_actor` `secretary` and
`how_to_read`; the publish adds the card's class and visibility. The secretary claims, sends and
withdraws it with the reminder verbs, and a direct send is told as `reminder_fallback`.

### 8. The delivery adapter

`reminder_due`, `reminder_fallback` and `watchdog_alert` need a C1 client that knows C1 1.5.0 and
later: the `ulei` adapter, whose `ulei-inbox serve` takes every C1 kind. The `drop` adapter's
older inbox library refuses them (`invalid packet values`): the secretary never sees the events
and the owner gets only the fallback's original text. A deployment switches to `delivery = "ulei"`
with `inbox_socket` and `inbox_producer_key_file` before it enables the watchdog.

### 9. The units

`ulei-hive-watchdog.service` is a oneshot in `ulei.slice` with the hardening of
`ulei-uncertain.service` (`AF_UNIX` for the inbox socket, `AF_INET` and `AF_INET6` for the Bot API
of `ulei-tg notify`), `MemoryMax=128M`, `TasksMax=16` and `TimeoutStartSec=10min`. It runs
`/usr/bin/env $HIVE_CLI watchdog` with `HIVE_CLI` (the interpreter and `hive.py`) and
`HIVE_CONFIG` from `%E/ulei/hive-watchdog.env`, writes only `%S/ulei/hive` unless a drop-in names
the directory of the deployment's hive db, and has no `[Install]`.
`ulei-hive-watchdog.timer` starts it every 5 minutes (`OnCalendar=*:0/5`, 30 s jitter).
`deploy/install.sh` installs both and prints the enable line of the timer; the operator enables it
after the steps of `docs/runbooks/mvp-pilot-cutover.md` part 7.

## Consequences

- A stall of the hive reaches the owner within one timer period of the condition, through the
  secretary when the owner loop works and through `ulei-tg notify` when it does not.
- An ordinary reminder's fallback still needs the dispatcher's tick; a dispatcher that stops
  ticking is itself an alert (`queue_stalled`) once a card waits.
- The escalation and bridge conditions depend on the deployment's sources; a deployment without
  them keeps those two off.
- The new kinds reach the secretary only after the switch to the `ulei` adapter; before it, the
  owner still gets every reminder and alert from the fallback.
