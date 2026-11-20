# Runbook: `ulei-engine run` live smoke against a real qb gateway

A manual procedure, never run in CI: CI and tests reach only the scripted fake gateway
(`crates/engine/AGENTS.md`). It checks what the fake cannot show: how real qb answers the
engine's requests. Run it after a change of the engine's wire behaviour or of the qb pin. The
run is manual: its first pass against a live qb gateway is a U4 step of `docs/roadmap.md`.

Placeholders used below:

| Placeholder | Meaning |
|---|---|
| `<bin>` | The built `ulei-engine` binary. |
| `<qb-socket>` | Unix socket of the running qb gateway. |
| `<route>` | A route alias configured in qb that can call tools. |
| `<route-b>` | A second route alias, pinned in qb to another vendor or model than `<route>`. |
| `<compact-route>` | The route alias in qb of the compaction summary turn. |
| `<c6-socket>` | Path of the C6 socket of `serve`. |
| `<mcp-url>` | `http://` URL on a loopback host of a Streamable HTTP MCP server run for the smoke. |
| `<mcp-tool>` | A tool of that server that answers without side effects. |
| `<state-dir>` | A fresh directory for the smoke journals, owned by the operator. |
| `<work-dir>` | A scratch directory the tool turns may write into. |
| `<brain-dir>` | A directory holding a SOUL.md and `memories/MEMORY.md` and `memories/USER.md` written for the smoke. |

## `ulei-engine run`

```sh
ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--channel NAME] [--event] [--config PATH]
```

Runs one turn of the current session of key `S` on the route `ALIAS`, whatever route the
session stores, with the built-in tools in the current directory, appends the turn to the journal and prints the final assistant text to stdout. A
turn that reaches its step limit asks the model once more, without tools, for a summary and
prints it. A reply without visible text is retried; one that stays empty prints `(empty)`. `--new` first starts an empty session for `S`; the older sessions stay in the
journal. The first turn of a session builds its system prompt from the prompt files and the
host and stores it with the session; every later turn sends it unchanged. `--channel NAME`
names the channel the prompt came on and `--event` marks it as an event rather than the
owner's message; a turn is a top-level owner turn only when the owner's prompt comes on the
channel `engine.main_channel` names (ADR-0022).

| Exit | Meaning | stderr |
|---|---|---|
| 0 | The model stopped, or the step limit was reached. | nothing |
| 1 | Any other failure: settings, journal, gateway error, transport. | `ulei-engine: <reason>` |
| 2 | Usage error. | the usage line |
| 3 | Every login of the route is out of quota. | `quota exhausted on route <route>; nearest reset <UTC time>` |
| 130 | SIGINT, SIGTERM or SIGHUP interrupted the turn: its tool process groups were killed and its gateway turn cancelled (ADR-0029). | `ulei-engine: the turn was interrupted` |

Every turn that reached the gateway is appended, whatever its exit code: the prompt, the
messages the turn produced and the request ids of its steps. The first SIGINT, SIGTERM or
SIGHUP ends the turn and the process exits once the turn is appended; later ones change
nothing. A process killed before the append, by SIGKILL or by a signal that arrives while the
settings are read, stores nothing of its turn.

## `ulei-engine serve`

```sh
ulei-engine serve [--config PATH]
```

Reads its settings once, opens the journal, binds the C6 socket `engine.c6_socket` with mode
0600, prints `ulei-engine ready <socket>` to stdout and serves C6 per ADR-0008 until `accept`
fails: `inject` and `is_idle` on the session `engine.owner_session`, `status`, `submit_turn`,
`interrupt` and `set_route` (ADR-0028). `interrupt` ends the running turn of a session as a
signal ends the turn of `run` and drops the turn queued next; the turn reports `interrupted`
once it is appended. Each submitted turn is a turn of `run` with `--route` set to the alias
`set_route` stored with the session, else `engine.default_route`, its `channel` as `--channel`
and its `new` as `--new`; a session runs one turn at a time and queues up to 32 more, and
different sessions run at the same time. An injected event is such a turn of the owner session
with its payload as `--prompt` and `--event`, started only while that session runs and queues
no turn. The turn settings come from the config file read at the start and from
the environment when each turn starts, as `run` reads them; one that would fail a `run` fails
that turn with outcome `failed`. A missing directory of the socket is created 0700; a directory that is
not the user's or that other users can access fails the start, as does any setting below. A
failed start or a failed `accept` exits 1 with one stderr line `ulei-engine: <reason>`.
SIGINT and SIGTERM end `serve` at once: running turns are not appended, and their tool process
groups end only with the service unit's control group (ADR-0029).

