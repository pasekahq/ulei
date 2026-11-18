# ADR-0016: Engine tool surface: what stays different from the reference

Status: accepted
Date: 2026-11-17
Affects: crates/engine

## Context

The built-in tools of `ulei-engine` port the tool surface of the reference implementation
(`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3): the names `terminal`, `read_file`,
`write_file`, `patch` and `search_files`, their parameter sets and descriptions, and the error
texts the model sees. The step loop around them ports the end of a turn at the step limit and
the recovery from empty replies. AGENTS.md allows a difference from the reference only when it
is recorded with the upstream module and the reason. This record lists the differences that
stay.

## Decision

### 1. Parameters that are not advertised

| Parameter | Upstream | Why |
|---|---|---|
| `terminal.background`, `terminal.notify_on_complete`, `terminal.watch_patterns`, and the background guidance texts: the refusal of a foreground command that uses `nohup`/`disown`/`setsid`, `&` or starts a long-lived server, the `hint` of a background call without `notify_on_complete`, `notify_unsupported`, `watch_patterns_ignored` | `tools/terminal_tool.py` `TERMINAL_SCHEMA`, `_foreground_background_guidance`, `_resolve_notification_flag_conflict`, `terminal_tool`; `tools/process_registry.py` | The engine has no process registry and no `process` tool; every call ends with its process group killed, so a command with `&` or a server is not refused: it runs under the call's timeout and nothing it started outlives the call. The parameters and the texts come back together with the `process` tool, which is used but outside the current port and decided later. |
| `terminal.pty` | `tools/terminal_tool.py` `TERMINAL_SCHEMA` | The engine runs commands on pipes only; a pseudo-terminal needs its own allocation and reader. |
| `write_file.cross_profile`, `patch.cross_profile` | `tools/file_tools.py` `WRITE_FILE_SCHEMA`, `PATCH_SCHEMA`, `_check_cross_profile_path` | The guard protects other agent profiles on one host; ulei has no profiles. |

A call that sends one of them is not rejected: like the reference handlers, the tools ignore
fields they do not know. The description sentences and error clauses that point to these
parameters (`Background: ...`, `PTY: ...`, "Use background=true ..." in the timeout error) are
left out of the advertised texts; the `timeout` parameter description is verbatim, its closing
clause naming `background=true` included. The missing-content error of `write_file`
(`tools/file_tools.py` `_handle_write_file`) leaves out its clause pointing to `execute_code`,
which is not an engine tool.

### 2. Behaviour that is not ported

| Behaviour | Upstream | Why |
|---|---|---|
| Shell state between `terminal` calls: the working directory and exported variables persist, and a result carries a `cwd` field after `cd` | `tools/environments/local.py`, `TERMINAL_TOOL_DESCRIPTION` | Each call is a fresh `bash -c` in its own process group, killed when the call ends, so an interrupt leaves nothing behind. The sentences about persistence are left out of the description. |
| Syntax checks of `write_file` and `patch`: the refusal of JSON, YAML and TOML content that does not parse, the lint and LSP diagnostics after the write, the `lint` and `lsp_diagnostics` result fields | `tools/file_operations.py` `write_file`, `_FAIL_CLOSED_INPROC_EXTS`, `_check_lint_delta`; `tools/patch_parser.py` `apply_v4a_operations` | They run parsers, external linters and language servers; the engine starts none and carries no YAML or TOML parser (ADR-0010 budgets). The `patch` description leaves out its sentence `Auto-runs syntax checks after editing.` Without them `write_file` reads no pre-write text of its own, so the line ending of the file it replaces comes from its first 4096 bytes, as the reference finds it for an extension it neither lints nor hands to a language server. |
| Write guards of `patch`: the deny list of system and credential files and the safe write root, `_check_sensitive_path`, the refusal of text written into binary documents, the approval prompts for protected instruction files and approval-gated paths | `agent/file_safety.py` `get_write_denied_error`, `is_write_approval_required`; `tools/file_tools.py` `_check_sensitive_path`, `_check_binary_document_write`, `_check_protected_instruction_write`, `_check_approval_required_write` | The engine has no approval channel to wait on a human, and its path protection is the in-loop policy chain a deployment configures (`StateDbGuard`). |
| A file that is not UTF-8 text: `patch` decodes it with U+FFFD for each undecodable byte, edits that text and writes it back | `tools/file_operations.py` `patch_replace`, `read_file_raw`; `tools/patch_parser.py` `_apply_update`; `tools/environments/local.py` | Writing the replacement characters back destroys the bytes they stand for. The engine answers `Failed to read file: <path>` and writes nothing; in `mode='patch'` that is the validation error of the Update, and no operation of the patch is applied. A Delete or Move reads such a file with U+FFFD as the reference does. |
| `patch` arguments of another JSON type than the schema's: `replace_all` taken by truthiness, `mode: null` answered `Unknown mode: None`, other types failing inside the handler | `tools/file_tools.py` `_handle_patch`, `patch_tool` | Truthiness would turn `"false"` into a replace-all. The engine refuses a non-string `path`, `old_string`, `new_string` or `patch` and a non-boolean `replace_all` with ``field `<name>` must be a string`` or `a boolean`, treats a `null` field as absent, and writes a non-string mode as JSON. |
| Move and Delete of a V4A patch through `mv` and a `python3 -c` snippet in the terminal backend | `tools/file_operations.py` `move_file`, `delete_file`, `_python_delete` | The engine renames (copying across file systems) and unlinks in process. A failed Move answers `mv: cannot move '<src>' to '<dst>': <reason>` and a failed Delete `[Errno N] <reason>: '<path>'`, the texts those programs print for an operating system error; their other messages are not reproduced. Validation runs first, so these fail only when the files change between validation and apply. |
| V4A header paths found by a multi-line pattern over the whole patch, written back into the patch text and parsed again | `tools/file_tools.py` `patch_tool`, `_rewrite_v4a_patch_paths_for_host`; `agent/tool_dispatch_helpers.py` `_extract_file_mutation_targets` | The engine matches each header on its own line, in the tool and in the batch planner, and resolves the paths it parsed. Parsing the rewritten `*** Move File: <src> -> <dst>` again splits at the first `->`, so a working directory whose path holds `->` cuts the source short. The reference pattern lets the whitespace after `File:` cross a line break, so a header with an empty path takes the next line as its path for the `..` refusal, the reported paths and the rewrite, and the parse then answers `Operation with empty file path`; the engine resolves the empty path to the working directory, and its batch planner holds no path for that header. |
| The per-path lock around a `patch` and the note that verification evidence of the workspace is stale | `tools/file_state.py` `lock_path`, `tools/file_tools.py` `_mark_verification_stale` | The batch planner never runs a writer together with a call on an overlapping path, the engine runs no sibling agents, and it keeps no verification evidence. |
| Document conversion in `read_file` (notebooks, Office, PDF, EPUB) and the hand-off of images to `vision_analyze` | `tools/file_tools.py` `read_file_tool`, `tools/read_extract.py`, `tools/file_operations.py` `read_file` | No converters in the engine binary (ADR-0010 budgets); `vision_analyze` is not an engine tool. The `read_file` description and its binary refusal keep the reference texts that name both; a document is read as text or refused as binary. |
| `read_file` without `limit` reads 500 lines | `tools/file_tools.py` `_handle_read_file` | The handler passes 500 when the field is absent, below the default of 2000 that the schema advertises and `read_file_tool` declares; the engine applies the advertised 2000. |
| The read path through the terminal backend: `sed`, `cut`, `wc` and `base64` over the shell, `_strip_terminal_fence_leaks` on what comes back, `ls` for the not-found suggestions | `tools/file_operations.py` `read_file`, `_sample_file_bytes`, `_try_read_utf16`, `_suggest_similar_files` | The engine reads the file itself, so its bytes reach the model without fence or OSC stripping. The suggestions look at the first 50 names in byte order where `ls` sorts by the locale's collation, so names with equal scores can come in another order. |
| A unicode-equivalent spelling of a missing file name (NFC/NFD, narrow no-break space, curly quotes) is read instead | `tools/file_operations.py` `_unicode_variant_match` | Needs Unicode normalisation tables the engine binary does not carry (ADR-0010 budgets); such a path is reported as not found with similar names. |
| The not-found cache of `read_file` and `search_files` (60 s) | `tools/file_tools.py` `_check_not_found_cache`, `_record_not_found` | It saves the shell round trips of a repeated miss; the engine looks the path up in process. A repeated miss runs again and carries the repeat warning, which the reference leaves off an answer it takes from the cache. |
| Read blocks of the agent home: credential stores, the MCP token directory, the skills hub cache | `agent/file_safety.py` `get_read_block_error` | The engine has no agent home holding them. The project environment-file names are blocked; the search walk skips them as hidden entries, so the `_omitted` filter of `search_files` only removes what a future walk could return. |
| Warnings of `write_file` and `patch` (`_warning`): the file changed since it was read, a sibling agent wrote it, a relative path resolved outside the workspace; the per-path read stamps and the cross-agent registry behind them | `tools/file_tools.py` `_check_file_staleness`, `_update_read_timestamp`, `_path_resolution_warning`, `tools/file_state.py` | The engine runs no sibling agents, and relative paths resolve against the session working directory where every `terminal` call also starts, so the workspace divergence cannot arise. The change-since-read warning is advisory, the write proceeds in both; the tracker keeps the dedup stamps of read regions only. |
| Clearing the `read_file` dedup stamps when the context is compressed, and the summary of files read | `tools/file_tools.py` `reset_file_dedup`, `get_read_files_summary`; `agent/conversation_compression.py` | The engine does not compress context; a dedup stamp lives for the session in the engine process. |
| Unicode decimal digits outside ASCII in `offset`, `limit` and in the line-number prefixes `write_file` refuses | `tools/file_operations.py` `_coerce_int`, `tools/file_tools.py` `_looks_like_read_file_line_numbered_content` | Python's `int()` and `str.isdigit` take every Unicode decimal digit; the engine takes ASCII digits, so such a value falls back to the default and such a prefix does not count. |
| ripgrep as the `search_files` backend | `tools/file_operations.py` `_search_with_rg`, `_search_files_rg` | Regular expressions use `regex-lite` (ADR-0013) over the engine's own walk. Consequences below. |
| Hints on a search with no match: case-insensitive, hidden-file and fixed-string probes | `tools/file_operations.py` `_zero_match_probe` | Each probe scans the tree again; the scan bounds of section 4 would apply three more times per empty search. |
| A `path` holding several paths separated by spaces or commas | `tools/file_operations.py` `_try_multi_path_search` | Not ported; such a path is reported as not found with similar names. |
| A `workdir` that does not exist: the wrapper's `cd` fails and the call ends with the shell's message and exit code 126 | `tools/environments/base.py` `_wrap_command` | The engine runs no wrapper script (see the shell state row); it starts the shell in that directory, so the start fails and the result is `Command execution failed: FileNotFoundError: [Errno 2] No such file or directory: '<directory>'` with `exit_code` -1. |
| Three retries of a `terminal` call whose start failed, after 2, 4 and 8 s | `tools/terminal_tool.py` `terminal_tool` | A local start failure (missing directory, missing shell) does not heal within the backoff, which would hold the turn for 14 s; the failure is answered at once. |
| `terminal.timeout` (`TERMINAL_TIMEOUT`), `tool_output.max_bytes`, `tool_output.max_lines`, `tool_output.max_line_length` and `file_read_max_chars` settings | `tools/terminal_tool.py` `_get_env_config`, `tools/tool_output_limits.py`, `tools/file_tools.py` `_get_max_read_chars` | The engine config has no per-tool settings; it uses the reference defaults: a 180 s timeout, a 50,000-character output cap, 2000 lines of at most 2000 characters and 100,000 characters per read. |
| Environment of the `terminal` process: provider credentials removed, sane `PATH` entries added, the `HOME` contract of `terminal.home_mode` applied | `tools/environments/local.py` `_make_run_env` | The process inherits the engine's environment unchanged. Its `HOME` is the engine's own, the real home that `home_mode real` selects: the engine has no profile home to repair from. The engine reads no vendor credential (ADR-0009), so it has none to remove. |
| The `verification_evidence` field of a `terminal` result and the store of test and build runs behind it | `agent/verification_evidence.py` `record_terminal_result`, `agent/verification_stop.py`, `tools/terminal_tool.py` `terminal_tool` | Its only consumer is the verify-on-stop policy, and that policy is off for the secretary: `verify_on_stop_enabled` defaults to off, the secretary profile does not set `agent.verify_on_stop`, and even the `auto` value turns it off on a messaging surface. The evidence would be written to a new store that nothing reads. It returns only if the owner asks for verify-on-stop. |
| sudo handling: `sudo` rewritten to read a password from `SUDO_PASSWORD` or a prompt, the per-session password cache and its clearing on a wrong password, the failure tip, the `sudo_auth_failed` and `sudo_cache_cleared` result fields | `tools/terminal_tool.py` `_transform_sudo_command`, `_handle_sudo_failure`, `_sudo_wrong_password_failure`, `_invalidate_cached_sudo_on_auth_failure`, `_sudo_password_cache` | Dropped. The prompt path needs an interactive user; the `SUDO_PASSWORD` path would put a password into the engine's environment; the failure tip tells the model to set a variable that ulei does not have. A `sudo` command runs as written and returns the system's own error. Whether the live secretary relies on sudo is a deploy check: issue:6c58239734285dfd6357. |
| Redaction patterns registered by plugins | `agent/redact.py` `register_redaction_patterns` | The engine loads no plugins; the built-in token shapes are the whole set. |
| A warning at startup when `security.redact_secrets` is off | `gateway/run.py`, `cli.py` `_log_redaction_status` | The engine has no log; the setting is read where the tools are built. |

### 3. Tool batches

The calls of one model step run as `agent/tool_dispatch_helpers.py` `_plan_tool_batch_segments`
plans them and as `agent/tool_executor.py` `execute_tool_calls_concurrent` runs a parallel
segment: at most 8 worker threads, a 420 s deadline, results in call order. These parts differ:

| Behaviour | Upstream | Why |
|---|---|---|
| MCP tools from a server that allows parallel calls join a parallel run | `agent/tool_dispatch_helpers.py` `_is_mcp_tool_parallel_safe`, `tools/mcp_tool.py` `is_mcp_tool_parallel_safe` | The engine has no MCP client yet; every tool outside the parallel-safe set is a barrier. It comes with the MCP card. |
| Multimodal results: the `_multimodal` envelope, its text summary and the subdirectory hint appended to it | `agent/tool_dispatch_helpers.py` `_is_multimodal_tool_result`, `_multimodal_text_summary`, `_append_subdir_hint_to_multimodal` | No engine tool returns images; every result is text. |
| Untrusted-output wrapping: `<untrusted_tool_result>` around the results of `web_*`, `browser_*` and `mcp_*` tools, the upstream-elision notice and the risk metadata | `agent/tool_dispatch_helpers.py` `make_tool_result_message`, `_maybe_wrap_untrusted`, `_maybe_append_elision_notice`, `_tool_output_risk_metadata` | It applies only to those tools, and none is an engine tool; it comes with the web and MCP tools. |
| A call past the deadline is abandoned: its daemon thread keeps running detached, gets the interrupt flag, and the turn goes on at once | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `tools/daemon_pool.py` | Workers are scoped threads (ADR-0010: no thread outlives its segment, none exists at idle). Calls past the deadline get the reference text `Error executing tool '<name>': timed out after <s>s` and no further call of the segment starts, but the segment returns when the running calls end. The parallel-safe engine tools are bounded file operations; `search_files` stops at the scan bounds of section 4 and at an interrupt. A worker stack is 2 MiB. |
| The start-order gate and the authorization gate of a parallel segment: dispatch serialised by call order, approval prompts one at a time, their 120 s and 360 s bounds | `agent/tool_executor.py` `_begin_in_order`, `_ConcurrentToolAuthorizationGate`, `_START_ORDER_GATE_TIMEOUT_S`, `_AUTHORIZATION_GATE_LOCK_TIMEOUT_S` | The policy chain is not thread-safe and has no approval prompt: the loop thread runs the pre hooks of every call of the segment in call order before the first call starts, and the post hooks in call order after the segment, where the reference runs each post hook when its call ends. |
| A call not started when the interrupt fires answers `[Tool execution cancelled — <name> was skipped due to user interrupt]` | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `execute_tool_calls_sequential` | The engine answers `interrupted before the tool ran`, the text of every call after an interrupt on the sequential path too. A running `search_files` stops at the interrupt; `terminal` is never in a parallel segment, so its process group is killed as on the sequential path. |
| The concurrency cap of `image_generate` (`image_gen.max_parallel_requests`, default 4) | `agent/tool_executor.py` `_image_generate_parallel_limit`, `_max_workers_for_tool_batch` | The engine has no image tool; the cap comes with it. |
| The heuristic for destructive `terminal` commands and the file checkpoint taken before them and before `write_file` and `patch` | `agent/tool_dispatch_helpers.py` `_DESTRUCTIVE_PATTERNS`, `_is_destructive_command`; `agent/tool_executor.py` `_ensure_file_checkpoint` | It only decides checkpoints, and the engine has no checkpoint manager. In the planner `terminal` is a barrier whatever its command, as in the reference. |
| The deadline setting: `timeouts.tools.concurrent_batch` in the config file winning over `HERMES_CONCURRENT_TOOL_TIMEOUT_S`; an invalid value logged and skipped | `agent/tool_executor.py` `_resolve_concurrent_tool_timeout`, `agent/deadline.py` `resolve_timeout`, `clamp_timeout` | The engine reads `engine.concurrent_tool_timeout`, overridden by `ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT` as every engine setting is. The values are the reference's: seconds, `0` or less or `nan` for no deadline, at most 365 days. A value that is not a number fails the run with an error naming the setting, since the engine has no log to carry a warning. |
| `~user` in a path the planner compares | `agent/tool_dispatch_helpers.py` `_canonical_path` | The planner resolves a path as the engine tools do, which expand only `~` and `~/`. |
| Spinner, progress lines, activity heartbeats, the post-call hook events with their status and duration, and the session flush after each tool | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `_flush_session_db_after_tool_progress` | The engine has no display, inactivity monitor or plugin hooks; it journals a turn when the turn ends. |
| The tool-output budget of a turn and the steer marker added after a batch | `agent/tool_executor.py` `execute_tool_calls_segmented`; `tools/tool_result_storage.py` `enforce_turn_budget`, `maybe_persist_tool_result` | Not part of the dispatch port: the engine keeps no turn budget of tool output, and steer belongs to the C6 server. |

### 4. `search_files` over `regex-lite`

- Pattern syntax is that of `regex-lite`: no Unicode classes, ASCII `\w`, `\d`, `\s`.
- Hidden entries below the searched path and symlinks are skipped as ripgrep does by default;
  `.gitignore` files are not read.
- A pattern with `\n` is not switched to multiline mode, so it matches no line.
- A search stops after 32 MiB of file content or 20,000 directory entries instead of the 60 s
  timeout of the reference. The result then carries `"truncated": true` and
  `"limit_reason": "scan_limit"` where the reference sets `"search_timeout"`.
- Results come in the name order of a depth-first walk; ripgrep's order is that of its
  parallel traversal. `target='files'` sorts by modification time, newest first, as the
  reference does.
- The description leaves out "Ripgrep-backed, faster than shell equivalents."

### 5. Secret redaction over `regex-lite`

The redaction passes of `agent/redact.py` (`redact_sensitive_text`) run on `regex-lite`, whose
`\d`, `\w` and case-insensitive matching are ASCII. Python's `re` also takes other Unicode
decimal digits for `\d` (Telegram bot ids, phone numbers, the digits of a Slack app token),
other Unicode letters and digits for `\w` (the scheme word of an `Authorization` header) and
Unicode case variants under `IGNORECASE` (`K` U+212A for `k`, `ſ` U+017F for `s`). Real
credentials, header schemes and key names are ASCII, so these shapes stay unmasked or are
masked differently only for text no service issues. `\s` is Python's, the four ASCII
separators and the Unicode spaces included.

### 6. Error results

- Every failed tool call sets the C4G `is_error` flag besides its reference text; the reference
  has no such flag.
- Rust tools return their failures as values and the release profile aborts on panic, so the
  built-in tools have no counterpart of an exception escaping a handler. The sanitised
  `[TOOL_ERROR] Tool execution failed: <kind>: <error>` form is `Output::exception`, for a tool
  that reports a failure it did not handle.

### 7. Step limit and empty replies

Usage maps onto the reference's empty-attempt record as follows: the route's vendor and model
and the C4G stop reason form the signature, prompt tokens are `input`, `cache_read` and
`cache_write` together, and `output` counts reasoning as well. The reasoning and tool-call tags
of `strip_think_blocks` match case-insensitively in ASCII only, as in section 5.

| Behaviour | Upstream | Why |
|---|---|---|
| Vendor shaping of the summary request: reasoning fields copied, internal and underscore keys removed, `api_content` substituted, tool calls sanitised for strict APIs, `_sanitize_api_messages`, temperature, `extra_body` (reasoning, tags, provider preferences, plugins), prefill messages, the codex, Anthropic and chat-completions call paths | `agent/chat_completion_helpers.py` `handle_max_iterations` | The engine sends one C4G request to qb, which owns every vendor shape (ADR-0009). The summary request is the turn's request with the summary message appended and `tools` absent. |
| The relay bookkeeping of the summary call: one `iteration-summary:` request id for both tries, `retry_count`, `complete_logical_call` | `agent/chat_completion_helpers.py` `handle_max_iterations`, `agent/relay_llm.py` | Each try is a step of the turn with its own request id in the journal. |
| A turn at the limit reports `completed` false | `agent/turn_finalizer.py` `finalize_turn` | The summary is the turn's answer and the process exits 0; the journal and the exit code carry no completion flag. |
| A `quota_exhausted` error of the summary request answers with the summary failure text | `agent/chat_completion_helpers.py` `handle_max_iterations` | An exhausted quota ends the turn with its line naming the route and the nearest reset and with its own exit code, on the summary request as on any other step; the owner keeps that behaviour of the engine as it is. Every other failure answers with the reference text. |
| Assistant turns without visible text or tool calls other than the prefill stubs are left out of every request | `agent/agent_runtime_helpers.py` `drop_thinking_only_and_merge_users`, `run_agent.py` `_is_thinking_only_assistant` | The engine leaves out the prefill stubs only. Another such turn reaches the history only from a `max_tokens` or `refusal` stop, whose paths are not ported (see below). |
| An interrupt during the summary request answers with the summary failure text | `agent/chat_completion_helpers.py` `handle_max_iterations` | The engine's interrupt ends any step of the turn as interrupted. |
| The iteration budget shared with subagents, its refunds and grace call; the preserved verification answer at the limit; the kanban failure record | `agent/iteration_budget.py`, `agent/conversation_loop.py`, `agent/turn_finalizer.py` | The engine has no subagents, no verification gate and no kanban worker; the step limit is the only bound. |
| `agent.max_turns` is unlimited by default; a value that does not parse falls back to the default | `hermes_cli/config.py` `resolve_turn_limit` | The default is 200 steps by the owner's decision. A value that is no count fails as a settings error, like every other engine setting. |
| `agent.empty_response_guard.enabled` is on for any string but `0`, `false`, `no` and `off` | `agent/empty_response_guard.py` `resolve_guard_settings` | The engine reads every switch as `security.redact_secrets`: on only for `1`, `true`, `yes` or `on`. |
| The provider fallback chain after empty replies, and the `and fallback` wording of the terminal texts | `agent/conversation_loop.py`, `_try_activate_fallback` in `agent/chat_completion_helpers.py` | Route and provider choice belong to qb (ADR-0009); the engine knows one route alias. |
| The cost-aware retry budget: one retry when an attempt costs `cost_threshold_usd` (0.25) or more, the streak cost line | `agent/empty_response_guard.py` `empty_retry_budget`, `REDUCED_EMPTY_RETRY_BUDGET`, `DEFAULT_COST_THRESHOLD_USD`, `streak_cost_usd` | Prices live with qb; the engine has no price table, so the budget is always 3. |
| Partial-stream recovery: text streamed before a cut becomes the answer | `agent/conversation_loop.py`, `PARTIAL_STREAM_STUB_ID` in `agent/chat_completion_helpers.py` | A cut C4G stream ends the turn with a transport error. |
| Earlier text of a step that called only housekeeping tools becomes the answer | `agent/conversation_loop.py` `_last_content_with_tools`, `_HOUSEKEEPING_TOOLS` | The engine has none of `memory`, `todo`, `skill_manage`, `session_search`; the branch comes with them. |
| A labelled excerpt of the model's reasoning instead of `(empty)` | `agent/conversation_loop.py`, `_extract_reasoning` in `run_agent.py` | C4G thinking blocks are opaque vendor payloads; the engine has no readable reasoning, so the answer is `(empty)`. |
| The rewind of the trailing tool results and their tool-call message when the scaffolding is dropped at the `(empty)` end | `run_agent.py` `_drop_trailing_empty_response_scaffolding` | The reference rewinds only its in-memory list while its store keeps the rows. The journal keeps the tool calls and results the turn ran, then the `(empty)` answer. |
| The wait before an empty retry is cut by an interrupt, answering `Operation interrupted: retrying empty response from model (retry n/m).` | `agent/conversation_loop.py` | The wait blocks like the `overloaded` retry; the interrupt ends the turn at the next step as interrupted. |
| The truncation continuation of a `length` stop and the result of a `content_filter` stop | `agent/conversation_loop.py` | Not ported; a `max_tokens` or `refusal` reply ends the turn as it is and never reaches the empty-reply recovery. |
| Status lines and logs of the step limit and the empty-reply recovery | `agent/conversation_loop.py`, `agent/turn_finalizer.py`, `agent/chat_completion_helpers.py` | The engine has no status channel or log. |

### 8. Failure hints

The `hint` of a `terminal` result ports `tools/terminal_hints.py` with its call site in
`terminal_tool`. Its patterns run on `regex-lite`, whose `\w`, `\d` and `\b` are ASCII as in
section 5: a command, module, field or file name with other Unicode letters or digits is
matched up to them or not at all. `\s` and `\S` are Python's.

| Text | Upstream | Why |
|---|---|---|
| The hint of exit code 124 ends after `Raise timeout= (foreground max 600s).` | `tools/terminal_hints.py` `_EXIT_CODE_HINTS` | Its closing clause `or run it with background=true and notify_on_complete=true` points to parameters the engine ignores (section 1), as the clause left out of the timeout error does; a model following it reruns the command in the foreground. |

## Consequences

- A model that learned the reference tools calls the same names with the same arguments; the
  parameters of section 1 are accepted and ignored.
- Adding one of the parameters or behaviours above is a change of this record.
