# Runbook: the night gardener

A manual procedure, never run in CI and never run by an agent: the owner runs it on the deployment
host, as the account that runs the ulei units. It sets up `ulei-engine night` (ADR-0078) under
its timer, checks a dry run first, makes the first full pass over an imported history within a
budget, and reads the run report and the digest. Every step names its command, what it must show,
and how to roll it back. A step that fails stops the run: record the command, its output and exit
code and `journalctl --user -u ulei-night -n 50`, then roll back from that step upwards.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_CHECKOUT>`, `<RELEASE_DIR>` | A checkout of the release's commit and the release directory, as in `docs/runbooks/mvp-pilot-cutover.md` 1.3. |
| `<CONFIG_HOME>`, `<STATE_HOME>`, `<RUNTIME_DIR>` | `$XDG_CONFIG_HOME` (default `$HOME/.config`), `$XDG_STATE_HOME` (default `$HOME/.local/state`) and `$XDG_RUNTIME_DIR` (`/run/user/<uid>`) of the account. |
| `<ROUTE>` | The cheap route alias of the gateway the night calls; it must be in the gateway's config. |
| `<PRICES>` | The USD prices per million tokens of `<ROUTE>`'s model: input, output, cache read, cache write. |
| `<SOURCE>` | An archive source the history comes from: `dialogue` for the engine's own, or the name an import gave (`docs/runbooks/memory-archive.md`). |
| `<BUDGET>` | The USD one night may spend. |

The night needs the gateway (`qb.socket`), the garden (`garden.socket`, `docs/runbooks/garden-service.md`)
and, for the history, the archive (`memory.socket`, `docs/runbooks/memory-archive.md`) running.
Its unit writes only `<STATE_HOME>/ulei/night/` and reaches the three by their sockets.

## 1. Install

`deploy/install.sh` puts `ulei-night.service` and `ulei-night.timer` in place with the other
units; the night is the subcommand `night` of `ulei-engine`, so no binary is added:

```sh
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
systemctl --user daemon-reload
systemctl --user cat ulei-night.service ulei-night.timer
```

Expected: both units printed. The timer is not enabled yet.

On a host whose engine runs as the system unit (`mvp-pilot-cutover.md` 6.3, case (b)), give the
night the clients' drop-in, as for the inbox, the channel, the archive and the garden; without it
the run cannot reach the sockets and stops at its first call:

```sh
d=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-night.service.d
install -d -m 0755 "$d"
install -m 0644 <ULEI_CHECKOUT>/deploy/systemd/system/client-no-userns.conf "$d/90-system-engine.conf"
systemctl --user daemon-reload
```

The file is `~/.config/systemd/user/ulei-night.service.d/90-system-engine.conf`.

Roll back: nothing to undo; the units stay unused until the timer is enabled. Remove the drop-in
with the rest of case (b).

## 2. `[night]` in `ulei.toml`

Add the section of `docs/ulei.example.toml`, with the history sources and a small first budget:

```toml
[night]
route = "<ROUTE>"
state = "<STATE_HOME>/ulei/night"
max_usd = "0.5"
price_input = "<input price>"
price_output = "<output price>"
price_cache_read = "<cache read price>"
price_cache_write = "<cache write price>"
sources = "dialogue,<SOURCE>"
```

`state` is the unit's state directory; the unit also sets `ULEI_NIGHT_STATE` to it. An archive
whose `external_id` has another separator than `/` between session and message sets `separator`.
Tool output and system text stay out of the transcripts unless `roles` names them; a message is
cut at `message_chars` (default 3,000).

Roll back: remove the section.

## 3. A dry run

```sh
ULEI_NIGHT_STATE=<STATE_HOME>/ulei/night ~/.local/bin/ulei-engine night --dry-run
```

Expected: exit 0 and one JSON line per stage: `b` the forget rows waiting, `a` the seedbed
records and the calls they make, `c` the sessions (or parts), messages and calls of the history
not yet sorted, each with `tokens_in`, `tokens_out` (taken as a quarter of the input) and `usd`;
`c` also counts in `waiting` the sessions among them that failed before and are tried again;
`d` the hinted facts waiting for the judge and its calls (ADR-0096).
Nothing is sent to the model and nothing is written. The sum of `usd` is what a full pass costs at
`<PRICES>`; `--stage c --limit-sessions 20` estimates the first twenty sessions only.

Roll back: nothing to undo.

## 4. One small run by hand

```sh
systemctl --user start ulei-night.service
journalctl --user -u ulei-night -n 5
```

Expected: the start returns once the run ends; the journal's last line is
`ulei-engine night: <stop> taken <n> proposed <n> accepted <n> digest <n> refused <n> malformed <n>
cleared <n> sessions <n> waiting <n> tokens <in>/<out> usd <x> in <s> s; report <file>`. `<stop>`
is `done`; `done_with_failures` when every stage ran to its end but `waiting` sessions or parts
of the history failed and are tried again on the next runs; or `spend_limit` when the budget ran
out first. All three are clean ends with exit 0. `failed` (exit 1) names the daemon or the
gateway error; what was done before it stays done.

To try a few sessions first: `ULEI_NIGHT_STATE=<STATE_HOME>/ulei/night ~/.local/bin/ulei-engine
night --stage c --limit-sessions 5`.

Roll back: the garden keeps what the run proposed; a fact is withdrawn with the engine's `forget`
of its evidence, which the next night clears (stage b).

## 5. Reading the report

The report is `<STATE_HOME>/ulei/night/runs/<start>-<pid>.jsonl`, one JSON line per call and a
last `total` line:

- `forget`: forget queue rows taken and facts closed.
- `seedbed`: a batch of records with its outcome: `done` with the edge's outcomes (`accepted`,
  `digest`, `refused`, `duplicate`), `as_written` for secret pointers and pins the edge decided as
  written, or `left` with `why` when the answer was malformed twice; its records wait for the next
  night. An answer that hits `max_tokens` is not asked again: the batch's line reads `split` and
  its two halves follow as lines of their own, halved again down to one record. A single record
  whose answer still hits `max_tokens` is `left` with `why` `the answer hit max_tokens`.
- `session`: a session or part of the history with its source, time span, messages, the id of
  its forest record (`forest`, `forest_status`, and `forest_replaced` when it replaced records of
  an earlier run), candidates and their outcomes; `left` when malformed, `given_up` after three
  failed nights (the cursor drops it; its items stay in the archive). A `left` session does not
  stop the run: the walk goes on with the next session, and the session is tried again first on
  each later run.
- `total`: the run's counts, `stop`, tokens and `usd`; `split` counts the batches split,
  `given_up` the sessions given up in this run and `waiting` the sessions still left to try again.

```sh
tail -n 1 <STATE_HOME>/ulei/night/runs/*.jsonl | tail -n 1
grep '"type":"session"' <STATE_HOME>/ulei/night/runs/<report> | head -n 3
```

The summaries are in the forest (ADR-0079), section 5a.

## 5a. Checking the forest after a night

Each session or part stage c sent is a forest record in the archive's file: its summary, time
span, the run and route that made it, and links to the archive items it compresses. With the
`c10` helper of `docs/runbooks/memory-archive.md` section 5:

```sh
c10 health '{}'
c10 forest_search '{"q":"<WORD>","limit":3}'
c10 forest_search '{"q":"<WORD>","source":"<SOURCE>","since":<UNIX_FROM>,"until":<UNIX_TO>}'
```

Expected: `forest` in `health` is about the `sessions` of the run reports since the first night;
`forest_search` answers `hits`, each with `session`, `text`, `from`, `to`, `producer` (`run` names
the report `runs/<run>.jsonl`) and `links`. A session sent again (a reset cursor, a run killed
before its cursor was written) replaces its record: the report line's `forest_replaced` names the
old records, which stay in the file and are never found. To the owner bot: `Search the archive
for <WORD>`; the answer's `forest` list holds the record with its `forest:<id>` ref and
`archive:<id>` refs, and the archive is not searched.

Fail: `forest` absent from `health` after a run with `sessions` above 0: the run's `total` line
has `stop` `failed` and its stderr line names the archive's error (`journalctl --user -u
ulei-night.service`); the unsent session waits for the next run. A `forest_put` refused
`not_found` names an entry deleted from the archive during the run; the next run sends that
session again without it.

Forget a summary with the engine's `forget` and its ref `forest:<id>`; `undo` brings it back. A
summary of a dialogue session that holds a message forgotten in the journal is never shown by
`archive_search`.

## 6. The digest, the judge and its changes

Candidates of low confidence, contradictions of an attribute and ambiguous entity names wait in
the garden's digest for the owner, and so do the contradictions the judge of stage d recorded
between a new fact and an earlier one, each with the judge's one-line reason in `note`
(ADR-0096). Nothing is pushed to the owner; the digest is read on demand:

```sh
curl -sS --unix-socket <RUNTIME_DIR>/ulei/garden.sock -d '{}' http://localhost/v1/digest
```

Expected: the open items with their reasons. The owner settles each with C7 `resolve`; the
night never decides them.

Stage d also merges a new fact into an earlier one that says the same (the earlier wording stays
and its `proof_count` rises) and applies a supersede the source rank allows, silently. Each
change is recorded with both facts, the judge's model and reason and the embedder's model, and
can be listed and reversed:

```sh
curl -sS --unix-socket <RUNTIME_DIR>/ulei/garden.sock -d '{"limit": 20}' \
  http://localhost/v1/reconcile_review
curl -sS --unix-socket <RUNTIME_DIR>/ulei/garden.sock -d '{"change_id": 42}' \
  http://localhost/v1/reconcile_undo
```

An undo writes the retracted fact back as current and keeps the pair from the judge for good. An
agent reads the same list with its tool `garden_review`. The run report has one `judge` line per
call with counts only, and the totals `judged`, `merged`, `superseded`, `contradictions`,
`stale`, `judge_failures`, `judge_timeouts` and `judge_usd`; the garden's `health` has the
anonymous counters under `reconcile` (facts waiting and hinted, verdicts, undos, the judge's
calls and tokens, the wait from a fact to its verdict in buckets). They stay on the host.

The judge spends from `night.judge_max_usd` (default 0.5 USD), apart from `night.max_usd`, and
runs after stage a or c met their limit; facts it leaves wait for the next night. A run with
`--stage d` judges only. Before trusting a new route as the judge, run the judge eval by hand:

```sh
python3 ci/judge_eval.py --gateway <RUNTIME_DIR>/qb.sock --route small
```

Without a qb route, the same eval runs through the Claude Code CLI on the account it is logged
in to (each call a context-free headless session: empty working directory, the judge prompt as
the whole system prompt, no tools; `--limit 6` is a cheap smoke run):

```sh
python3 ci/judge_eval.py --claude --model sonnet --out judge-eval.json
```

## 7. The first full pass over an imported history

Raise `max_usd` to `<BUDGET>` and run by hand, or let the timer do it night by night:

```sh
ULEI_NIGHT_MAX_USD=<BUDGET> ULEI_NIGHT_STATE=<STATE_HOME>/ulei/night \
  ~/.local/bin/ulei-engine night --stage c
```

The cursor `<STATE_HOME>/ulei/night/cursor.json` holds per source the `seq` all earlier entries
of which are done, the window being walked and, under `retry`, the failed sessions of earlier
windows with their failure counts. A run stopped by the limit, a timeout or a kill resumes at the
next unsent session; no session is sent twice. A failed session does not stop the pass; it waits
in `retry` until it is done or given up after three runs. A pass larger than one night's
budget continues on the next nights. Check the dry run of step 3 again between runs: its stage `c`
shrinks to the history still unsorted.

Roll back: stop the run (`systemctl --user stop ulei-night.service`); the cursor stays consistent.
Deleting `cursor.json` would send the history again.

## 8. Enable the timer

```sh
systemctl --user enable --now ulei-night.timer
systemctl --user list-timers ulei-night.timer
```

Expected: the next run at 03:30 within 15 minutes. A night missed while the user manager was down
runs at its next start (`Persistent=true`).

Roll back: `systemctl --user disable --now ulei-night.timer`.