## Prerequisites

- A running qb gateway on `<qb-socket>` and a route `<route>` with at least one login that
  has quota. The gateway serves only its own uid and the uids its config lists as clients:
  run the engine as one of them, or every request is refused with `auth`. List the routes,
  their state and, for a pinned route, its vendor and model:

  ```sh
  curl -sS --unix-socket <qb-socket> http://localhost/v1/model/routes
  ```
- The binary, built from the commit under test:

  ```sh
  cargo build --release --locked -p ulei-engine
  <bin> --version
  ```

- Settings. Each environment variable wins over the config file; only
  `engine.concurrent_tool_timeout`, `engine.max_steps`, `engine.empty_response_guard.enabled`,
  `engine.memory_char_limit`, `engine.user_char_limit`, `engine.web_extract_char_limit`,
  `engine.no_ask_guard.timeout`, the `engine.tool_loop_guardrails.*` and
  `engine.compression.*` settings and `security.redact_secrets` have a default, and
  `engine.service_unit`, `engine.timezone`, `engine.disabled_skills`, `engine.web_helper`,
  `engine.main_channel`, `engine.turn_reminder_file`, `engine.no_ask_guard.route`,
  `engine.mcp_servers` and the `engine.routes.<alias>.*` settings may stay unset.
  `engine.c6_socket`, `engine.owner_session` and `engine.default_route` are read by `serve`
  only, which requires them. The other `engine.web_*` settings are read only when
  `engine.web_helper` is set, the other `engine.no_ask_guard.*` settings only when
  `engine.no_ask_guard.route` is set, the `engine.mcp.<server>.*` settings only for the
  servers `engine.mcp_servers` names; of these only `url` is required.

  | Config key | Environment variable | Value |
  |---|---|---|
  | (the file itself) | `ULEI_CONFIG` | Path of the config file; `--config PATH` wins over it. |
  | `qb.socket` | `ULEI_QB_SOCKET` | `<qb-socket>` |
  | `engine.c6_socket` | `ULEI_ENGINE_C6_SOCKET` | Path of the C6 socket of `serve`, at most 107 bytes, in a directory only the user can access. |
  | `engine.owner_session` | `ULEI_ENGINE_OWNER_SESSION` | Session key of the owner's session, which `inject` runs events in and `is_idle` reports on; 1 to 128 chars. |
  | `engine.default_route` | `ULEI_ENGINE_DEFAULT_ROUTE` | Route alias in qb on which the turns of `serve` run in a session that stores none, matching `^[a-z][a-z0-9_-]{0,63}$`. |
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
  | `engine.web_base_url` | `ULEI_ENGINE_WEB_BASE_URL` | Base URL of the Exa API, an `https://` URL; `/search` and `/contents` are appended to it. A build with the `web-loopback-http` test switch also takes an `http://` URL whose host is a loopback address literal. |
  | `engine.web_env_file` | `ULEI_ENGINE_WEB_ENV_FILE` | Absolute path of the env-file with the line `EXA_API_KEY=<key>`. The helper reads it at each call; the key never enters argv, an environment, the journal or a tool result. |
  | `engine.web_store` | `ULEI_ENGINE_WEB_STORE` | Absolute path of the directory for the full text of each page `web_extract` cut; the result's footer names the file. |
  | `engine.web_extract_char_limit` | `ULEI_ENGINE_WEB_EXTRACT_CHAR_LIMIT` | Characters per page a `web_extract` call without `char_limit` returns whole, clamped to 2000-500000. Default 15000. |
  | `engine.mcp_servers` | `ULEI_ENGINE_MCP_SERVERS` | Names of the MCP servers, comma separated. At the start of each turn every server is connected at once, until the turn is interrupted; one that fails is reported on stderr as `ulei-engine: Failed to connect to MCP server '<server>': <reason>` and the run goes on without its tools. Its tools are offered as `mcp__<server>__<tool>`, every character outside `[A-Za-z0-9_]` of both parts as `_`. |
  | `engine.mcp.<server>.url` | `ULEI_ENGINE_MCP_<SERVER>_URL` | `http://` URL of the server's Streamable HTTP endpoint on `127.0.0.1`, `[::1]` or `localhost` (resolving only to loopback); any other URL fails the run with an error naming the key. |
  | `engine.mcp.<server>.env_file` | `ULEI_ENGINE_MCP_<SERVER>_ENV_FILE` | Absolute path of an env-file whose `NAME=value` lines are the request headers of the server, for example `Authorization=Bearer <token>`. The values never reach argv, an environment, the journal, a tool result or stderr. Unset: no extra headers. |
  | `engine.mcp.<server>.timeout` | `ULEI_ENGINE_MCP_<SERVER>_TIMEOUT` | Seconds one call to the server may take before it fails with `MCP call timed out`. `0` or less means no deadline. Default 300. |
  | `engine.mcp.<server>.connect_timeout` | `ULEI_ENGINE_MCP_<SERVER>_CONNECT_TIMEOUT` | Seconds connecting to the server and listing its tools may take. `0` or less means no deadline. Default 60. |
  | `engine.mcp.<server>.tools.include` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_INCLUDE` | Tools of the server to offer, comma separated exact names or globs with `*`, `?` or `[...]`. Unset: all. |
  | `engine.mcp.<server>.tools.resources` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_RESOURCES` | Offers `list_resources` and `read_resource` of a server that advertises resources; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
  | `engine.mcp.<server>.tools.prompts` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_PROMPTS` | Offers `list_prompts` and `get_prompt` of a server that advertises prompts; read as `tools.resources`. Default on. |
  | `engine.main_channel` | `ULEI_ENGINE_MAIN_CHANNEL` | The channel whose owner prompts (`--channel` equal to it, no `--event`) are top-level owner turns. Unset: no turn is one. |
  | `engine.turn_reminder_file` | `ULEI_ENGINE_TURN_REMINDER_FILE` | Absolute path of the UTF-8 file whose bytes, unchanged, are appended after a blank line to every request copy of the prompt of a top-level owner turn; the journal and the system prompt never carry them. Read when each turn starts; a missing or unreadable file fails that turn, an empty one is off. Unset: off. |
  | `engine.no_ask_guard.route` | `ULEI_ENGINE_NO_ASK_GUARD_ROUTE` | Route alias in qb of the no-ask-guard classifier. Set: on a top-level owner turn, a final reply whose last two non-blank lines (code blocks and URLs left out) hold a `?` goes to one tool-less request on this route per part of the reply; on an `extra` verdict the reply does not end the turn, which continues with the note, at most 2 times per turn; a `legit` verdict, a failure, a timeout or a quota answer delivers the reply as it is (ADR-0023). Unset: off, and no other `engine.no_ask_guard.*` setting is read. |
  | `engine.no_ask_guard.note_file` | `ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE` | Absolute path of the UTF-8 continuation note: the whole user message sent after a held-back reply, with one or more `{chain}` placeholders for the 16-hex chain id; `{{` and `}}` stand for braces. Read when each turn starts; unset, missing, unreadable or without `{chain}` fails that turn. |
  | `engine.no_ask_guard.instructions_file` | `ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE` | Absolute path of the UTF-8 classifier instructions with the placeholders `{tag}` (a one-time `⟦DATA-<12 hex>⟧` marker), `{owner}` (the end of the owner's prompt) and `{reply}` (a part of the reply), braces as in the note. The classifier answers with a JSON object whose `verdict` is `extra` or `legit`. Read when each turn starts; unset, missing, unreadable or without one of the placeholders fails that turn. |
  | `engine.no_ask_guard.log` | `ULEI_ENGINE_NO_ASK_GUARD_LOG` | Absolute path of the JSON-lines file of the guard's records (`pass`, `pass_loop_cap`, `suppressed_continue`, `continuation_done`); its directory is created. A held-back reply whose record cannot be written is delivered instead. |
  | `engine.no_ask_guard.timeout` | `ULEI_ENGINE_NO_ASK_GUARD_TIMEOUT` | Seconds one classifier request may take before it is cancelled and the reply delivered. `0` or less means no deadline. Default 45. |
  | `engine.tool_loop_guardrails.warnings_enabled` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARNINGS_ENABLED` | Appends a `[Tool loop warning: ...]` line to the result of a call that failed again with identical arguments, a tool that failed again this turn, or a read-only call (`read_file`, `search_files`, `web_search`, `web_extract`) that returned the same result again (ADR-0016 section 12); on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
  | `engine.tool_loop_guardrails.hard_stop_enabled` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_ENABLED` | Blocks a call past its `hard_stop_after` count before it runs, halts the turn at the `same_tool_failure` count, and ends the turn after that batch with a fixed answer naming the tool; on only for `1`, `true`, `yes` or `on` (any case). Default off. |
  | `engine.tool_loop_guardrails.warn_after.exact_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_EXACT_FAILURE` | Failures of one call with identical arguments in a turn that warn; a positive integer. Default 2. |
  | `engine.tool_loop_guardrails.warn_after.same_tool_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_SAME_TOOL_FAILURE` | Failures of one tool in a turn, with no success of it between, that warn; a positive integer. Default 3. |
  | `engine.tool_loop_guardrails.warn_after.idempotent_no_progress` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_IDEMPOTENT_NO_PROGRESS` | Unchanged results in a turn of one read-only call with identical arguments, each the same as its previous one, that warn; a positive integer. Default 2. |
  | `engine.tool_loop_guardrails.hard_stop_after.exact_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_EXACT_FAILURE` | With hard stop: failures of one call with identical arguments after which it is blocked; a positive integer. Default 5. |
  | `engine.tool_loop_guardrails.hard_stop_after.same_tool_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_SAME_TOOL_FAILURE` | With hard stop: failures of one tool at which the turn halts; a positive integer. Default 8. |
  | `engine.tool_loop_guardrails.hard_stop_after.idempotent_no_progress` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_IDEMPOTENT_NO_PROGRESS` | With hard stop: unchanged results of one read-only call with identical arguments after which it is blocked; a positive integer. Default 5. |
  | `engine.tool_loop_guardrails.loop_caps.max_web_searches` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES` | `web_search` calls one turn may make, hard stop on or off; the next one does not run, gets a `loop_web_search_cap` result and the turn ends after its batch with the fixed answer naming the tool. A non-negative integer; `0` is no limit. Default 50. |
  | `engine.stall_guards` | `ULEI_ENGINE_STALL_GUARDS` | Within a turn, from the 3rd consecutive call of one tool with identical arguments and an identical result, appends a `[note: this is the 3rd consecutive identical call ...]` line; from the 2nd, replaces a successful result of 512 characters or more with a `[note: this result is byte-identical ...]` reference to the first call of the run (ADR-0016 section 12); on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
  | `engine.compression.route` | `ULEI_ENGINE_COMPRESSION_ROUTE` | Route alias in qb of the compaction summary turn, a tool-less turn without `session_key` whose `request_id` is `compact-<16 hex>`. An alias qb refuses makes every compaction store the deterministic fallback summary. Default `compact`. |
  | `engine.compression.timeout` | `ULEI_ENGINE_COMPRESSION_TIMEOUT` | Seconds the summary turn may take before it is cancelled. `0` or less means no deadline. Default 450. |
  | `engine.compression.protect_last_n` | `ULEI_ENGINE_COMPRESSION_PROTECT_LAST_N` | Recent messages a compaction keeps verbatim at least, read as 3 to 8; a non-negative integer. Default 20. |
  | `engine.compression.target_ratio` | `ULEI_ENGINE_COMPRESSION_TARGET_RATIO` | Share of the threshold kept as the verbatim tail, clamped to 0.10-0.80; a decimal number, in the file a string such as `"0.2"` or an integer. Default 0.20. |
  | `engine.compression.threshold` | `ULEI_ENGINE_COMPRESSION_THRESHOLD` | Share of the route's input budget (`context_length` less the step's `max_tokens`) at which a step compacts first, raised to 0.75 for a window under 512000 tokens and floored at 64000 tokens unless that leaves no room; a decimal number as `target_ratio`. Default 0.3. |
  | `engine.compression.threshold_tokens` | `ULEI_ENGINE_COMPRESSION_THRESHOLD_TOKENS` | Prompt tokens at which a step compacts first at the latest, whatever the share gives; a non-negative integer, `0` sets no cap. Default 300000. |
  | `engine.routes.<alias>.context_length` | `ULEI_ENGINE_ROUTES_<ALIAS>_CONTEXT_LENGTH` | Context length in tokens of the model behind the route alias, a positive integer. Set: a step whose prompt, as the last step's usage reports it (`input` + `cache_read` + `cache_write`), reaches the threshold compacts the session first. Unset: only a `context_too_long` answer compacts, sized for a 256000-token window. |
  | `engine.routes.<alias>.max_tokens` | `ULEI_ENGINE_ROUTES_<ALIAS>_MAX_TOKENS` | `max_tokens` of every step on the route alias, which the window reserves for output; a positive integer. Default 16384. A compaction summary turn always reserves 16384. |
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
the step `s1-t1-0-<tag>` (12 hex digits) with the route and usage qb reported:

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

