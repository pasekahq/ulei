# ADR-0016: Engine tool surface: what stays different from the reference

Status: accepted
Date: 2026-11-17
Affects: crates/engine

## Context

The built-in tools of `ulei-engine` port the tool surface of the reference implementation
(`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3): the names `terminal`, `read_file`,
`write_file`, `patch`, `search_files`, `skills_list`, `skill_view`, `web_search`,
`web_extract` and `memory`, their parameter sets and
descriptions, and the error texts the model sees. The step loop around them ports the end of a turn at the step limit,
the recovery from empty replies and the per-turn tool-call guardrail. AGENTS.md allows a difference from the reference only when it
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
| MCP tools from a server that sets `supports_parallel_tool_calls` join a parallel run | `agent/tool_dispatch_helpers.py` `_is_mcp_tool_parallel_safe`, `tools/mcp_tool.py` `is_mcp_tool_parallel_safe` | The setting is off by default and the engine does not read it (ADR-0026), so every MCP tool is a barrier, as in the reference without it. |
| Multimodal results: the `_multimodal` envelope, its text summary and the subdirectory hint appended to it | `agent/tool_dispatch_helpers.py` `_is_multimodal_tool_result`, `_multimodal_text_summary`, `_append_subdir_hint_to_multimodal` | No engine tool returns images; every result is text. |
| The upstream-elision notice appended to the results of `web_*` and `browser_*` tools, and the risk metadata of all untrusted tools | `agent/tool_dispatch_helpers.py` `_maybe_append_elision_notice`, `_tool_output_risk_metadata` | The notice's markers are those of MCP server payloads; the MCP tools append it, labelled `[note: ` (ADR-0026). The risk metadata is internal and never reaches the model. The untrusted-data frame of `_maybe_wrap_untrusted` is ported for `web_search`, `web_extract` (section 11) and the MCP tools; the `browser_*` prefix comes with those tools. |
| A call past the deadline is abandoned: its daemon thread keeps running detached, gets the interrupt flag, and the turn goes on at once | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `tools/daemon_pool.py` | Workers are scoped threads (ADR-0010: no thread outlives its segment, none exists at idle). Calls past the deadline get the reference text `Error executing tool '<name>': timed out after <s>s` and no further call of the segment starts, but the segment returns when the running calls end. Every parallel-safe engine tool is bounded: the file and skill tools are file operations, `search_files` stops at the scan bounds of section 4 and at an interrupt, and a `web_search` or `web_extract` call ends within 305 s of its start (ADR-0015 section 2) and at an interrupt, so a segment with web calls may hold the turn that long. A worker stack is 2 MiB. |
| The start-order gate and the authorization gate of a parallel segment: dispatch serialised by call order, approval prompts one at a time, their 120 s and 360 s bounds | `agent/tool_executor.py` `_begin_in_order`, `_ConcurrentToolAuthorizationGate`, `_START_ORDER_GATE_TIMEOUT_S`, `_AUTHORIZATION_GATE_LOCK_TIMEOUT_S` | The policy chain is not thread-safe and has no approval prompt: the loop thread runs the pre hooks of every call of the segment in call order before the first call starts, and the post hooks in call order after the segment, where the reference runs each post hook when its call ends. |
| A call not started when the interrupt fires answers `[Tool execution cancelled — <name> was skipped due to user interrupt]` | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `execute_tool_calls_sequential` | The engine answers `interrupted before the tool ran`, the text of every call after an interrupt on the sequential path too. A running `search_files` stops at the interrupt. Every running `web_search` and `web_extract` of the segment has its helper's process group killed at once and answers `{"error": "Interrupted", "success": false}` (section 11): the interrupt handle wakes every supervisor registered on it. `terminal` is never in a parallel segment, so its process group is killed as on the sequential path. |
| The concurrency cap of `image_generate` (`image_gen.max_parallel_requests`, default 4) | `agent/tool_executor.py` `_image_generate_parallel_limit`, `_max_workers_for_tool_batch` | The engine has no image tool; the cap comes with it. |
| The heuristic for destructive `terminal` commands and the file checkpoint taken before them and before `write_file` and `patch` | `agent/tool_dispatch_helpers.py` `_DESTRUCTIVE_PATTERNS`, `_is_destructive_command`; `agent/tool_executor.py` `_ensure_file_checkpoint` | It only decides checkpoints, and the engine has no checkpoint manager. In the planner `terminal` is a barrier whatever its command, as in the reference. |
| The deadline setting: `timeouts.tools.concurrent_batch` in the config file winning over `HERMES_CONCURRENT_TOOL_TIMEOUT_S`; an invalid value logged and skipped | `agent/tool_executor.py` `_resolve_concurrent_tool_timeout`, `agent/deadline.py` `resolve_timeout`, `clamp_timeout` | The engine reads `engine.concurrent_tool_timeout` from the required `ulei.toml` through `ulei_contracts::config` (ADR-0040), overridden by `ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT` as every engine setting is. The values are the reference's: seconds, `0` or less or `nan` for no deadline, at most 365 days. A value that is not a number fails the run with an error naming the setting, since the engine has no log to carry a warning. |
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
| Output muted after a step whose calls are all housekeeping | `agent/conversation_loop.py` `_mute_post_response`, `_vprint` in `run_agent.py` | The engine prints no progress. The visible text of such a step answers a later empty reply as in the reference (`_last_content_with_tools`, `_HOUSEKEEPING_TOOLS`, with `memory`, `todo`, `skill_manage` and `session_search`, of which the engine has `memory`) and reaches the stop policies as any final reply does; a step that calls another tool clears it. |
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

### 9. Lifecycle guard

`terminal` refuses a command that would restart, stop or kill the service running the engine,
before it runs and in the working directory of the run. It ports the terminal path of
`cron/lifecycle_guard.py` (branches C and D of `_GATEWAY_LIFECYCLE_PATTERN`, the data-sink
exemption, the referenced-script and shell-payload walk with its size, depth and binary limits),
its call site in `terminal_tool` with the blocked result, and the own-target narrowing of the
owner's local patch 0017. The blocked text names the unit where the reference names its gateway.
Its patterns run on `regex-lite`: every `\b` sits next to an ASCII letter of a literal, so an
ASCII boundary holds wherever Python's does and adds one only before a non-ASCII letter; `(?i)`
is ASCII as in section 5, and the Unicode variants it misses spell no command or unit name the
shell or systemd would run. `\s` and `\S` are Python's.

| Behaviour | Upstream | Why |
|---|---|---|
| The protected target: only the systemd unit running this engine, and for `kill`/`pkill` also the `ulei-engine` executable. A systemctl shape blocks when the rest of its line names the unit, with or without `.service`; a kill shape when it names the unit or the executable; every other name passes, one that extends the unit's name included. The unit is `engine.service_unit` (`ULEI_ENGINE_SERVICE_UNIT`); unset, under systemd (`INVOCATION_ID` set) it is the last `.service` component of the cgroup path in `/proc/self/cgroup`, read once at start, and without one the engine refuses to start; outside systemd the guard is off | `cron/lifecycle_guard.py` `_GATEWAY_LIFECYCLE_PATTERN`; patch 0017 `_SYSTEMD_GATEWAY_UNIT_REF`, `_own_gateway_profile_suffix`, `_is_sibling_profile`, `_match_targets_only_sibling_gateways`; `tools/terminal_tool.py` `_HERMES_GATEWAY` | The PO's decision: the command must not kill its own caller, and another unit cannot. The reference names a family of gateway units told apart by profile suffix and blocks a family name it cannot attribute; the engine knows its exact unit. Discovery keeps the guard on in a deployed service without a setting; a one-shot run outside a unit has nothing to guard, as the reference guard is off outside its gateway process. |
| A `systemctl restart\|stop\|start` unit that is a glob matching the unit, or holds `$`, a backtick or `{`; units fed to it by `xargs`; a `pkill` pattern, or a `pgrep`/`pidof` pattern on a line that runs `kill` or `pkill`, that matches the unit or the executable as a case-insensitive regular expression or does not compile as one; a name of the unit or executable before a pipe into `systemctl restart\|stop\|start` or `kill`/`pkill`: each blocks | patch 0017 `_KILL_UNSAFE_CHARS` | The PO's rule: a glob, regex, substitution or pipe that could widen the target to the own unit blocks. The patch applies it only to a line that spells the gateway family name, which every gateway unit carries; the engine's unit has no such family name, so these shapes are checked against the unit itself. |
| launchctl: branch B of `_GATEWAY_LIFECYCLE_PATTERN`, `contains_launchctl_submit_command` and its blocked text | `cron/lifecycle_guard.py`, `tools/terminal_tool.py` `terminal_tool` | launchd is macOS; the engine runs on Linux under systemd. |
| The CLI restart/stop branch: branch A of `_GATEWAY_LIFECYCLE_PATTERN`, the self-target refusal of `gateway stop\|restart`, the profile flags of a kill pattern | `cron/lifecycle_guard.py`; `hermes_cli/gateway.py`; patch 0017 `_gateway_cli_self_targeting`, `_PROFILE_FLAG_REF` | ulei has no such command and no profiles. |
| The cron-job side: the check of a job's prompt and script at creation | `cron/lifecycle_guard.py` `check_gateway_lifecycle`, `_resolve_script_path`, `_read_script_for_scanning`, `GatewayLifecycleBlocked` | ulei has no cron. |
| The kill switch that restores the unnarrowed guard | patch 0017 `HERMES_LIFECYCLE_GUARD_OWN_ONLY` | The PO's decision: the guard always protects the own unit only. |
| The qb gateway unit is not protected; considered and rejected | patch 0017 | The reference blocks because the command kills the calling process. A qb restart does not, and restarting qb on the owner's request is legitimate work for the secretary. |
| Paths in a cloud-synced macOS folder refused unread; the second read of a script through the remote backend (`head -c`) | `cron/lifecycle_guard.py` `_is_cloud_placeholder_path`, `_CLOUD_PLACEHOLDER_MARKERS`; `tools/terminal_tool.py` `_read_script_in_env` | FileProvider placeholders are macOS. The engine runs commands only on the local host, so the second read is the local one, bounded to one byte past the limit, under the contract of `_sanitize_remote_script_text`. |
| Scripts whose arguments are masked as data like a data sink's | patch 0017 `_DEFAULT_DATA_SINK_SCRIPTS`, `HERMES_LIFECYCLE_DATA_SINK_SCRIPTS`, `_data_sink_argument_start` | The default names a script path of the deployment, which the engine does not carry; an own-unit lifecycle command in such a script's arguments blocks. |
| `~user` in a script path | `cron/lifecycle_guard.py` `_expand_candidate_path` | Paths expand only `~` and `~/`, as in the other engine tools. |

### 10. Skills

`skills_list` and `skill_view` port `tools/skills_tool.py` over the one directory of
`engine.skills_dir`, the directory C6 `skills_dir` names; the system prompt indexes the same
directory (ADR-0020 section 4). Both scan it only when they run.

Frontmatter is read by a port of the YAML subset the reference fields need, not by a YAML
library: a YAML crate would be a new dependency whose binary size and RSS count against the
ADR-0010 budgets, and `name`, `description`, `tags`, `related_skills`, `platforms`,
`compatibility` and `metadata` need only the subset. It reads block mappings nested by space
indentation, block sequences of scalars at or below their key, one-line flow sequences of
scalars, plain scalars with their continuation lines and ` #` comments, one-line single- and
double-quoted scalars with their escapes, and literal and folded block scalars with an optional
chomping indicator; `~`, `null` and an empty value are null. A block outside the subset is read
by the fallback `parse_frontmatter` applies to YAML it cannot parse: one stripped `key: value`
per line.

| Behaviour | Upstream | Why |
|---|---|---|
| Plain scalars YAML resolves to booleans, numbers or dates stay text: `description: yes` is `yes` where the reference has `True` in the index and `true` in `skill_view` | `agent/skill_utils.py` `parse_frontmatter` | The subset keeps every scalar as written; the reference fields are text. |
| Anchors, aliases, tags, flow mappings, quoted scalars over several lines, block scalars with an indentation indicator, more-indented lines in a folded scalar and sequences of mappings are read by the fallback, where YAML resolves them | `agent/skill_utils.py` `parse_frontmatter` | Outside the subset; the deployment's skills use none of them in the fields read. |
| Skills outside the one directory: external directories, trusted project directories, the organisation mirror and plugin skills. `_org` at the top of the directory is skipped, as the reference skips it without an active organisation; a qualified `namespace:skill` falls through to `namespace/skill` in the directory, as the reference does when no plugin provides it | `agent/skill_utils.py` `get_external_skills_dirs`, `get_project_skills_dirs`, `iter_project_skill_files`, `read_active_org_id`, `is_quarantined_project_skill`; `tools/skills_tool.py` `_serve_plugin_skill`, `_plugin_skill_linked_files` | The engine reads the one configured directory, loads no plugins and joins no organisation. |
| Required environment variables, credential files, setup help and the secret capture: `skill_view` answers `required_environment_variables`, `missing_required_environment_variables` and `missing_credential_files` empty, `setup_needed` false and `readiness_status` `available`, without `setup_help`, `setup_note` or `gateway_setup_hint`, whatever the frontmatter declares | `tools/skills_tool.py` `_get_required_environment_variables`, `_capture_required_environment_variables`, `_remaining_required_environment_names`, `_build_setup_note`, `load_env`; `tools/env_passthrough.py`; `tools/credential_files.py` | The engine has no profile environment file, secret prompt or sandbox to pass variables into; `terminal` runs with the engine's environment (section 2). A skill that needs a variable finds out when its command runs. |
| The environment relevance gate (`environments:` kanban, docker, s6) | `agent/skill_utils.py` `skill_matches_environment`, `_detect_environment` | Its detection reads the reference's own worker variables and container markers; the engine runs on the host. Such a skill is indexed and listed. `platforms` is matched against Linux, the engine's platform. |
| Conditional activation by tools and toolsets, and `tags` and `related_skills` under the reference's own metadata namespace | `agent/skill_utils.py` `extract_skill_conditions`; `agent/prompt_builder.py` `_skill_should_show`; `tools/skills_tool.py` `skill_view` | The namespace key is the reference product's name, which engine code may not spell (AGENTS.md), and the engine has no toolsets. `tags` and `related_skills` come from the top level; `skill_view` still returns the whole `metadata` mapping, so the namespaced tags reach the model there. |
| Template variables and inline shell in `SKILL.md` | `agent/skill_preprocessing.py` `preprocess_skill_content` | The variable names carry the reference product name; inline shell is off by default and the deployment does not turn it on. The content is served as it is on disk. |
| The warnings for a skill outside the trusted directories and for prompt-injection phrases | `tools/skills_tool.py` `skill_view`, `_INJECTION_PATTERNS` | They only reach the reference's log; the engine has no log. |
| View and use counters and the background-review read mark | `tools/skills_tool.py` `_skill_view_with_bump`; `tools/skill_usage.py` `bump_view`, `bump_use`; `tools/skill_manager_tool.py` `mark_background_review_skill_read` | They feed the curator and the review of `skill_manage`, which the engine does not have. |
| The 30 s scan cache of the skill list | `tools/skills_tool.py` `_SKILLS_CACHE`, `_skills_scan_signature` | Each call scans; the engine keeps no state between turns at idle (ADR-0010). |
| Clearing the repeat-view dedup when the context is compressed | `tools/skills_tool.py` `reset_skill_view_dedup` | The engine does not compress context; the dedup lives with the registry, as the read dedup does (section 2). |
| The disabled skills: `engine.disabled_skills` (`ULEI_ENGINE_DISABLED_SKILLS`), comma-separated names, one list for every surface | `agent/skill_utils.py` `get_disabled_skill_names`; `tools/skills_tool.py` `_is_skill_disabled` (`skills.disabled`, `skills.platform_disabled`) | The engine has no platform (ADR-0020 section 2), so a deployment sets the union of the global list and its platform's list; the engine config has no lists. |
| A disabled skill answers `Skill '<name>' is disabled. Enable it by removing it from the engine.disabled_skills setting or inspect the files directly on disk.` | `tools/skills_tool.py` `skill_view` | The reference text names its own CLI command. |
| Linked and available files come in name order within each directory | `tools/skills_tool.py` `skill_view` (`glob`, `rglob`) | The reference takes directory order, which depends on the file system. |
| The `<name>.md` lookup enters no symlinked directory, reads `\` in the name as an escape and is skipped for a name whose pattern does not parse (an unclosed `[`) | `tools/skills_tool.py` `skill_view` (`rglob`) | It uses the glob of `search_files`; pathlib follows a literal component through a symlinked directory and reads `\` and an unclosed `[` literally. |
| The index walk enters a symlinked directory only when it is not already inside it | `agent/skill_utils.py` `iter_skill_index_files` (`os.walk(followlinks=True)`) | A symlink cycle would make the walk recurse without end. |
| A `SKILL.md` or linked file that is neither a regular file nor a directory (a FIFO, socket or device) is unreadable | `tools/skills_tool.py` `_find_all_skills`, `skill_view`; `agent/prompt_builder.py` `_parse_skill_file` | Reading it could block the turn. |

### 11. Web tools

`web_search` and `web_extract` port `tools/web_tools.py` with the Exa backend of
`plugins/web/exa/provider.py`; their transport, limits and URL checks are in ADR-0015. A call
runs the `ulei-web` helper. The engine frames every result of 32 characters or more, failures
included, in the reference's `<untrusted_tool_result source="<tool>">` block with any
delimiter inside defanged, as `make_tool_result_message` does for these tools. A `web_search`
result fails (`is_error`) when `success` is false; a `web_extract` result fails when the whole
call is refused or every page carries an error.

| Behaviour | Upstream | Why |
|---|---|---|
| A call interrupted while the helper runs kills the helper's process group and answers `{"error": "Interrupted", "success": false}` for both tools | `tools/web_tools.py` `web_search_tool`; `plugins/web/exa/provider.py` `search`, `extract` | The reference checks the interrupt only before the Exa call, and its extract then answers a list of per-page `Interrupted` errors; the engine stops a running call, as it stops `terminal`. |
| A helper that does not answer within 5 s past the 300 s Exa call limit is killed, and the call answers `Error searching web: the web helper did not answer within 305s` or `Error extracting content: ...`; a helper that cannot start, ends by a signal or exceeds the 16 MiB reply answers its fault after the same prefix | `tools/web_tools.py` `web_search_tool`, `web_extract_tool` | The reference runs the backend in process with no time limit; these failures exist only for the helper process. The prefixes are those of the reference's exception answers. |
| `engine.web_extract_char_limit` (`ULEI_ENGINE_WEB_EXTRACT_CHAR_LIMIT`) that is not a non-negative integer fails as a settings error | `tools/web_tools.py` `_get_extract_char_limit` (`web.extract_char_limit`) | The reference falls back to 15000; the engine reads it as every other count setting. A valid value is clamped to 2000-500000 as the reference clamps it. |
| The keyless free tier, the keyless rescue of a failed call, the other backends and the provider registry with its selection and disabled-plugin errors | `tools/web_tools.py`, `plugins/web/keyless_mcp.py`, `agent/web_search_registry.py` | Exa with a key is the deployment's backend (ADR-0015 section 5). Without `engine.web_helper` the tools are not registered, as the reference does not offer them without a backend. |
| The debug session log of the calls | `tools/web_tools.py` `_debug` (`WEB_TOOLS_DEBUG`) | The engine has no log. |

### 12. Tool-call guardrails

The `tool-guardrails` policy ports `agent/tool_guardrails.py` `ToolCallGuardrailController` with
its call sites in `agent/tool_executor.py`, `run_agent.py` and `agent/conversation_loop.py`:
thresholds, texts, the synthetic result of a blocked call, the appended guidance and the
controlled halt answer. It is the last policy of the chain, so another policy's denial comes
first, as a plugin block comes before `before_call`, and a denied or blocked call is not
counted. A block or the same-tool halt ends the turn after its batch; `ulei-engine run` prints
the halt answer and exits 0. A call counts as failed when its result is `is_error`, or when it
is a `terminal` call whose result has an `exit_code` other than 0.

The loop cap comes before the hard-stop check and applies with hard stop off: once a turn let
`loop_caps.max_web_searches` `web_search` calls through, the next is blocked with
`loop_web_search_cap`. With `engine.stall_guards` on, the policy tracks the run of consecutive
calls with the same tool, canonical arguments and result hash: from the 2nd call of a run, a
successful result of 512 characters or more is replaced by the reference stub naming the run's
first call id and at most 120 characters of the canonical arguments; from the 3rd, the
identical-call notice follows after a blank line. The stub replaces the raw result first, the
guidance is appended to it, the notice comes last.

| Behaviour | Upstream | Why |
|---|---|---|
| The idempotent names `session_search`, `browser_snapshot`, `browser_console`, `browser_get_images` and the eight `mcp_filesystem_*` names; the mutating names `execute_code`, `todo`, `skill_manage`, `browser_click`, `browser_type`, `browser_press`, `browser_scroll`, `browser_navigate`, `send_message`, `cronjob`, `delegate_task` and `process` | `agent/tool_guardrails.py` `IDEMPOTENT_TOOL_NAMES`, `MUTATING_TOOL_NAMES` | The engine has none of these tools; by the PO's decision a name comes back with its tool. |
| The failure signal: a result fails when its first 500 characters hold `"error"` or `"failed"` in any case or it starts with `Error`, or it is a JSON object with a non-empty `error`, or a non-empty `message` beside `success: false` or an `error` key; a `terminal` result fails only on a non-zero `exit_code`; a landed `write_file` or `patch` and a multimodal result never fail; a full `memory` store fails | `agent/display.py` `_detect_tool_failure`, `agent/tool_result_classification.py` `file_mutation_result_landed`, `agent/tool_guardrails.py` `classify_tool_failure` | The engine's tools set `is_error` on every failure they report, so the flag stands for the text test, which also fails a read file or a search match that holds the word `"error"`. `write_file` and `patch` set `is_error` only when the write did not land. A `terminal` result fails on a non-zero `exit_code` as in the reference, and also when it is `is_error` without one (invalid arguments), where the reference sees no failure. |
| The flat keys `exact_failure_warn_after`, `exact_failure_block_after`, `same_tool_failure_warn_after`, `same_tool_failure_halt_after`, `no_progress_warn_after` and `no_progress_block_after` beside `warn_after` and `hard_stop_after` | `agent/tool_guardrails.py` `ToolCallGuardrailConfig.from_mapping` | The engine reads only the nested keys under `engine.tool_loop_guardrails` of the required `ulei.toml` (ADR-0040), each with its environment variable winning over it; C9 has no flat key, so the decoder refuses one as an unknown key. |
| A count that is not a positive integer falls back to its default; a switch is on for `1`, `true`, `yes`, `on`, `enabled` or a non-zero number, off for `0`, `false`, `no`, `off`, `disabled`, and its default for any other value | `agent/tool_guardrails.py` `_positive_int`, `_as_bool` | A count that is not a positive integer is a settings error naming its key. A switch is read as every engine switch (section 7): on for `1`, `true`, `yes` or `on`, off for any other value. |
| The warning of a `web_search` or `web_extract` result comes inside its `<untrusted_tool_result>` frame | `agent/tool_dispatch_helpers.py` `make_tool_result_message`, `_maybe_wrap_untrusted` | The engine frames the result inside the tool (section 11), so the guidance follows the closing tag; the hash of a result is taken over the framed text. |
| The status line of a halt, and the `guardrail` field and `guardrail_halt` exit reason of the turn result | `agent/conversation_loop.py`, `agent/turn_finalizer.py` | The engine has no status channel or log, and its outcome carries no turn result metadata. |
| The identical-call notice and the reference stub open with `[`, the reference product name and ` note: ` | `agent/tool_guardrails.py` `observe_call`, `_build_result_reference_stub` | The product name may not appear in engine source (AGENTS.md hard rule 8, `cargo xtask arch`); the label is `[note: ` and every other byte of both texts is the reference's. |
| A stub whose referenced result was persisted out of context ends with the line naming the spill file, recorded by `record_persisted_result` | `agent/tool_guardrails.py` `record_persisted_result`, `_build_result_reference_stub`; `agent/tool_executor.py` `_record_persisted_path_for_stub`; `tools/tool_result_storage.py` `extract_persisted_path` | The engine persists no tool result out of context (section 3), so no result has a spill path; the stub never carries the line. |
| `loop_caps.max_subagents` (default 50) and the `loop_subagent_cap` block of `delegate_task` spawns | `agent/tool_guardrails.py` `LoopCapConfig`, `_check_loop_cap`, `_subagent_spawn_count` | The engine has no `delegate_task` tool, so the key is not read; by the PO's decision the cap comes back with the tool. |
| `process`, `bfl_flux3_get_result` and names ending in `_get_result` or `_poll` get no identical-call notice | `agent/tool_guardrails.py` `STALL_GUARD_REPEATABLE_TOOLS`, `_STALL_GUARD_REPEATABLE_SUFFIXES`, `is_stall_guard_repeatable` | The engine has none of these tools and no poller of that naming; every engine tool gets the notice. By the PO's decision the exemption comes back with such a tool. |
| A `loop_caps` cap that is negative or not an integer falls back to its default; `agent.stall_guards` is on for any truthy value | `agent/tool_guardrails.py` `_non_negative_int`; `agent/agent_init.py` | Such a cap is a settings error naming its key; `engine.stall_guards` is read as every engine switch (section 7). |
| `agent.stall_guards` also switches the continuation of a reply that ends without a tool call on an announced next action | `agent/conversation_loop.py`, `agent/agent_runtime_helpers.py` `trailing_continue_intent` | The engine has no such continuation; `engine.stall_guards` switches only the notice and the stub. |
| `agent/repetition_guard.py`: the check that aborts the continuation of a reply cut by the output limit when the cut text is dominated by verbatim repeats | `agent/repetition_guard.py` `is_repetition_dominated`, `agent/conversation_loop.py` | The engine has no continuation of a reply cut by the output limit (section 7); the continuation and the guard are filed together as issue:164f64a36156327bb11b. |

### 13. Memory

`memory` ports `tools/memory_tool.py` over the two files of `engine.memory_file` and
`engine.user_file` with the limits of `engine.memory_char_limit` and `engine.user_char_limit`,
the paths and limits the system prompt renders (ADR-0020): `MEMORY_SCHEMA`, the dispatcher
`memory_tool` with the `new_text` alias, `MemoryStore.add`, `replace`, `remove`, `apply_batch`,
`_batch_error`, `_success_response`, `_previews`, `_consolidation_failure` with its cap of 3
failures a turn, `_missing_old_text_error`, `_memory_target_error`, `_read_failed_error`,
`_detect_external_drift`, `_drift_error`, the lock file and the atomic write. Every write takes
the lock `<file>.lock`, rereads the file and replaces it by a rename. A `replace`, `remove` or
batch first checks the text it reread: when its stripped entries joined do not give that text
back, or one entry is longer than the store's limit, the text is copied to
`<file>.bak.<unix seconds>` beside the store and the write is refused with the reference error;
an `add` appends without the check, as in the reference. The registry is built for each turn, so the failure count starts at
zero with each turn, as `reset_consolidation_failures` sets it. A write changes the files and
never the prompt stored with a running session (ADR-0020 section 1).

| Behaviour | Upstream | Why |
|---|---|---|
| The write approval gate: a staged write, its pending id and the inline prompt; the replay of a staged write | `tools/memory_tool.py` `_apply_write_gate`, `_apply_batch_write_gate`, `apply_memory_pending`; `tools/write_approval.py` | The gate is off unless `memory.write_approval` is set, and the secretary profile leaves it off (its approvals are off too); the engine has no channel to approve a staged write. |
| The store switches `memory.memory_enabled` and `memory.user_profile_enabled`: a disabled target refused with `Built-in <file> writes are disabled in memory config.`, the schema narrowed to the enabled target, the tool left out when both are off | `tools/memory_tool.py` `target_enabled`, `_memory_target_error`, `get_builtin_memory_store_flags`, `check_memory_requirements`, `_build_memory_schema_overrides` | The secretary profile sets neither, so both stores are on; the engine config has no such keys (C9) and both stores are always writable. |
| The live entry list of the store: the answer to a `replace` or `remove` without `old_text` shows the entries loaded at session start or at the last write | `tools/memory_tool.py` `MemoryStore.memory_entries`, `_missing_old_text_error` | The engine keeps no store state between calls; that answer reads the file when it is given. A file that cannot be read answers the read-failure text where the reference shows the entries it holds. |
| The temporary file beside the link, and the copy over a cross-device or busy target | `utils.py` `atomic_write_text`, `atomic_replace` | The temporary file is made beside the file the link points to, so the rename never crosses devices. |
| The `.bak.<unix seconds>` snapshot of a store that does not look tool-written is created with mode 0600 | `tools/memory_tool.py` `_detect_external_drift` | It holds the store's text, which the engine writes owner-only; the reference creates it with the process umask. |
| A field of another JSON type than the schema's fails as an exception inside the handler | `tools/memory_tool.py` `memory_tool`, `apply_batch` | Ported as the same `[TOOL_ERROR] Tool execution failed: AttributeError: ...` answer; a non-string target is refused as an invalid target, where a list or object target makes the reference fail on an unhashable value. |

### 14. Hardline floor

The `hardline` policy is the first policy of the engine chain, so it answers before every other
policy and before `terminal` runs. It ports the unconditional floor of `tools/approval.py` with
the owner's patch 0018: `detect_hardline_command` with `_command_parser_limit_exceeded` and its
three limits, the twelve `HARDLINE_PATTERNS` with their descriptions and the helpers they are
built from (`_CMDPOS`, `_RM_FLAG_PREFIX`, `_hardline_rm_path`, `_HARDLINE_SYSTEM_DIRS`),
`_normalize_command_for_detection`, the detection variants of `_command_detection_variants`
(quoted newlines masked, quoted grep PCRE operands blanked by `_grep_safe_detection_variant`, the
drive-path variant, the programs of `bash -c` and of the program options of `rg`, `sort`, `ag`
and `man`, real command starts marked, command words deobfuscated), and
`_hardline_block_result` with `_save_blocked_payload`. A refused command gets the blocked result
of `terminal_tool`: `output` empty, `exit_code` -1, the reference block text as `error` and
`status` `blocked`.

A command whose grep pattern operands do not parse is handled as patch 0018 handles it. In the
default mode `warn` it is refused with `command parser limit or malformed executable payload`
when a `$(` or backtick follows the start, a separator or a group sign; otherwise the patterns
also run on the crude variants of `_malformed_crude_variants` (the raw command and its form with
quotes made spaces, split at every separator, group sign, backtick and `$(`, up to four `sh -c`,
`eval` or `exec` wrappers peeled, joined by newlines, then normalised), and when none matches,
the line `ulei-engine: Hardline downgraded to warning: <description> (command: <first 200
characters>)`, its secrets masked by the redaction of `security.redact_secrets` as the
reference's redacting log formatter masks them, goes to stderr and the detection variants run as
for any command. In mode `block`
the command is refused with that description, as the reference does. The mode is the C9 key
`engine.hardline_malformed_mode`; the engine has no yolo, approval mode or cron mode, so nothing
turns the floor off. Its patterns compile on the first `terminal` call.

The patterns run on `regex-lite` against the lowercased command with `(?is)` for
`IGNORECASE | DOTALL`; the pattern text is the reference's. `\s` and `\S` are Python's. `\w`
takes every non-ASCII character besides Python's word characters, so it matches wherever
Python's does. Every `\b` sits next to an ASCII letter or digit of the pattern, so an ASCII
boundary holds wherever Python's does and adds one only before a non-ASCII letter. `$` appears
only as `(?:\s|$|...)`, where `\s` takes the final newline Python's `$` also accepts. The
patterns have no lookaround. With `IGNORECASE` Python also reads `ſ` as `s` and `ı` as `i`; the
shell runs no command or path so spelled. The `$IFS` step ends `IFS` at a Unicode word boundary
as Python's `\b` does. The three patterns of patch 0018 take the same translation; the wrapper
pattern names only ASCII words, so its ASCII `IGNORECASE` misses only such spellings. The
helper patterns of the variants (`_PARAM_REPLACEMENT_RE`, `_PARAM_DEFAULT_RE`,
`_SIMPLE_SHELL_LITERAL_RE`, `_ENV_ASSIGNMENT_RE`, the `echo` flag pattern and the drive-path
search) are scans with the same matches: in each, every repeated class stops at the character
that must follow it, so the regex has one way to match.

| Behaviour | Upstream | Why |
|---|---|---|
| The malformed-grep mode is the C9 key `engine.hardline_malformed_mode` (`ULEI_ENGINE_HARDLINE_MALFORMED_MODE`), `warn` or `block`, read when a turn starts; any other value fails the turn naming the key | patch 0018 (`HERMES_HARDLINE_MALFORMED_MODE` read on every call, every value other than `block` is `warn`) | Engine settings come from `ulei.toml` with an environment override (ADR-0040), and a C9 string key lists the values its reader accepts (ADR-0041). |
| The malformed-grep warning is one stderr line with the newlines of the shown command as spaces, made after the secret redaction | patch 0018 (`logger.warning` through the `RedactingFormatter` of the log handlers) | The engine has no log; its warnings are stderr lines, as the MCP connect warnings are. |
| Interpreter flags and heredocs, the descriptions of the execution findings and the malformed-payload finding | `tools/approval.py` `_interpreter_family`, `_interpreter_exec_flag`, `_INTERPRETER_EXEC_FLAGS`, `_INTERPRETER_WITH_ARG`, the findings of `_execution_flag_findings` without a payload | Not ported: a finding without a payload adds no variant, so the hardline answer is the same without them; they serve `detect_dangerous_command` (row below). |
| Substitutions nested more than 64 deep refuse the command with the parser-limit result | `tools/approval.py` `_iter_shell_command_starts` (its recursive `scan`) | The reference recurses once per nested `$(` or backtick, and past Python's limit of 1000 frames the scan raises and `terminal_tool` answers a failed call without running the command. The port scans with its own stack and stops at 64, which keeps the scans of a 128,000-character command near a second; no hand-written command nests that deep. |
| NFKC: only the fullwidth forms U+FF01 to U+FF5E fold to ASCII and U+3000 to a space | `tools/approval.py` `_normalize_command_for_detection` (`unicodedata.normalize('NFKC')`) | The engine binary carries no Unicode normalisation tables (ADR-0010 budgets, as in section 2 and ADR-0020). The fold covers what NFKC changes in the characters the patterns name; the other compatibility forms (small forms, superscripts, ligatures) stay as written, and the shell does not read them as the ASCII NFKC maps them to either. |
| The user-home fold takes `HOME` and its resolved form; without `HOME` nothing folds | `tools/approval.py` `_rewrite_resolved_user_home` (`os.path.expanduser`) | `expanduser` falls back to the password database when `HOME` is unset; the engine reads none, and its other tools expand `~` from `HOME` only. |
| The profile-home fold to `~/.<home>/` | `tools/approval.py` `_rewrite_resolved_hermes_home` | No hardline pattern names the profile home: the fold makes no text a pattern matches and removes none. The engine's counterpart, its state directory, is not folded. |
| A refused parser-limit payload is saved under `blocked-scripts` in `engine.terminal_output`, owner-only, with `by the agent` in its header | `tools/approval.py` `_save_blocked_payload` (`cache/blocked-scripts` in the profile home, the process umask) | The terminal output directory is the engine's cache of terminal files. The command may hold secrets, so it is written owner-only as the spill files are. The product name may not appear in engine source (AGENTS.md hard rule 8). |
| Container backends skip the floor | `tools/approval.py` `_should_skip_container_guards` | The engine runs commands on the local host only, so the floor always applies. |
| `force=True` skips every guard | `tools/terminal_tool.py` `terminal_tool` | The engine's `terminal` has no `force` parameter. |
| The workdir check, the lifecycle guard (section 9) and the self-repo guard answer before the floor | `tools/terminal_tool.py` `terminal_tool` | The floor is a policy and the chain runs before any tool, so a command both refuse gets the hardline result. |
| The warning log line of a block | `tools/approval.py` `check_all_command_guards`, `check_dangerous_command` | The engine has no log; the refused call and its result are in the journal. |
| `DANGEROUS_PATTERNS`, `detect_dangerous_command`, the approval prompts and the permanent allowlist | `tools/approval.py` | The secretary profile runs with `approvals.mode off`, where none of them refuses a command; the floor is the guard ported for that mode. |
| User deny rules (`approvals.deny`) and their block text | `tools/approval.py` `_match_user_deny_rule`, `_user_deny_block_result` | The secretary profile runs with approvals off and the ported floor is the hardline list only; C9 has no `approvals.deny` key. |
| The sudo stdin guard: `sudo -S` refused while `SUDO_PASSWORD` is unset | `tools/approval.py` `_check_sudo_stdin_guard`, `_SUDO_STDIN_RE`, `_sudo_stdin_block_result` | The secretary profile runs with approvals off and the ported floor is the hardline list only; the engine has no `SUDO_PASSWORD` handling (section 2). |

## Consequences

- A model that learned the reference tools calls the same names with the same arguments; the
  parameters of section 1 are accepted and ignored.
- Adding one of the parameters or behaviours above is a change of this record.
