# ADR-0023: Engine no-ask-guard policy

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The owner's profile plugin `no-ask-guard` and its classifier script `scripts/ask_guard.py` hold
back a final reply of the main-platform session that ends with a needless question to the owner,
and hand the turn back to the agent with a fixed note. The plugin works through gateway hooks:
`pre_llm_call` remembers the turn and the owner's message, `transform_llm_output` classifies the
reply, `ctx.inject_message` queues the note as a new turn of the same session, and the silence
marker `NO_REPLY` keeps the reply from the owner. The engine has no gateway and runs one owner
prompt per `ulei-engine run`. The engine ports the mechanism; every text the model sees comes from
files the deployment names, and the classifier is a tool-less call through qb.

## Decision

### 1. Mechanism mapping

| Plugin and gateway | Engine |
|---|---|
| `pre_llm_call` `on_turn_start`, once per gateway turn | `Policy::on_turn_start`, once per `Agent::run` with the turn's origin and input message, before the turn context is appended |
| `transform_llm_output` `on_output` on the final response | `Policy::on_stop` on the final reply of the turn: a stop for `end`, or for `tool_use` without a call once its re-prompts are spent, and the text of a housekeeping step that answers an empty reply (ADR-0016 section 7) |
| `ctx.inject_message(note, session_key)`: an internal event queued behind the turn, run as the next turn of the gateway session | `StopDecision::Continue(note)`: the loop appends the note as a user message and runs the next step in the same `ulei-engine run` |
| `NO_REPLY` replaces the reply, the gateway does not send it, the transcript keeps it | the held-back reply stays in the turn's messages and the journal; `ulei-engine run` prints the reply that ends the turn |
| the gateway session key, `session_id` in the records | the journal session key (`--session`), `session_id` in the records |

### 2. Settings and texts

- `engine.no_ask_guard.route` (`ULEI_ENGINE_NO_ASK_GUARD_ROUTE`) names the classifier's qb route
  alias. Set, the policy is on and `engine.no_ask_guard.note_file`,
  `engine.no_ask_guard.instructions_file` and `engine.no_ask_guard.log` are required; each file
  is read when a turn starts. Unset, the policy is off and no other `engine.no_ask_guard.*` setting is
  read. Every `engine.no_ask_guard.*` setting is a key of the required `ulei.toml`, read through
  `ulei_contracts::config` (ADR-0040), and its `ULEI_ENGINE_NO_ASK_GUARD_*` variable wins over it.
- The note file is the whole continuation message, its prefix included, with `{chain}`
  placeholders. The instructions file holds the classifier instructions with their examples and
  the placeholders `{tag}`, `{owner}` and `{reply}`. Both are read as Python's `str.format` reads
  the plugin's constants: `{{` and `}}` are braces, so the deployment supplies the plugin's texts
  unchanged. An unset, missing or unreadable file, an unknown placeholder, an unbalanced brace
  or a missing placeholder fails the start with an error naming the setting.
- The engine carries no text of its own; tests use synthetic texts.

### 3. Classifier call

- Pre-filter as `ends_with_question` and `_strip_markup`: code blocks and URLs removed, a text
  holding `MEDIA:` never asks, and a `?` in the last 600 characters of the last two non-blank
  lines is a question. Without one no call is made.
- Input as `prepare`: the end of the owner's request (`tail_of`, 1800 characters) and the
  stripped reply in parts of at most 8000 characters, every part but the last followed by
  `\n…\n` and the reply's last 600 characters.
- One request per part: `POST /v1/model/turn` on the configured alias with one user message, the
  rendered instructions, a fresh `⟦DATA-<12 hex>⟧` tag per part; no system prompt, no tools, no
  session key, no effort, `max_tokens` 16384 and the request id `ask-<16 hex>`. A tag that
  occurs in the data makes the verdict `legit`. The call is not a step of the turn and is not
  journaled.