### 7. Repeated request id numbers

The request id of a step is `s<session>-t<turn>-<step>-<tag>`: the numbers come from journal
state, and the tag is drawn at random for each run of a turn. qb sees the same numbers again in
two cases.

Crash before the append: start a long turn and kill the process with SIGKILL while it
streams (the turn is not appended; Ctrl-C would append it), then run again.

```sh
<bin> run --session crash --route <route> --prompt 'Count slowly from 1 to 300, one per line' &
# while it streams
kill -KILL $!
<bin> run --session crash --route <route> --prompt 'Reply with the word: again'
```

Two journal files: point a second journal at the same qb.

```sh
ULEI_ENGINE_JOURNAL=<state-dir>/b/journal.sqlite3 \
  <bin> run --session other --route <route> --prompt 'Reply with the word: second'
```

Expected: both runs exit 0 with their word. The killed turn and the rerun both send
`sNN-tMM-0-<tag>` with the same numbers and different tags (the rerun's id in the journal's
`steps` is the next turn after the last appended one, so the killed request carried its numbers
too; qb's audit log shows both ids), and the run on journal `b` sends `s1-t1-0-<tag>`, the
numbers of check 1 under another tag. qb answers each as a new turn: no `bad_request` for an
id already running, and no replay of the earlier answer.

### 8. SIGINT during a tool call

```sh
<bin> run --session stop --route <route> --prompt 'Run this shell command and report its output: sleep 300; echo done' &
# once `pgrep -f 'sleep 300'` prints a pid
kill -INT $!; wait $!; echo "exit $?"
pgrep -f 'sleep 300' || echo gone
<bin> run --session stop --route <route> --prompt 'Did the last command finish? Reply in one line.'
```

Expected: exit 130 with the one stderr line `ulei-engine: the turn was interrupted`, then
`gone`: the tool's process group did not outlive the engine. The journal holds the interrupted
turn's prompt, tool call and `[Command interrupted]` result, and the second run exits 0 with an
answer about the interrupted command: the next turn runs in the same history.

### 9. Cached tokens in `usage`

The engine takes `input` + `cache_read` + `cache_write` of a step's `usage` as the size of its
prompt (the compaction threshold, the empty-reply guard), so the three counts must not overlap:
C4G `Usage.input` counts the input tokens not read from the cache. Use a route with a single
login whose vendor caches prompts, and run the two turns within the cache lifetime:

```sh
<bin> run --session cache --new --route <route> --prompt 'Reply with the word: one'
<bin> run --session cache --route <route> --prompt 'Reply with the word: two'
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT request_id, json_extract(usage, '$.input'), json_extract(usage, '$.cache_read'),
     json_extract(usage, '$.cache_write'), json_extract(usage, '$.input')
     + json_extract(usage, '$.cache_read') + json_extract(usage, '$.cache_write')
   FROM steps WHERE session = (SELECT max(id) FROM sessions WHERE key = 'cache') ORDER BY turn"
```

Expected: the second step reads `cache_read` above 0 and an `input` far below its sum, and its
sum is at least the first step's and larger by about the tokens of the first reply and the
second prompt. A second sum below the first, or an `input` that alone is about the whole
prompt while `cache_read` is large, means the counts overlap: a defect against C4G. With
`cache_read` and `cache_write` 0 in both steps nothing was cached and the check is
inconclusive; record that.

### 10. Compaction on the compression route

Needs `<compact-route>` in qb (`GET /v1/model/routes` lists it). Start a session and read the
prompt size of its step, the last column:

```sh
<bin> run --session compact --new --route <route> --prompt 'Reply with the word: start'
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT request_id, json_extract(usage, '$.input') + json_extract(usage, '$.cache_read')
     + json_extract(usage, '$.cache_write')
   FROM steps WHERE session = (SELECT max(id) FROM sessions WHERE key = 'compact') ORDER BY turn"
```

Add to the config file, with `<threshold>` that size plus 2000 and `<route>` written out:

```toml
[engine.compression]
route = "<compact-route>"
protect_last_n = 3
threshold_tokens = <threshold>

[engine.routes.<route>]
context_length = 200000
```

With these settings the threshold is `<threshold>`: the share of the window,
(200000 - 16384) * 0.75 = 137712 tokens, is above it. Then run four filler turns of 1000
tokens or more each (every number is at least one token) and a last turn:

```sh
for n in 1 2 3 4; do
  <bin> run --session compact --route <route> \
    --prompt "Filler $n: $(seq -s ' ' 1 1000). Reply with the word: noted"
done
<bin> run --session compact --route <route> --prompt 'Reply with the word: done'
```

Expected: every run exits 0 and the last one prints `done`. A turn compacts before its first
step when the last step's prompt size reached `<threshold>`; a first compaction also needs
more than 7 messages, the 3 kept at the head, at least 3 kept at the tail and one between.

- The step of the third or the fourth turn reaches `<threshold>`. The fourth turn starts with 7
  messages and does not compact.
- The fifth turn (filler 4) compacts: it keeps `start`, its reply and filler 1 at the head,
  at least filler 3, its reply and filler 4 at the tail, and replaces the messages between
  with one summary message. Its step's prompt size stays above `<threshold>`, since the kept
  head and tail alone hold three fillers.
- So the last turn compacts again. A summary now opens the history, so nothing is kept at the
  head: the earlier summary, `start` and the messages up to the last 3 or more go into one new
  summary, which updates the earlier one. That summary may open the first kept message rather
  than stand in a message of its own.

```sh
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT seq, turn, active, compacted, instr(message, '[CONTEXT COMPACTION') > 0,
     instr(message, 'deterministic fallback') > 0
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'compact') ORDER BY seq"
```

Each compaction archives the live rows (`active` 0, `compacted` 1), and the rows a turn
writes live carry that turn's number. After the last turn:

- one archived row of turn 5 reads 1 in the fifth column: the first summary;
- every live row (`active` 1, `compacted` 0) is of turn 6, exactly one of them reads 1 in the
  fifth column, and the last one is the reply `done`;
- both summary rows read 0 in the sixth column: the summary turns answered on
  `<compact-route>`.

In qb, each summary turn picks with its own `run_id` `compact-<16 hex>` (it has no
`session_key`), while the session's steps pick with `run_id` `compact`. A sixth column of 1
means the summary turn failed and the engine wrote the deterministic fallback summary, for
example because qb refused an unknown `<compact-route>` with `bad_request`; a summary turn
that failed on `auth`, `quota_exhausted`, its timeout or the connection leaves the session
uncompacted instead. Remove the added sections before the next check.

### 11. A skill

Write `<brain-dir>/skills/smoke/smoke-skill/SKILL.md`:

```markdown
---
name: smoke-skill
description: Holds the code word of the live smoke.
---

# Smoke skill

The code word is amber-lantern-42.
```

```sh
<bin> run --session skill --new --route <route> \
  --prompt 'Load the skill smoke-skill with skill_view and reply with its code word only'
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT instr(system_prompt, 'smoke-skill') > 0 FROM sessions WHERE key = 'skill'"
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT seq, json_extract(message, '$.role'), json_extract(message, '$.content[0].type'),
     json_extract(message, '$.content[0].name'), json_extract(message, '$.content[0].input.name')
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'skill') ORDER BY seq"
```

Expected: exit 0 and stdout `amber-lantern-42`; the session's system prompt indexes the skill
(1); the journal holds a `tool_use` of `skill_view` with the name `smoke-skill`, then its
`tool_result`. With `ULEI_ENGINE_DISABLED_SKILLS=smoke-skill` and `--new` the new session's
system prompt leaves the skill out, a `skill_view` call of it gets an error result naming it
disabled, and no code word is printed.

### 12. An MCP tool

Run a Streamable HTTP MCP server on `<mcp-url>`. Add to the `[engine]` section of the config
file `mcp_servers = "smoke"`, and append:

```toml
[engine.mcp.smoke]
url = "<mcp-url>"
```

When the server needs request headers, put them into an env-file of mode 0600 with
`env_file = "<state-dir>/mcp-smoke.env"` in that section; use a synthetic value such as
`Authorization=Bearer smoke-token-0001`.

```sh
<bin> run --session mcp --new --route <route> \
  --prompt 'Call the tool mcp__smoke__<mcp-tool> once and report its result in one line'
echo "exit $?"
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT seq, json_extract(message, '$.content[0].type'), json_extract(message, '$.content[0].name'),
     substr(json_extract(message, '$.content[0].content'), 1, 80)
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'mcp') ORDER BY seq"
sqlite3 <state-dir>/a/journal.sqlite3 .dump | grep -c smoke-token-0001
```

Expected: exit 0, stderr empty, stdout reports the tool's answer; the journal holds a
`tool_use` of `mcp__smoke__<mcp-tool>` (every character outside `[A-Za-z0-9_]` of the tool name
as `_`) and its `tool_result` with the server's answer; the header value appears nowhere in the
journal (count 0). Stop the server and run the prompt again: stderr holds the one line
`ulei-engine: Failed to connect to MCP server 'smoke': <reason>`, the turn runs without the
server's tools and exits 0. Remove the MCP settings before the next check.

### 13. `set_route` over C6, then a turn on the new alias

Add to the `[engine]` section of the config file `c6_socket = "<c6-socket>"`,
`owner_session = "owner"` and `default_route = "<route>"`, then start `serve` and call C6 with
curl:

```sh
<bin> serve &
# stdout: ulei-engine ready <c6-socket>
c6() { curl -sS --unix-socket <c6-socket> -H 'Content-Type: application/json' -d "$2" "http://localhost/v1/$1"; echo; }
c6 submit_turn '{"session": "switch", "prompt": "Reply with the word: first", "new": true}'
c6 status '{"session": "switch", "turn": "<turn of the answer>"}'
c6 set_route '{"session": "switch", "route": "<route-b>"}'
c6 status '{"session": "switch"}'
c6 submit_turn '{"session": "switch", "prompt": "Reply with the word: second"}'
c6 status '{"session": "switch", "turn": "<turn of the answer>"}'
```

Repeat each `status` with a turn until its `turn.state` is `ended`. Then:

```sh
c6 set_route '{"session": "switch", "route": "nosuch-route"}'
c6 submit_turn '{"session": "switch", "prompt": "Reply with the word: third"}'
c6 status '{"session": "switch", "turn": "<turn of the answer>"}'
kill %1
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT key, route FROM sessions WHERE key = 'switch'"
sqlite3 <state-dir>/a/journal.sqlite3 \
  "SELECT request_id, json_extract(route, '$.vendor'), json_extract(route, '$.model') FROM steps
   WHERE session = (SELECT max(id) FROM sessions WHERE key = 'switch') ORDER BY turn"
```

Expected: both first turns end `completed`; `set_route` answers
`{"session": "switch", "route": "<route-b>"}` and the `status` after it reads `route`
`<route-b>`. The second turn's step names the vendor and model `GET /v1/model/routes` gives
for `<route-b>`, the first turn's those of `<route>`. The alias qb does not know is stored:
the turn after it ends `failed` with a `line` naming `bad_request`, and the journal's session
row reads `nosuch-route`, which stays until the next `set_route`.
