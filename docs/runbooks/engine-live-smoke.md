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
| `<brain-dir>` | A directory holding a SOUL.md and `memories/MEMORY.md` and `memories/USER.md` written for the smoke. |

## `ulei-engine run`

```sh
ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--config PATH]
```

Runs one turn of the current session of key `S` with the built-in tools in the current
directory, appends the turn to the journal and prints the final assistant text to stdout. A
turn that reaches its step limit asks the model once more, without tools, for a summary and
prints it. A reply without visible text is retried; one that stays empty prints `(empty)`. `--new` first starts an empty session for `S`; the older sessions stay in the
journal. The first turn of a session builds its system prompt from the prompt files and the
host and stores it with the session; every later turn sends it unchanged.

| Exit | Meaning | stderr |
|---|---|---|
| 0 | The model stopped, or the step limit was reached. | nothing |
| 1 | Any other failure: settings, journal, gateway error, transport. | `ulei-engine: <reason>` |
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

- Settings. Each environment variable wins over the config file; only
  `engine.concurrent_tool_timeout`, `engine.max_steps`, `engine.empty_response_guard.enabled`,
  `engine.memory_char_limit`, `engine.user_char_limit`, `engine.web_extract_char_limit` and
  `security.redact_secrets` have a default, and `engine.service_unit`, `engine.timezone`,
  `engine.disabled_skills` and `engine.web_helper` may stay unset. The other `engine.web_*`
  settings are read only when `engine.web_helper` is set.

  | Config key | Environment variable | Value |
  |---|---|---|
  | (the file itself) | `ULEI_CONFIG` | Path of the config file; `--config PATH` wins over it. |
  | `qb.socket` | `ULEI_QB_SOCKET` | `<qb-socket>` |
  | `engine.journal` | `ULEI_ENGINE_JOURNAL` | Absolute path of the journal file. |
  | `engine.terminal_output` | `ULEI_ENGINE_TERMINAL_OUTPUT` | Absolute path of the directory for the full output of `terminal` calls whose result was cut. |
  | `engine.concurrent_tool_timeout` | `ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT` | Seconds a parallel batch of tool calls may run; a call still running after that is answered with a timeout error. `0` or less means no deadline. Default 420. |
  | `engine.max_steps` | `ULEI_ENGINE_MAX_STEPS` | Steps of one turn before the summary request. `0` or less, `none`, `null`, `unlimited`, `infinite`, `infinity`, `inf` or `∞` means no limit. Default 200. |
  | `engine.empty_response_guard.enabled` | `ULEI_ENGINE_EMPTY_RESPONSE_GUARD_ENABLED` | Two empty replies in a row with zero output tokens from one route end the empty-reply retries early; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
  | `engine.service_unit` | `ULEI_ENGINE_SERVICE_UNIT` | The systemd service unit running the engine, with or without `.service`; `terminal` refuses a command that would restart, stop or kill it. Unset: under systemd (`INVOCATION_ID` set) the unit is the last `.service` component of the engine's cgroup path, and the engine refuses to start when there is none; outside systemd the guard is off. |
  | `engine.soul_file` | `ULEI_ENGINE_SOUL_FILE` | SOUL.md, the identity that opens the system prompt. A missing or blank file fails the turn. |
  | `engine.index_file` | `ULEI_ENGINE_INDEX_FILE` | INDEX.md of the knowledge base; the system prompt names it and the engine does not read it. |
  | `engine.memory_file` | `ULEI_ENGINE_MEMORY_FILE` | MEMORY.md, rendered as the MEMORY block; absent means no block. |
  | `engine.user_file` | `ULEI_ENGINE_USER_FILE` | USER.md, rendered as the USER PROFILE block; absent means no block. |
  | `engine.memory_char_limit` | `ULEI_ENGINE_MEMORY_CHAR_LIMIT` | Char limit shown in the MEMORY block header; a larger file is rendered in full at 100%. Default 2200. |
  | `engine.user_char_limit` | `ULEI_ENGINE_USER_CHAR_LIMIT` | The same for the USER PROFILE block. Default 1375. |
  | `engine.skills_dir` | `ULEI_ENGINE_SKILLS_DIR` | Absolute path of the skills directory: one directory per skill with its `SKILL.md`, under category directories. The system prompt indexes it, `skills_list` and `skill_view` read it, and C6 `skills_dir` names it. Absent means no index; `skills_list` creates it. |
  | `engine.disabled_skills` | `ULEI_ENGINE_DISABLED_SKILLS` | Comma-separated names (frontmatter `name` or directory name) of the skills the index, `skills_list` and `skill_view` leave out. Unset: none. |
  | `engine.timezone` | `ULEI_ENGINE_TIMEZONE` | IANA zone of the `Conversation started` line, read from the system zone database. Unset: the host's local time from `/etc/localtime`, UTC without that file. |
  | `engine.web_helper` | `ULEI_ENGINE_WEB_HELPER` | Path of the `ulei-web` helper (`cargo build --release --locked -p ulei-engine --features web --bin ulei-web`). Set: `web_search` and `web_extract` are offered and each call runs the helper once with an empty environment. Unset: no web tools. |
  | `engine.web_base_url` | `ULEI_ENGINE_WEB_BASE_URL` | Base URL of the Exa API, an `https://` URL; `/search` and `/contents` are appended to it. |
  | `engine.web_env_file` | `ULEI_ENGINE_WEB_ENV_FILE` | Absolute path of the env-file with the line `EXA_API_KEY=<key>`. The helper reads it at each call; the key never enters argv, an environment, the journal or a tool result. |
  | `engine.web_store` | `ULEI_ENGINE_WEB_STORE` | Absolute path of the directory for the full text of each page `web_extract` cut; the result's footer names the file. |
  | `engine.web_extract_char_limit` | `ULEI_ENGINE_WEB_EXTRACT_CHAR_LIMIT` | Characters per page a `web_extract` call without `char_limit` returns whole, clamped to 2000-500000. Default 15000. |
  | `security.redact_secrets` | `ULEI_SECURITY_REDACT_SECRETS` | Masks secrets in the output of `terminal`, `read_file` and `search_files`; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |

  The config file accepts only section headers, `key = "string"`, `key = integer`,
  `key = true` or `false`, comments and blank lines; any other line fails with `config <path> line <n>: <reason>`. For the
  smoke:

  ```toml
  [qb]
  socket = "<qb-socket>"

  [engine]
  journal = "<state-dir>/a/journal.sqlite3"
  terminal_output = "<state-dir>/a/terminal-output"
  soul_file = "<brain-dir>/SOUL.md"
  index_file = "<brain-dir>/INDEX.md"
  memory_file = "<brain-dir>/memories/MEMORY.md"
  user_file = "<brain-dir>/memories/USER.md"
  skills_dir = "<brain-dir>/skills"
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
The step id is `s1-t3-0`. Edit `<brain-dir>/SOUL.md` and run the check again: the session
keeps the system prompt its first turn stored, so the edit does not reach it.

```sh
sqlite3 <state-dir>/a/journal.sqlite3 'SELECT id, length(system_prompt) FROM sessions'
```

### 4. `--new`

```sh
<bin> run --session smoke --route <route> --new --prompt 'Which word did you answer first?'
```

Expected: exit 0; the model says it has no earlier answer. The step id is `s2-t1-0`, session 2
stores a system prompt built from the edited SOUL.md, and session 1 keeps its rows:

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
