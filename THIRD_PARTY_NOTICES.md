# Third-party notices

`ulei` ports behaviour and algorithms from the projects below to Rust. Their licences require the
copyright and permission notice to travel with substantial portions of their code.

<a id="ref-agent"></a>

## Hermes Agent (notice id `ref-agent`)

- Upstream: https://github.com/NousResearch/hermes-agent (version 0.20.5 as pinned by the owner's
  deployment, plus the owner's local patches).
- Licence: MIT.
- Ported Rust files start with `// Port: THIRD_PARTY_NOTICES.md#ref-agent` and are listed in the
  table below together with the upstream paths they port. The id is neutral on purpose: Rust code
  outside `crates/runtime-adapters` must not contain the product name (AGENTS.md hard rule 8), and
  several upstream paths contain it, so paths live only in this table.

### Ported modules

| ulei file | upstream path | notes |
|---|---|---|
| `crates/engine/src/tools.rs` | `tools/registry.py` | `tool_error` and its 2048-character bound with `… [truncated]` |
| `crates/engine/src/tools.rs` | `model_tools.py` | `_sanitize_tool_error`, the `[TOOL_ERROR] ` form |
| `crates/engine/src/tools.rs` | `agent/conversation_loop.py` | `_invalid_tool_name_error_content`: the unknown-name and empty-name texts |
| `crates/engine/src/tools/terminal.rs` | `tools/terminal_tool.py` | `TERMINAL_SCHEMA`, `TERMINAL_TOOL_DESCRIPTION`, the command, timeout and workdir checks and the result dicts of `terminal_tool`, its spill fields and `truncation_note`, the redaction of its output and spill file, `_redact_terminal_error_text` |
| `crates/engine/src/tools/terminal.rs` | `tools/environments/base.py` | `_wait_for_process`: the interrupt and timeout markers with codes 130 and 124 |
| `crates/engine/src/tools/terminal.rs` | `tools/environments/local.py` | `_find_bash` and the `bash -c` of `_run_bash`; `_kill_process`: SIGTERM to the group, 1 s, SIGKILL, 2 s |
| `crates/engine/src/tools/terminal/collector.rs` | `tools/environments/base.py` | `_BoundedOutputCollector`, the spill path and its seven-day cleanup in `_wait_for_process`, the incremental UTF-8 decoding of `_drain` |
| `crates/engine/src/tools/terminal/collector.rs` | `tools/spill_safety.py` | `ensure_spill_dir` |
| `crates/engine/src/tools.rs` | `tools/spill_safety.py` | `open_exclusive`, `write_text_exclusive` |
| `crates/engine/src/tools/terminal/ansi.rs` | `tools/ansi_strip.py` | `strip_ansi` |
| `crates/engine/src/tools/terminal/exit_note.rs` | `tools/terminal_tool.py` | `_interpret_exit_code`, `_interpret_signal_exit`, `_SIGNAL_EXIT_NOTES` |
| `crates/engine/src/tools/terminal/hints.rs` | `tools/terminal_hints.py`, `tools/terminal_tool.py` | `_SCAN_CHARS`, the `_hint_*` functions in `_OUTPUT_HINTS` order, `_EXIT_CODE_HINTS`, `annotate_failure`, `annotate_masked_success` with `_READONLY_HEADS`, `_FAILURE_SHAPES`, `_MASKING_PIPE_RE`, `_MASKING_OR_RE`, `_first_token`; their gate on the exit code and `_interpret_exit_code` and the `hint` field in `terminal_tool` |
| `crates/engine/src/tools/terminal/lifecycle.rs` | `cron/lifecycle_guard.py`, `tools/terminal_tool.py` | `_GATEWAY_LIFECYCLE_PATTERN` branches C and D, `_SHELL_LINE_CONTINUATION`, `contains_gateway_lifecycle_command`, `_iter_command_segments`, `_command_token_index`, `_DATA_SINK_EXECUTABLES`, `_UNSAFE_DATA_ARG_MARKERS`, `_PIPE_TO_INTERPRETER`, `_mask_data_sink_arguments`, `_lifecycle_command_scan_with_data_exemption`, `_direct_lifecycle_scan`, `_iter_referenced_shell_scripts`, `_iter_shell_command_payloads`, `_read_referenced_script` with `_BINARY_MAGIC_PREFIXES`, `_sanitize_remote_script_text`, `_contains_unsafe_gateway_action`, `contains_gateway_lifecycle_command_or_referenced_script`, the size, depth and binary limits; the guard call of `terminal_tool` with its blocked result and the local read of `_read_script_in_env`; local patch 0017 (`_SYSTEMD_GATEWAY_UNIT_REF`, `_KILL_UNSAFE_CHARS`, `_KILL_SEGMENT_END`, `_match_targets_only_sibling_gateways`, `_scan_lifecycle_matches`); the tokenizer is CPython's `shlex.shlex` in POSIX mode, see [`psf`](#psf) |
| `crates/engine/src/tools/files.rs` | `tools/file_tools.py` | `WRITE_FILE_SCHEMA`, `PATCH_SCHEMA`, `_handle_write_file`, `_handle_patch`, `patch_tool` in both modes with its checks, the V4A header extraction and its `..` refusal, result fields and `_hint` texts, `_record_patch_failure`, `_reset_patch_failures`, the `write_file_tool` result and its refusal of `_is_internal_file_tool_content`, the dedup invalidation of `_update_read_timestamp` |
| `crates/engine/src/tools/files.rs` | `tools/file_operations.py` | `write_file`: atomic replace keeping the mode, the CRLF of `_detect_file_line_ending` and the byte order mark of `_file_has_bom` kept from the file it replaces, post-write verification, `WriteResult`; `patch_replace`, `_detect_line_ending`, `_normalize_line_endings`, `_strip_bom`, `_unified_diff` |
| `crates/engine/src/tools/v4a.rs` | `tools/patch_parser.py` | `parse_v4a_patch`, `_validate_operations` with its overlay of pending moves and deletes, `apply_v4a_operations`, `_apply_add`, `_apply_delete`, `_apply_move`, `_apply_update`, `_count_occurrences` |
| `crates/engine/src/tools/v4a.rs` | `tools/file_operations.py`, `tools/file_tools.py` | `patch_v4a`, `PatchResult.to_dict`, `read_file_raw` with `_is_image`, `IMAGE_EXTENSIONS`, `delete_file`, `move_file`; the path resolution of `_rewrite_v4a_patch_paths_for_host` |
| `crates/engine/src/tools/fuzzy.rs` | `tools/fuzzy_match.py` | `fuzzy_find_and_replace` and its nine strategies, `UNICODE_MAP`, `IDENTICAL_STRINGS_ERROR`, `is_already_applied`, `_format_match_locations`, `_detect_escape_drift`, `_detect_backslash_doubling`, `_reindent_replacement`, `_maybe_unescape_new_string`, `_preserve_unicode_in_replacement`, `_apply_replacements`, `_map_normalized_positions`, `_build_orig_to_norm_map`, `_map_positions_norm_to_orig`, `find_closest_lines`, `_visualize_whitespace`, `format_no_match_hint` |
| `crates/engine/src/tools/difflib.rs` | `tools/fuzzy_match.py`, `tools/file_operations.py`, `tools/patch_parser.py` | the calls of `difflib.SequenceMatcher` and `difflib.unified_diff`; the algorithms are those of CPython, see [`psf`](#psf) |
| `crates/engine/src/tools/read.rs` | `tools/file_tools.py` | `READ_FILE_SCHEMA`, `_handle_read_file`, `read_file_tool` with its guards, dedup and repeat answers, `_DEFAULT_MAX_READ_CHARS`, `_truncate_to_char_budget`, `_LARGE_FILE_HINT_BYTES`, `_BLOCKED_DEVICE_PATHS`, `_is_blocked_device_path`, `_is_blocked_device`, `_special_file_kind`, `_READ_DEDUP_STATUS_MESSAGE`, the `file_read` redaction of the content |
| `crates/engine/src/tools/read.rs` | `tools/file_operations.py`, `tools/binary_extensions.py` | `MAX_LINES`, `MAX_LINE_LENGTH`, `DEFAULT_READ_LIMIT`, `normalize_read_pagination`, `ShellFileOperations.read_file`, `_add_line_numbers`, `_strip_bom`, `_is_likely_binary_bytes`, `_try_read_utf16`, `_not_regular_error`, `_suggest_similar_files`, `describe_binary_file`, `identify_binary_bytes`, `_MAGIC_SIGNATURES`, `ReadResult.to_dict`; `BINARY_EXTENSIONS`, `has_binary_extension` |
| `crates/engine/src/tools/tracker.rs` | `tools/file_tools.py`, `model_tools.py` | `_read_tracker` (`last_key`, `consecutive`, `dedup`, `dedup_hits`) behind `_read_tracker_lock`, `_DEDUP_CAP` of `_cap_read_tracker_data`, `notify_other_tool_call`, `_invalidate_dedup_for_path`; `_READ_SEARCH_TOOLS` and the call of `notify_other_tool_call` in `handle_function_call` |
| `crates/engine/src/tools/batch.rs` | `agent/tool_dispatch_helpers.py` | `_NEVER_PARALLEL_TOOLS`, `_PARALLEL_SAFE_TOOLS`, `_PATH_SCOPED_READERS`, `_PATH_SCOPED_WRITERS`, `_plan_tool_batch_segments`, `_canonical_path`, `_extract_parallel_scope_paths`, `_paths_overlap`, the V4A targets of `_extract_file_mutation_targets` |
| `crates/engine/src/agent/dispatch.rs` | `agent/tool_executor.py`, `run_agent.py` | `_MAX_TOOL_WORKERS`, `_DEFAULT_CONCURRENT_TOOL_TIMEOUT_S`, `_max_workers_for_tool_batch`, `execute_tool_calls_concurrent` (results in call order, the per-call error and timeout texts, the batch deadline), `execute_tool_calls_segmented`; `_execute_tool_calls` |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py`, `agent/turn_finalizer.py`, `run_agent.py` | the bound `api_call_count < max_iterations` with retries and nudges counted as iterations; the order of the no-tool-call branch (post-tool nudge, thinking prefill, empty retry, `(empty)` end) before the stop policies; the pops of `_thinking_prefill` scaffolding on the tool-call path and of all scaffolding before a final reply; prefill stubs left out of every request and the adjacent user messages then merged, from `agent/agent_runtime_helpers.py` `drop_thinking_only_and_merge_users`; the reset of `_post_tool_empty_retried` after tools; `_EPHEMERAL_SCAFFOLDING_FLAGS` kept out of the store; the `(empty)` answer row of `finalize_turn` |
| `crates/engine/src/compress.rs` | `agent/context_compressor.py`, `agent/model_metadata.py` | the default path of `ContextCompressor.compress` with its abort on an access, quota or network failure; `_prune_old_tool_results` passes 1 to 4 with `_demote_tool_result_at`, `_truncate_tool_call_args_json`, `_protect_head_size`, `_effective_protect_first_n`, `_align_boundary_forward`, `_align_boundary_backward`, `_find_tail_cut_by_tokens`, `_ensure_last_user_message_in_tail`, `_ensure_last_assistant_message_in_tail`, `_find_turn_pair_end`, `_find_last_user_message_idx`, `_blank_echo_indices_after`, `_find_context_summaries`, `_strip_context_summary_handoff_message`, `_template_visible_role`, `_sanitize_tool_pairs`, `_compute_summary_budget`, `tail_token_budget`, `max_summary_tokens`, `_generate_summary` with its failure classification and cooldowns, `_record_compression_failure_cooldown`, `_MIN_SUMMARY_TOKENS`, `_SUMMARY_RATIO`, `_SUMMARY_TOKENS_CEILING`, `_PRUNE_MIN_CHARS`, `_MAX_TAIL_MESSAGE_FLOOR`, `_PRESSURE_KEEP_RECENT_MESSAGES`, `_CHARS_PER_TOKEN`; `_compute_threshold_tokens`, `_effective_threshold_percent`, `_apply_threshold_tokens_cap`, `_coerce_threshold_tokens_cap`, `MINIMUM_CONTEXT_LENGTH`, `_MIN_CTX_TRIGGER_RATIO`, `_SMALL_CTX_WINDOW_LIMIT`, `_SMALL_CTX_THRESHOLD_PERCENT`, `DEFAULT_FALLBACK_CONTEXT`; `update_from_response`, `should_compress`, `_automatic_compression_blocked`, `_record_ineffective_compression_verdict`, `record_completed_compaction`, `_ANTI_THRASH_RECOVERY_SECONDS`; `estimate_tokens_rough`, `estimate_messages_tokens_rough`, `estimate_request_tokens_rough` |
| `crates/engine/src/compress/text.rs` | `agent/context_compressor.py` | `SUMMARY_PREFIX`, `LEGACY_SUMMARY_PREFIX`, `COMPRESSION_CONTINUATION_USER_CONTENT`, `_SUMMARY_END_MARKER`, `_MERGED_PRIOR_CONTEXT_HEADER`, `_MERGED_SUMMARY_DELIMITER`, `_PRUNED_TOOL_PLACEHOLDER`, the summarizer prompt and template of `_generate_summary`, `_serialize_for_summary`, `_bound_summary_input`, `_derive_auto_focus_topic`, `_ground_historical_task_snapshot`, `_latest_user_task_snapshot`, `_validate_summary_user_provenance`, `_strip_summary_prefix`, `_with_summary_prefix`, `classify_summary_content`, `_is_synthetic_compression_user_turn`, `_build_static_fallback_summary`, `_summarize_tool_result`, `_redact_compaction_text` |
| `crates/engine/src/agent.rs` | `agent/turn_context.py` | `compose_user_api_content`: the plugin context appended to the API copy of a string user message after `\n\n`, sent at every step of the turn |
| `crates/engine/src/policy.rs` | `agent/turn_context.py` | the `pre_llm_call` contexts of `build_turn_context`: empty ones skipped, the others joined by `\n\n` in hook order |
| `crates/engine/src/policy/turn_reminder.rs` | profile plugin `turn-reminder` (`on_pre_llm_call`) | the owner's own plugin, not the reference agent; scope by the turn origin, text from `engine.turn_reminder_file` (ADR-0022) |
| `crates/engine/src/policy/no_ask_guard.rs` | profile plugin `no-ask-guard` (`on_turn_start`, `on_output`, `_check`, `_write_log`, `MAX_CONSECUTIVE`) and its classifier script `scripts/ask_guard.py` (`ends_with_question`, `_strip_markup`, `tail_of`, `prepare`, `_tag`, `_parse_obj`, `combine`, `classify`, `TAIL_CHARS`, `REPLY_PART_CHARS`, `TIMEOUT`) | the owner's own plugin, not the reference agent; scope by the turn origin, the note and the classifier instructions from `engine.no_ask_guard.*` files, the classifier through a qb route alias (ADR-0023) |
| `crates/engine/src/policy/tool_guardrails.rs` | `agent/tool_guardrails.py`, `run_agent.py`, `agent/tool_executor.py`, `agent/display.py`, `agent/agent_init.py` | `ToolCallGuardrailConfig` with its defaults and the `warnings_enabled`, `hard_stop_enabled`, `warn_after`, `hard_stop_after` and `loop_caps` keys of `from_mapping`; `LoopCapConfig` with `max_web_searches` and its default, `from_mapping` and `_non_negative_int` (0 is no limit), `_check_loop_cap` for `web_search` and the cap part of `before_call` ahead of the hard-stop check; `STALL_GUARD_IDENTICAL_CALL_THRESHOLD`, `IDENTICAL_RESULT_STUB_MIN_CHARS`, `_RESULT_STUB_ARGS_PREVIEW_CHARS`, `IdenticalCallObservation`, `observe_call` with its ordinal suffixes, `_build_result_reference_stub` without the spill-path line, the streak and cap counters of `reset_for_turn`; `agent.stall_guards` and `_stall_guards_enabled`; `IDEMPOTENT_TOOL_NAMES` and `MUTATING_TOOL_NAMES` limited to the engine's tools; `ToolCallSignature`, `canonical_tool_args`, `ToolGuardrailDecision.to_metadata`, `ToolCallGuardrailController.reset_for_turn`, `before_call`, `after_call`, `_is_idempotent`, `toolguard_synthetic_result`, `append_toolguard_guidance`, `_tool_failure_recovery_hint`, `_result_hash`, `_sha256`; `_append_guardrail_observation`, `_guardrail_block_result`, `_set_tool_guardrail_halt`, `_toolguard_controlled_halt_response`; the `before_call` block path of the tool executor; the failure signal of `_detect_tool_failure` mapped onto `is_error` and the `terminal` exit code (ADR-0016 section 12) |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py`, `agent/turn_context.py` | the `guardrail_halt` exit after a tool batch with the controlled halt answer as the assistant reply; `reset_for_turn` of the guardrails at the start of a turn |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py` | `_DROPPED_TOOLCALL_NUDGE_CONTENT` and the dropped tool-call re-prompt: a `tool_use` stop without a call nudged at most 3 times in a row, the reply and the nudge kept out of the store and popped before a final reply, the count reset after tool calls and at the end of the turn |
| `crates/engine/src/agent/empty.rs` | `agent/conversation_loop.py` | `_EMPTY_TOOL_RESPONSE_NUDGE`, the post-tool nudge `tool -> assistant("(empty)") -> user(nudge)` with its five-message look-back, `_has_inline_thinking`, the thinking-only prefill (2), the empty retry and its backoff, the `(empty)` terminal |
| `crates/engine/src/agent/empty.rs` | `agent/empty_response_guard.py`, `agent/retry_utils.py` | `DEFAULT_EMPTY_RETRY_BUDGET`, `EmptyAttempt`, `record_empty_attempt`, `_zero_output`, `deterministic_empty`, `guard_enabled`; `jittered_backoff` with `base_delay=5.0`, `max_delay=60.0` |
| `crates/engine/src/agent/think.rs` | `agent/agent_runtime_helpers.py`, `run_agent.py`, `agent/chat_completion_helpers.py` | `_REASONING_TAG_NAMES`, `_TOOL_CALL_TAG_NAMES` and the patterns of `strip_think_blocks`; `_has_content_after_think_block`; the `<think>` strip of `handle_max_iterations` |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py`, `agent/conversation_compression.py` | the compaction gate before an API call on the last reported prompt tokens, a rough request estimate without one and none right after a compaction; the context-overflow path: one more compaction attempt, the `Context length exceeded: max compression attempts (N) reached.` and `Context length exceeded (N tokens). Cannot compress further.` ends, the progress check on messages and rough tokens, the 2 s wait before the retry; `compression_attempts` shared per turn and re-armed by `_should_rearm_compression_budget`; the reset of the empty-reply and dropped-call recovery after a compaction; `_compress_context` replacing the live messages with the compressed set in place |
| `crates/engine/src/agent/summary.rs` | `agent/chat_completion_helpers.py`, `agent/context_compressor.py` | `handle_max_iterations`: the summary request as a user message, the call without tools, one more call for an empty reply, its fallback texts and the history it leaves; `MAX_ITERATIONS_SUMMARY_REQUEST` |
| `crates/engine/src/config.rs` | `hermes_cli/config.py`, `agent/empty_response_guard.py` | `resolve_turn_limit`, `_UNLIMITED_SPELLINGS`, `TURN_LIMIT_UNLIMITED` for `agent.max_turns`; the `enabled` switch of `agent.empty_response_guard` |
| `crates/engine/src/tools/safety.rs` | `agent/file_safety.py` | `_BLOCKED_PROJECT_ENV_BASENAMES`, the project environment-file block of `get_read_block_error` |
| `crates/engine/src/tools/search.rs` | `tools/file_tools.py` | `SEARCH_FILES_SCHEMA`, `_handle_search_files`, `search_tool` with its repeat count, BLOCKED guard and `_warning`, its read block of the search path, `_filter_read_blocked_search_results` and `_omitted`, its truncation hint and the `file_read` redaction of match content |
| `crates/engine/src/tools/search.rs` | `tools/file_operations.py` | `search`, `_search_files`, `_search_files_rg`, `_search_with_rg`, `normalize_search_pagination` with `_coerce_int`, `SearchResult.to_dict` |
| `crates/engine/src/tools/redact.rs` | `agent/redact.py` | `redact_sensitive_text` and its pattern families, `mask_secret`, `_mask_token`, `_mask_token_nonreusable`, `_mask_control_split_tokens`, `_key_has_secret_keyword`, `_redact_query_string`, `_redact_form_body`, `_redact_strict_url_credentials`, `_canonical_url_param_name`, `redact_terminal_output`, `is_env_dump_command`, `_command_reads_env_file` |
| `crates/engine/src/config.rs` | `agent/redact.py`, `hermes_cli/main.py` | `_REDACT_ENABLED`: the values that turn redaction on, the environment winning over `security.redact_secrets` |
| `crates/engine/src/config.rs` | `agent/deadline.py`, `agent/tool_executor.py` | `clamp_timeout`, `MAX_SAFE_TIMEOUT_S`, the environment value of `resolve_timeout` read for `_resolve_concurrent_tool_timeout` |
| `crates/engine/src/prompt.rs` | `agent/system_prompt.py` | `build_system_prompt_parts`: the stable, context and volatile tiers, their order and join, the place of the skills index, the `Conversation started:` line with its date format and zone suffix; `build_system_prompt` |
| `crates/engine/src/prompt.rs` | `agent/prompt_builder.py`, `tools/memory_tool.py` | `TASK_COMPLETION_GUIDANCE`, `PARALLEL_TOOL_CALL_GUIDANCE`, the local host block of `build_environment_hints`; the `MemoryStore` defaults `memory_char_limit` 2200 and `user_char_limit` 1375 |
| `crates/engine/src/prompt.rs` | `agent/conversation_loop.py` | `_restore_or_build_system_prompt`: the missing, null, empty and present states and the reuse with history; the working-directory check of `_stored_prompt_matches_runtime` with `host_info_value` and its scan of each `User home directory:` anchor |
| `crates/engine/src/prompt/skills.rs` | `agent/prompt_builder.py`, `agent/skill_utils.py` | `build_skills_system_prompt`, `_build_skills_system_prompt_inner` (the `## Skills (mandatory)` text, the category and skill order, the dedup by name), `_parse_skill_file`, `_build_snapshot_entry` (skill name and category from the path); `extract_skill_description` with `SKILL_PROMPT_DESC_LIMIT`, the `DESCRIPTION.md` pass |
| `crates/engine/src/skills.rs` | `agent/skill_utils.py`, `tools/skills_tool.py` | `EXCLUDED_SKILL_DIRS`, `SKILL_SUPPORT_DIRS`, `iter_skill_index_files`, `is_skill_support_path`, `is_excluded_skill_path`, `PLATFORM_MAP`, `skill_matches_platform_list`, `skill_matches_platform`; the `utf-8-sig` and `errors="replace"` reads |
| `crates/engine/src/skills/frontmatter.rs` | `agent/skill_utils.py` | `parse_frontmatter`: the byte order mark strip, the `---` fences, the fallback for YAML it cannot parse; the YAML subset of ADR-0016 section 10 |
| `crates/engine/src/tools/skills.rs` | `tools/skills_tool.py`, `tools/path_security.py` | `SKILLS_LIST_SCHEMA`, `SKILL_VIEW_SCHEMA`, `MAX_NAME_LENGTH`, `MAX_DESCRIPTION_LENGTH`, `_skill_lookup_path_error`, `_find_all_skills`, `_get_category_from_path`, `_parse_tags`, `_sort_skills`, `skills_list`, `skill_view` (the lookup strategies, the ambiguity, not-found, platform and disabled answers, the linked files, the `..` refusal and the missing-file listing), `_skill_view_with_bump`, `_SKILL_VIEW_DEDUP_CAP`, `_SKILL_VIEW_DEDUP_MESSAGE`, `_record_skill_view`, `_check_skill_view_dedup`, `_skill_view_fingerprint`; `validate_within_dir`, `has_traversal_component`; `agent/skill_utils.py` `is_valid_namespace`, `parse_qualified_name` |
| `crates/engine/src/config.rs` | `agent/skill_utils.py` | `get_disabled_skill_names` with `_normalize_string_set`: the disabled names stripped, blank ones dropped |
| `crates/engine/src/prompt/context.rs` | `agent/prompt_builder.py` | `load_soul_md`, `build_context_files_prompt`, `_find_git_root`, `_agents_md_directory_chain`, `_load_agents_md`, `_load_claude_md`, `_load_cursorrules`, `_truncate_content` with `CONTEXT_FILE_MAX_CHARS`, `CONTEXT_TRUNCATE_HEAD_RATIO`, `CONTEXT_TRUNCATE_TAIL_RATIO` and its marker, the byte order mark strip of `_scan_context_content` |
| `crates/engine/src/prompt/memory.rs` | `tools/memory_tool.py` | `MEMORY_BLOCK_HEADERS`, `ENTRY_DELIMITER`, `_render_block`, `_parse_entries`, the `utf-8-sig` read of `_read_raw_checked`, the entry dedup of `load_from_disk` |
| `crates/engine/src/config.rs` | `tools/memory_tool.py`, `hermes_time.py` | the `memory.memory_char_limit` and `memory.user_char_limit` settings, the `timezone` setting of `_resolve_timezone_name` |
| `crates/engine/src/tools/web.rs` | `tools/web_tools.py` | `WEB_SEARCH_SCHEMA`, `WEB_EXTRACT_SCHEMA` and their registered handlers (`urls[:5]`); the `limit` clamp of `web_search_tool`; `DEFAULT_EXTRACT_CHAR_LIMIT`, `_get_extract_char_limit` and the `char_limit` clamp of `web_extract_tool`; the `Interrupted` answer and the `Error searching web: ` and `Error extracting content: ` prefixes |
| `crates/engine/src/tools/web.rs` | `agent/tool_dispatch_helpers.py` | `_UNTRUSTED_TOOL_NAMES`, `_UNTRUSTED_WRAP_MIN_CHARS`, `_DELIMITER_TOKEN_RE`, `_neutralize_delimiters`, `_maybe_wrap_untrusted` |
| `crates/engine/src/web.rs` | `tools/web_tools.py` | the result of `web_search_tool`; `web_extract_tool`: the secret and credential pre-checks with their refusals, the SSRF filter, the reordering around invalid and refused items, the trimmed result, `Content was inaccessible or not found`, the final base64 sweep; `_web_extract_url` |
| `crates/engine/src/web.rs` | `plugins/web/exa/provider.py`, `agent/web_search_provider.py`, `hermes_cli/config.py` | `ExaWebSearchProvider.search` and `.extract` with their result and failure shapes, the missing-key error of `_get_exa_client`; `get_provider_env`; `load_env`, `_parse_env_value`; the request fields and the `Request failed with status code` text of the Exa SDK `exa_py` 2.10.2 (MIT) the provider calls |
| `crates/engine/src/web/url.rs` | `tools/url_safety.py` | `normalize_url_for_request`, `_SENSITIVE_QUERY_PARAM_NAMES`, `sensitive_query_param_name`, `_BLOCKED_HOSTNAMES`, `_CGNAT_NETWORK`, `_is_blocked_ip`, `is_safe_url`; CPython's `urlsplit`, `urlunsplit`, `quote`, `unquote`, `parse_qsl`, the `idna` codec and the `ipaddress` ranges, see [`psf`](#psf) |
| `crates/engine/src/web/page.rs` | `tools/web_tools.py` | `convert_base64_images_to_links`, `_truncate_with_footer`, `_store_full_text`, `MAX_STORED_TEXT_CHARS` |
| `crates/engine/src/config.rs` | `tools/web_tools.py` | the `web.extract_char_limit` setting |
| `crates/engine/src/journal.rs` | `hermes_state_common.py`, `hermes_state.py`, `agent/conversation_compression.py` | the `active` and `compacted` message columns with their defaults and the live-message index; `archive_and_compact` without its watermark, lock holder, model-config patch and counters: the live rows archived as `active=0, compacted=1` and the compacted set inserted as live rows in one transaction; the `active = 1` filter of the live-context load and `include_inactive` of `get_messages`; the in-place branch of `_compress_context` keeping the session id (ADR-0014 section 6) |
| `crates/channels/src/bot_api/config.rs` | `plugins/platforms/telegram/adapter.py` | `base_url`/`base_file_url` handling and `_max_doc_bytes` of `TelegramAdapter.__init__`/`connect`, HTTP timeouts of `connect`, `disable_link_previews` |
| `crates/channels/src/bot_api/error.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py`, `agent/redact.py` | `_redact_telegram_error_text`, `_looks_like_polling_conflict`, `_looks_like_auth_error`, `_looks_like_network_error`, `_looks_like_connect_timeout`, `_is_bad_request_error`, `_is_transient_typing_error`; `_is_retryable_error`, `_is_timeout_error`, `_RETRYABLE_ERROR_PATTERNS`; `_TELEGRAM_RE` of `redact_sensitive_text` |
| `crates/channels/src/bot_api/client.rs` | `plugins/platforms/telegram/adapter.py` | Bot API calls of `connect`, `_delete_webhook_best_effort`, `send`, `send_typing`, `_set_reaction`, `_clear_reactions`, `send_document`, `send_voice`, media download paths, `_link_preview_kwargs`, `_notification_kwargs`, caption cut |
| `crates/channels/src/bot_api/types.rs` | `plugins/platforms/telegram/adapter.py` | request fields of the calls above |
| `crates/channels/src/bot_api/poller.rs` | `plugins/platforms/telegram/adapter.py` | `_start_polling_resilient`, `_start_polling_once`, `_handle_polling_network_error`, `_handle_polling_conflict`, `_record_polling_progress` counters; `allowed_updates` |
| `crates/channels/src/bot_api/sender.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | send attempt loop of `TelegramAdapter.send`, `send_typing`, `_record_typing_cooldown`, `_typing_in_cooldown`; `BasePlatformAdapter._send_with_retry` and its delivery-failure notice |
| `crates/channels/src/ingress/update.rs` | `plugins/platforms/telegram/adapter.py` (`_effective_update_message`, handler registration) | fields read by the ingress |
| `crates/channels/src/ingress/text.rs` | `plugins/platforms/telegram/adapter.py` (`_telegram_message_plain_text`, `_telegram_custom_emoji_text`, `_telegram_rich_message_text`, `_rich_blocks_to_plain`, `_extract_rich_reply_text`, `_clean_bot_trigger_text`) | local patches 0011, 0012 |
| `crates/channels/src/ingress/forward.rs` | `plugins/platforms/telegram/adapter.py` (`_forward_label`, `_forward_person_label`, `_forward_chat_label`, `_render_forward_batch`) | local patches 0001, 0016 |
| `crates/channels/src/ingress/media.rs` | `plugins/platforms/telegram/adapter.py` (`_telegram_media_size_allowed`, `_telegram_media_too_large_note`, `_surface_media_cache_failure`); `gateway/platforms/base.py` (`cache_*_from_bytes`, `cache_media_bytes`, type tables); `tools/audio_container.py` | |
| `crates/channels/src/ingress/event.rs` | `plugins/platforms/telegram/adapter.py` (`_enqueue_text_event`, `_can_coalesce`, `_merge_coalesced_parts`, `_enqueue_photo_event`); `gateway/platforms/base.py` (`_merge_caption`, `is_command`) | local patch 0010 |
| `crates/channels/src/ingress/coalesce.rs` | `plugins/platforms/telegram/adapter.py` (`_flush_text_batch`, `_dispatch_inbound`, `_flush_turn_coalesce`, `_flush_turn_coalesce_now`, `_flush_photo_batch`, `_queue_media_group_event`, `_flush_media_group_event`) | local patch 0010; deadlines instead of tasks |
| `crates/channels/src/ingress/prepare.rs` | `gateway/run.py` (`_prepare_inbound_message_text`, `_enrich_message_with_transcription`, `_build_document_context_note`, `_format_duration`) | local patch 0001 (forward label above transcripts) |
| `crates/channels/src/ingress/reaction.rs` | `plugins/platforms/telegram/adapter.py` (`_normalize_reaction_event`); profile plugin `reactions-log` | ADR-0018 |
| `crates/channels/src/ingress/command.rs` | `gateway/slash_commands.py` (`_handle_reset_command`, `_handle_stop_command`, `_handle_status_command`, `_handle_model_command`); `gateway/platforms/base.py` (`get_command`); `locales/ru.yaml` | ADR-0018 |
| `crates/channels/src/ingress/pipeline.rs` | `plugins/platforms/telegram/adapter.py` (`_is_user_authorized_from_message`, `_build_message_event`, `_handle_text_message`, `_handle_command`, `_handle_location_message`, `_handle_media_message`, `_handle_sticker`, `_cache_replied_media`, `_handle_unhandled_message`); `gateway/sticker_cache.py` | local patch 0011 |
| `crates/channels/src/render/markdown.rs` | `plugins/platforms/telegram/adapter.py` | `TelegramAdapter.format_message`, `_escape_mdv2`, `_TG_EMOJI_RE` (outbound custom emoji) |
| `crates/channels/src/render/plain.rs` | `plugins/platforms/telegram/adapter.py` | `_strip_mdv2` |
| `crates/channels/src/render/tables.rs` | `gateway/platforms/helpers.py`, `agent/markdown_tables.py` | `convert_table_to_bullets`, `_render_table_block`, `TABLE_SEPARATOR_RE`, `is_table_row`, `split_table_row` |
| `crates/channels/src/render/split.rs` | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | `BasePlatformAdapter.truncate_message`, `utf16_len`, `_custom_unit_to_cp`; chunk post-processing of `TelegramAdapter.send`, `_separate_chunk_indicator_from_fence` |
| `crates/channels/src/render/send.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | send decisions of `TelegramAdapter.send`, `_should_thread_reply`, `_link_preview_kwargs`, `_notification_kwargs` |
| `crates/channels/src/render/typing.rs` | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | `_keep_typing`, `_stop_typing_refresh`, `pause_typing_for_chat`; `send_typing` cooldown, `_is_transient_typing_error`, `_record_typing_cooldown` |
| `crates/channels/src/render/pytext.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | semantics of the `str` and `re` primitives (`isspace`, `strip`, `\s`, `\w`, `re.sub` scanning) the functions above rely on |

### Licence text

```
MIT License

Copyright (c) 2025 Nous Research

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

<a id="psf"></a>

## Python standard library (notice id `psf`)

- Upstream: https://github.com/python/cpython, module `Lib/difflib.py`.
- Licence: Python Software Foundation License Version 2.
- `crates/engine/src/tools/difflib.rs` ports `SequenceMatcher` (`find_longest_match`,
  `get_matching_blocks`, `get_opcodes`, `ratio`, the popular-element junking of `__chain_b`),
  `get_grouped_opcodes`, `unified_diff` and `_format_range_unified`.
- Summary of the changes: rewritten in Rust over slices of characters or lines; no `isjunk`
  callback and no `autojunk` switch (always on, as the reference calls it); `unified_diff` takes
  no dates and joins its lines into one string.

### Licence text

```
PYTHON SOFTWARE FOUNDATION LICENSE VERSION 2
--------------------------------------------

1. This LICENSE AGREEMENT is between the Python Software Foundation
("PSF"), and the Individual or Organization ("Licensee") accessing and
otherwise using this software ("Python") in source or binary form and
its associated documentation.

2. Subject to the terms and conditions of this License Agreement, PSF hereby
grants Licensee a nonexclusive, royalty-free, world-wide license to reproduce,
analyze, test, perform and/or display publicly, prepare derivative works,
distribute, and otherwise use Python alone or in any derivative version,
provided, however, that PSF's License Agreement and PSF's notice of copyright,
i.e., "Copyright (c) 2001, 2002, 2003, 2004, 2005, 2006, 2007, 2008, 2009, 2010,
2011, 2012, 2013, 2014, 2015, 2016, 2017, 2018, 2019, 2020, 2021, 2022, 2023 Python Software Foundation;
All Rights Reserved" are retained in Python alone or in any derivative version
prepared by Licensee.

3. In the event Licensee prepares a derivative work that is based on
or incorporates Python or any part thereof, and wants to make
the derivative work available to others as provided herein, then
Licensee hereby agrees to include in any such work a brief summary of
the changes made to Python.

4. PSF is making Python available to Licensee on an "AS IS"
basis.  PSF MAKES NO REPRESENTATIONS OR WARRANTIES, EXPRESS OR
IMPLIED.  BY WAY OF EXAMPLE, BUT NOT LIMITATION, PSF MAKES NO AND
DISCLAIMS ANY REPRESENTATION OR WARRANTY OF MERCHANTABILITY OR FITNESS
FOR ANY PARTICULAR PURPOSE OR THAT THE USE OF PYTHON WILL NOT
INFRINGE ANY THIRD PARTY RIGHTS.

5. PSF SHALL NOT BE LIABLE TO LICENSEE OR ANY OTHER USERS OF PYTHON
FOR ANY INCIDENTAL, SPECIAL, OR CONSEQUENTIAL DAMAGES OR LOSS AS
A RESULT OF MODIFYING, DISTRIBUTING, OR OTHERWISE USING PYTHON,
OR ANY DERIVATIVE THEREOF, EVEN IF ADVISED OF THE POSSIBILITY THEREOF.

6. This License Agreement will automatically terminate upon a material
breach of its terms and conditions.

7. Nothing in this License Agreement shall be deemed to create any
relationship of agency, partnership, or joint venture between PSF and
Licensee.  This License Agreement does not grant permission to use PSF
trademarks or trade name in a trademark sense to endorse or promote
products or services of Licensee, or any third party.

8. By copying, installing or otherwise using Python, Licensee
agrees to be bound by the terms and conditions of this License
Agreement.
```
