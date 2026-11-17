# Runbook: `ulei-engine run` live smoke against a real qb gateway

A manual procedure, never run in CI: CI and tests reach only the scripted fake gateway
(`crates/engine/AGENTS.md`). It checks what the fake cannot show: how real qb answers the
engine's requests. Run it after a change of the engine's wire behaviour or of the qb pin.

Placeholders used below:

| Placeholder | Meaning |
|---|---|
| `<bin>` | The built `ulei-engine` binary. |
| `<qb-socket>` | Unix socket of the running qb gateway. |
| `<route>` | A route alias configured in qb that can call tools. |
| `<state-dir>` | A fresh directory for the smoke journals, owned by the operator. |
| `<work-dir>` | A scratch directory the tool turns may write into. |

## `ulei-engine run`

```sh
ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--config PATH]
```

Runs one turn of the current session of key `S` with the built-in tools in the current
directory, at most 64 steps, appends the turn to the journal and prints the final assistant
text to stdout. `--new` first starts an empty session for `S`; the older sessions stay in the
journal.

| Exit | Meaning | stderr |
|---|---|---|
| 0 | The model stopped. | nothing |
| 1 | Any other failure: settings, journal, gateway error, transport, max steps. | `ulei-engine: <reason>` |
| 2 | Usage error. | the usage line |
| 3 | Every login of the route is out of quota. | `quota exhausted on route <route>; nearest reset <UTC time>` |

Every turn that reached the gateway is appended, whatever its exit code: the prompt, the
messages the turn produced and the request ids of its steps. A process killed before the
append stores nothing of its turn.

## Prerequisites

- A running qb gateway on `<qb-socket>` and a route `<route>` with at least one login that
  has quota. `GET /v1/model/routes` on the socket lists the routes.
- The binary, built from the commit under test:

  ```sh
  cargo build --release --locked -p ulei-engine
  <bin> --version
  ```

- Settings. Each environment variable wins over the config file; there are no defaults.

  | Config key | Environment variable | Value |
  |---|---|---|
  | (the file itself) | `ULEI_CONFIG` | Path of the config file; `--config PATH` wins over it. |
  | `qb.socket` | `ULEI_QB_SOCKET` | `<qb-socket>` |
  | `engine.journal` | `ULEI_ENGINE_JOURNAL` | Absolute path of the journal file. |

  The config file accepts only section headers, `key = "string"`, `key = integer`, comments
  and blank lines; any other line fails with `config <path> line <n>: <reason>`. For the
  smoke:

  ```toml
  [qb]
  socket = "<qb-socket>"

  [engine]
  journal = "<state-dir>/a/journal.sqlite3"
  ```

  ```sh
  export ULEI_CONFIG=<state-dir>/ulei.toml
  cd <work-dir>
  ```

- `sqlite3` to read the journal. Read a journal only while no `ulei-engine` runs on it.

## Checks

Run them in order; each names its expected result. A different result is a defect: record
the command, its stdout, stderr and exit code, and the journal rows of the turn.

### 1. Plain turn

```sh
<bin> run --session smoke --route <route> --prompt 'Answer with the single word: pong'
echo "exit $?"
```

Expected: stdout `pong` (in any case), `exit 0`, stderr empty. The journal directory
`<state-dir>/a` is mode 0700 and `journal.sqlite3` is 0600 (`stat -c %a`). The journal holds
the step `s1-t1-0` with the route and usage qb reported:

```sh
sqlite3 <state-dir>/a/journal.sqlite3 'SELECT request_id, route, usage FROM steps'
```

### 2. Tool turn

```sh
<bin> run --session smoke --route <route> \
  --prompt 'Use the write_file tool to create smoke.txt containing ok, then reply done'
```

Expected: exit 0, stdout contains `done`, `<work-dir>/smoke.txt` contains `ok`, and the
journal holds at least two steps of turn 2 (`s1-t2-0`, `s1-t2-1`, ...) and a `tool_result`
message.

### 3. Continuation after a restart

Each invocation is a new process, so the next one is the restart.

```sh
<bin> run --session smoke --route <route> --prompt 'Which word did you answer first?'
```

Expected: exit 0 and stdout names `pong`: qb received the stored history of turns 1 and 2.
The step id is `s1-t3-0`.

### 4. `--new`

```sh
<bin> run --session smoke --route <route> --new --prompt 'Which word did you answer first?'
```

Expected: exit 0; the model says it has no earlier answer. The step id is `s2-t1-0`, and
session 1 keeps its rows:

```sh
sqlite3 <state-dir>/a/journal.sqlite3 'SELECT id, key FROM sessions'
```

### 5. Quota line

Needs a route whose every login is out of quota, for example a route in qb with a single
exhausted login.

```sh
<bin> run --session quota --route <exhausted-route> --prompt hi; echo "exit $?"
```

Expected: exit 3, stdout empty, stderr exactly one line
`quota exhausted on route <exhausted-route>; nearest reset YYYY-MM-DD HH:MM:SS UTC`, the
time matching the reset qb reports in `GET /v1/model/routes`.

### 6. Two user messages in a row after an interrupted turn

A turn that ends without an assistant reply leaves its prompt as the last message, so the
next turn sends two user messages in a row. The quota turn of check 5 is one such turn; an
interrupted one is made by cutting the gateway while it streams:

```sh
<bin> run --session cut --route <route> --prompt 'Count slowly from 1 to 300, one per line'
# while it runs, restart or stop the qb gateway; the run exits 1 with one stderr line
<bin> run --session cut --route <route> --prompt 'Reply with the word: resumed'
<bin> run --session quota --route <route> --prompt 'Reply with the word: resumed'
```

Expected: the cut run exits 1 with `ulei-engine: ...` naming the cut or the socket error.
Both later runs exit 0 with `resumed`: qb accepts a history whose last two messages are user
messages. The journal shows them:

```sh
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT seq, turn, json_extract(message, '$.role') FROM messages
   WHERE session = (SELECT max(id) FROM sessions WHERE key = 'cut') ORDER BY seq"
```

### 7. A re-sent request_id

The request id of a step is `s<session>-t<turn>-<step>` from journal state alone, so qb sees
the same id again in two cases.

Crash before the append: start a long turn and kill the process while it streams (Ctrl-C
kills it, the turn is not appended), then run again.

```sh
<bin> run --session crash --route <route> --prompt 'Count slowly from 1 to 300, one per line'
# Ctrl-C while it streams
<bin> run --session crash --route <route> --prompt 'Reply with the word: again'
```

Two journal files: point a second journal at the same qb.

```sh
ULEI_ENGINE_JOURNAL=<state-dir>/b/journal.sqlite3 \
  <bin> run --session other --route <route> --prompt 'Reply with the word: second'
```

Expected: both runs exit 0 with their word. The killed turn and the rerun both send
`sNN-tMM-0` with the same numbers (the rerun's id in the journal's `steps` is the next turn
after the last appended one, so the killed request carried it too; check qb's log for it
only if qb records request ids), and the run on journal `b` sends `s1-t1-0`, the id of
check 1.
qb answers each as a new turn: no `bad_request` for a repeated id, and no replay of the
earlier answer.