- Verdict as `_parse_obj` and `combine`: the JSON object from the first `{` to the last `}`
  with `verdict` `extra` or `legit`, `category` cut to 20 and `reason` to 200 characters; any
  `legit` part wins. Any failure (a gateway error, a quota answer, a cut or broken stream, no
  verdict, the turn's interrupt) is the verdict `error`. A call past
  `engine.no_ask_guard.timeout` seconds (default 45, `TIMEOUT`) fails with a timeout and is
  cancelled at qb.
- Nothing the classifier returns enters the note.

### 4. Chain, cap and scope

- On a `TopLevelOwner` turn (ADR-0022) the chain is the owner's input: its text is the
  classifier's owner context for every reply of the turn, and its count starts at 0. Any other
  origin (another channel, an event, a sub-session) is never classified.
- An `extra` verdict with a count below 2 (`MAX_CONSECUTIVE`) draws a 16-hex chain id from
  `/dev/urandom`, writes the `suppressed_continue` record and continues with the note rendered
  with that id; the count rises by one. At 2 the reply is delivered (`pass_loop_cap`). A `legit`
  or `error` verdict delivers it (`pass`). A held-back reply whose record cannot be written is
  delivered.
- A reply that ends a continuation, delivered for any reason, writes `continuation_done` with
  the chain id of the suppression it answers.
- Only the policy's own `Continue` continues a chain, and every new input starts a new one, so
  text that imitates a note is an ordinary input with a fresh count.

### 5. Log

The engine has no log of its own: stdout carries the answer, stderr one line on failure, and the
journal only the conversation. The records therefore go, as in the plugin, to a JSON-lines file:
`engine.no_ask_guard.log` (`ULEI_ENGINE_NO_ASK_GUARD_LOG`), appended and synced per record, its
directory created. Fields: `ts`, `session_id`, `action` (`pass`, `pass_loop_cap`,
`suppressed_continue`, `continuation_done`), and on a verdict `reply_tail` (300 characters),
`ms`, `verdict`, `reason`, `category`, `parent`, `chain` and `streak` as the plugin writes them.

### 6. Differences from the plugin

| Behaviour | Upstream | Why |
|---|---|---|
| The note runs as a new gateway turn after the queue; the reply is replaced by `NO_REPLY` | `on_output`, `_check`, `ctx.inject_message` | Section 1: the engine has no gateway queue; `Continue` runs the note in the same run, and the reply that ends the turn is the one printed. |
| A refused injection, a missing session key or a missing injection grant delivers the reply with `pass_inject_failed` | `_check`, `register` | `Continue` is never refused. `pass_inject_failed` is written only when no chain id could be drawn. |
| The continuation starts a fresh gateway turn with its own iteration budget | `ctx.inject_message` | The continuation shares the run's `engine.max_steps`; a reply held back on the last step is followed by the summary request, whose answer ends the turn. |
| Chain ids live in a process-wide map, are bound to the session key, used once and found in the incoming message by `CONT_RE` | `on_turn_start`, `_chains` | The chain lives in the policy for one turn and only its own `Continue` continues it, so no id is parsed from text and no map is pruned. |
| Scope: the platform `MAIN_PLATFORM` and no `parent_session_id` | `on_turn_start` | The turn origin rule of ADR-0022. |
| Off switch: disabling the plugin or `NO_ASK_GUARD_DISABLED=1` | `register`, `on_output` | The setting `engine.no_ask_guard.route`. |
| The classifier runs as a subprocess through a one-shot agent of the profile with the `context_engine` toolset and a provider fallback chain (`ASK_GUARD_LLM_CHAIN`) | `oneshot`, `CHAIN`, `PROFILE` | Models only through qb: one tool-less request on the configured alias; fallback is qb's route. `context_engine` is on the port map's not-ported list. |
| The timeout kills the subprocess | `oneshot` | A deadline on the answer stream, then `POST /v1/model/cancel`. |
| The classifier call blocks the turn until it answers or times out | `oneshot`, `on_output` | The engine's interrupt ends any step of the turn (ADR-0016 section 7): the turn's interrupt cancels the open call at qb at once, its verdict is `error`, no continuation is issued, and the turn ends with the delivered reply as when no policy continues. |
| `PROMPT` and `CONTINUE_NOTE` are constants in code | `ask_guard.py`, `__init__.py` | The texts come from the configured files (section 2). |
| A continuation whose reply is empty or a silence marker writes `continuation_empty` | `on_output`, `_delivered` | An empty reply ends the turn with `(empty)` before the stop policies run, so the policy never sees one; such a continuation, like one cut by an error or an interrupt, leaves its suppression without `continuation_done`. |
| Records carry `reply_sha` (16 hex of SHA-256) and a local ISO time | `_check`, `_write_log` | The engine links no SHA-256 (it would be a new dependency), so `reply_tail` identifies the reply; `ts` is `YYYY-MM-DD HH:MM:SS UTC`, the engine's time format. |
| An error verdict's reason is `<exception class>: <message>`; a non-string `category` or `reason` is Python's `str` of it | `classify`, `_parse_obj` | The reason is the engine's error text cut to 150 characters; a non-string field is its JSON text. |
| A hook failure or a failed record write goes to the Python logger | `on_turn_start`, `on_output`, `_write_log` | The engine has no log; a failed suppression record still delivers the reply. |
| The reply classified is the response with reasoning blocks stripped | `on_output` `response_text` | The engine classifies the text blocks of the final reply, the text `ulei-engine run` prints. |

### 7. Not ported

- The nightly ledger metric and orphan report (`scripts/commitment_ledger.py`, `classify_many`,
  `BATCH_PROMPT`): owner tooling outside the engine. The records of section 5 keep what it reads.
- The script's built-in examples and its command line (`EXAMPLES`, `main`).

## Consequences

- A suppression costs one classifier request per reply part and one more model step.
- The deployment's config names its note and instructions files; the repository holds
  neither.
- The C6 server keeps the mapping: one `Agent::run` per owner prompt, the policy in its chain.
