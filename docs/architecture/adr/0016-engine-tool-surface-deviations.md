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
| Syntax checks and LSP diagnostics after `write_file` and `patch`, the `lint` and `lsp_diagnostics` result fields | `tools/file_operations.py` `write_file`, `_check_lint_delta` | They run external linters and language servers; the engine starts none. |
| Document conversion in `read_file` (notebooks, Office, PDF, EPUB) and the pointer to `vision_analyze` | `tools/file_tools.py` `READ_FILE_SCHEMA`, `read_file_tool` | No converters in the engine binary (ADR-0010 budgets); `vision_analyze` is not an engine tool. |
| ripgrep as the `search_files` backend | `tools/file_operations.py` `_search_with_rg`, `_search_files_rg` | Regular expressions use `regex-lite` (ADR-0013) over the engine's own walk. Consequences below. |
| Hints on a search with no match: case-insensitive, hidden-file and fixed-string probes | `tools/file_operations.py` `_zero_match_probe` | Each probe scans the tree again; the scan bounds of section 3 would apply three more times per empty search. |
| A `path` holding several paths separated by spaces or commas | `tools/file_operations.py` `_try_multi_path_search` | Not ported; such a path is reported as not found with similar names. |
| A `workdir` that does not exist: the wrapper's `cd` fails and the call ends with the shell's message and exit code 126 | `tools/environments/base.py` `_wrap_command` | The engine runs no wrapper script (see the shell state row); it starts the shell in that directory, so the start fails and the result is `Command execution failed: FileNotFoundError: [Errno 2] No such file or directory` with `exit_code` -1. |
| Three retries of a `terminal` call whose start failed, after 2, 4 and 8 s | `tools/terminal_tool.py` `terminal_tool` | A local start failure (missing directory, missing shell) does not heal within the backoff, which would hold the turn for 14 s; the failure is answered at once. |
| `terminal.timeout` (`TERMINAL_TIMEOUT`) and `tool_output.max_bytes` settings | `tools/terminal_tool.py` `_get_env_config`, `tools/tool_output_limits.py` | The engine config has no per-tool settings; the default timeout is 180 s and the output cap 50,000 characters, the reference defaults. |
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
