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

Roll back: nothing to undo; the units stay unused until the timer is enabled.

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
not yet sorted, each with `tokens_in`, `tokens_out` (taken as a quarter of the input) and `usd`.
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
cleared <n> sessions <n> tokens <in>/<out> usd <x> in <s> s; report <file>`. `<stop>` is `done`,
or `spend_limit` when the budget ran out first; both are clean ends with exit 0. `failed` (exit 1)
names the daemon or the gateway error; what was done before it stays done.

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
  night.
- `session`: a session or part of the history with its source, time span, messages, `summary`,
  candidates and their outcomes; `left` when malformed, `given_up` after three failed nights (the
  cursor passes it; its items stay in the archive).
- `total`: the run's counts, `stop`, tokens and `usd`.

```sh
tail -n 1 <STATE_HOME>/ulei/night/runs/*.jsonl | tail -n 1
grep '"type":"session"' <STATE_HOME>/ulei/night/runs/<report> | head -n 3
```

The summaries stay in the reports until the forest (ADR-0051) stores them.

## 6. The digest

Candidates of low confidence, contradictions of a current fact and ambiguous entity names wait in
the garden's digest for the owner:

```sh
curl -sS --unix-socket <RUNTIME_DIR>/ulei/garden.sock -d '{}' http://localhost/v1/digest
```

Expected: the open items with their reasons. The owner settles each with C7 `resolve`; the
night never decides them.

## 7. The first full pass over an imported history

Raise `max_usd` to `<BUDGET>` and run by hand, or let the timer do it night by night:

```sh
ULEI_NIGHT_MAX_USD=<BUDGET> ULEI_NIGHT_STATE=<STATE_HOME>/ulei/night \
  ~/.local/bin/ulei-engine night --stage c
```

The cursor `<STATE_HOME>/ulei/night/cursor.json` holds per source the `seq` all earlier entries
of which are done and the window being walked. A run stopped by the limit, a timeout or a kill
resumes at the next unsent session; no session is sent twice. A pass larger than one night's
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
