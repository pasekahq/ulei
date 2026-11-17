# ADR-0016: Engine tool surface: what stays different from the reference

Status: accepted
Date: 2026-11-17
Affects: crates/engine

## Context

The built-in tools of `ulei-engine` port the tool surface of the reference implementation
(`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3): the names `terminal`, `read_file`,
`write_file`, `patch` and `search_files`, their parameter sets and descriptions, and the error
texts the model sees. AGENTS.md allows a difference from the reference only when it is recorded
with the upstream module and the reason. This record lists the differences that stay.

## Decision

### 1. Parameters that are not advertised

| Parameter | Upstream | Why |
|---|---|---|
| `terminal.background`, `terminal.notify_on_complete`, `terminal.watch_patterns` | `tools/terminal_tool.py` `TERMINAL_SCHEMA`, `tools/process_registry.py` | The engine has no process registry and no `process` tool; every call ends with its process group killed. They come with the `process` tool. |
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
| V4A header paths written back into the patch text and parsed again | `tools/file_tools.py` `_rewrite_v4a_patch_paths_for_host` | The engine resolves the paths it parsed. Parsing the rewritten `*** Move File: <src> -> <dst>` again splits at the first `->`, so a working directory whose path holds `->` cuts the source short. |
| The per-path lock around a `patch` and the note that verification evidence of the workspace is stale | `tools/file_state.py` `lock_path`, `tools/file_tools.py` `_mark_verification_stale` | The engine runs one tool call at a time and keeps no verification evidence. |
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
| Hints on a search with no match: case-insensitive, hidden-file and fixed-string probes | `tools/file_operations.py` `_zero_match_probe` | Each probe scans the tree again; the scan bounds of section 3 would apply three more times per empty search. |
| A `path` holding several paths separated by spaces or commas | `tools/file_operations.py` `_try_multi_path_search` | Not ported; such a path is reported as not found with similar names. |
| A `workdir` that does not exist: the wrapper's `cd` fails and the call ends with the shell's message and exit code 126 | `tools/environments/base.py` `_wrap_command` | The engine runs no wrapper script (see the shell state row); it starts the shell in that directory, so the start fails and the result is `Command execution failed: FileNotFoundError: [Errno 2] No such file or directory: '<directory>'` with `exit_code` -1. |
| Three retries of a `terminal` call whose start failed, after 2, 4 and 8 s | `tools/terminal_tool.py` `terminal_tool` | A local start failure (missing directory, missing shell) does not heal within the backoff, which would hold the turn for 14 s; the failure is answered at once. |
| `terminal.timeout` (`TERMINAL_TIMEOUT`), `tool_output.max_bytes`, `tool_output.max_lines`, `tool_output.max_line_length` and `file_read_max_chars` settings | `tools/terminal_tool.py` `_get_env_config`, `tools/tool_output_limits.py`, `tools/file_tools.py` `_get_max_read_chars` | The engine config has no per-tool settings; it uses the reference defaults: a 180 s timeout, a 50,000-character output cap, 2000 lines of at most 2000 characters and 100,000 characters per read. |
| Environment of the `terminal` process: provider credentials removed, sane `PATH` entries added, the `HOME` contract of `terminal.home_mode` applied | `tools/environments/local.py` `_make_run_env` | The process inherits the engine's environment unchanged. Its `HOME` is the engine's own, the real home that `home_mode real` selects: the engine has no profile home to repair from. The engine reads no vendor credential (ADR-0009), so it has none to remove. |
| Redaction patterns registered by plugins | `agent/redact.py` `register_redaction_patterns` | The engine loads no plugins; the built-in token shapes are the whole set. |
| A warning at startup when `security.redact_secrets` is off | `gateway/run.py`, `cli.py` `_log_redaction_status` | The engine has no log; the setting is read where the tools are built. |

### 3. `search_files` over `regex-lite`

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

### 4. Secret redaction over `regex-lite`

The redaction passes of `agent/redact.py` (`redact_sensitive_text`) run on `regex-lite`, whose
`\d`, `\w` and case-insensitive matching are ASCII. Python's `re` also takes other Unicode
decimal digits for `\d` (Telegram bot ids, phone numbers, the digits of a Slack app token),
other Unicode letters and digits for `\w` (the scheme word of an `Authorization` header) and
Unicode case variants under `IGNORECASE` (`K` U+212A for `k`, `ſ` U+017F for `s`). Real
credentials, header schemes and key names are ASCII, so these shapes stay unmasked or are
masked differently only for text no service issues. `\s` is Python's, the four ASCII
separators and the Unicode spaces included.

### 5. Error results

- Every failed tool call sets the C4G `is_error` flag besides its reference text; the reference
  has no such flag.
- Rust tools return their failures as values and the release profile aborts on panic, so the
  built-in tools have no counterpart of an exception escaping a handler. The sanitised
  `[TOOL_ERROR] Tool execution failed: <kind>: <error>` form is `Output::exception`, for a tool
  that reports a failure it did not handle.

## Consequences

- A model that learned the reference tools calls the same names with the same arguments; the
  parameters of section 1 are accepted and ignored.
- Adding one of the parameters or behaviours above is a change of this record.
