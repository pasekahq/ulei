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
| `crates/engine/src/tools/spill.rs` | `tools/tool_result_storage.py`, `tools/budget_config.py`, `agent/tool_executor.py` | `maybe_persist_tool_result` (host-side path), `enforce_turn_budget`, `generate_preview`, `_safe_result_filename`, `_build_persisted_message`, `PERSISTED_OUTPUT_TAG`, `SPILLOVER_MAX_AGE_HOURS`; `DEFAULT_RESULT_SIZE_CHARS`, `DEFAULT_TURN_BUDGET_CHARS`, `DEFAULT_PREVIEW_SIZE_CHARS`, `DEFAULT_MCP_RESULT_SIZE_CHARS`, `PINNED_THRESHOLDS`, `BudgetConfig.resolve_threshold`, `budget_for_context_window`, `_configured_mcp_result_size`; the per-result and per-step calls of `execute_tool_calls_concurrent` and `execute_tool_calls_sequential` (ADR-0016 section 3) |
| `crates/engine/src/agent/dispatch.rs` | `agent/tool_executor.py`, `run_agent.py` | `_MAX_TOOL_WORKERS`, `_DEFAULT_CONCURRENT_TOOL_TIMEOUT_S`, `_max_workers_for_tool_batch`, `execute_tool_calls_concurrent` (results in call order, the per-call error and timeout texts, the batch deadline), `execute_tool_calls_segmented`; `_execute_tool_calls` |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py`, `agent/turn_finalizer.py`, `run_agent.py` | the bound `api_call_count < max_iterations` with retries and nudges counted as iterations; the order of the no-tool-call branch (post-tool nudge, thinking prefill, empty retry, `(empty)` end) before the stop policies; the pops of `_thinking_prefill` scaffolding on the tool-call path and of all scaffolding before a final reply; prefill stubs left out of every request and the adjacent user messages then merged, from `agent/agent_runtime_helpers.py` `drop_thinking_only_and_merge_users`; the reset of `_post_tool_empty_retried` after tools; `_EPHEMERAL_SCAFFOLDING_FLAGS` kept out of the store; the `(empty)` answer row of `finalize_turn` |
| `crates/engine/src/compress.rs` | `agent/context_compressor.py`, `agent/model_metadata.py` | the default path of `ContextCompressor.compress` with its abort on an access, quota or network failure; `_prune_old_tool_results` passes 1 to 4 with `_demote_tool_result_at`, `_truncate_tool_call_args_json`, `_protect_head_size`, `_effective_protect_first_n`, `_align_boundary_forward`, `_align_boundary_backward`, `_find_tail_cut_by_tokens`, `_ensure_last_user_message_in_tail`, `_ensure_last_assistant_message_in_tail`, `_find_turn_pair_end`, `_find_last_user_message_idx`, `_blank_echo_indices_after`, `_find_context_summaries`, `_strip_context_summary_handoff_message`, `_template_visible_role`, `_sanitize_tool_pairs`, `_compute_summary_budget`, `tail_token_budget`, `max_summary_tokens`, `_generate_summary` with its failure classification and cooldowns, `_record_compression_failure_cooldown`, `_MIN_SUMMARY_TOKENS`, `_SUMMARY_RATIO`, `_SUMMARY_TOKENS_CEILING`, `_PRUNE_MIN_CHARS`, `_MAX_TAIL_MESSAGE_FLOOR`, `_PRESSURE_KEEP_RECENT_MESSAGES`, `_CHARS_PER_TOKEN`; `_compute_threshold_tokens`, `_effective_threshold_percent`, `_apply_threshold_tokens_cap`, `_coerce_threshold_tokens_cap`, `MINIMUM_CONTEXT_LENGTH`, `_MIN_CTX_TRIGGER_RATIO`, `_SMALL_CTX_WINDOW_LIMIT`, `_SMALL_CTX_THRESHOLD_PERCENT`, `DEFAULT_FALLBACK_CONTEXT`; `update_from_response`, `should_compress`, `_automatic_compression_blocked`, `_record_ineffective_compression_verdict`, `record_completed_compaction`, `_ANTI_THRASH_RECOVERY_SECONDS`; `estimate_tokens_rough`, `estimate_messages_tokens_rough`, `estimate_request_tokens_rough` |
| `crates/engine/src/compress/text.rs` | `agent/context_compressor.py` | `SUMMARY_PREFIX`, `LEGACY_SUMMARY_PREFIX`, `COMPRESSION_CONTINUATION_USER_CONTENT`, `_SUMMARY_END_MARKER`, `_MERGED_PRIOR_CONTEXT_HEADER`, `_MERGED_SUMMARY_DELIMITER`, `_PRUNED_TOOL_PLACEHOLDER`, the summarizer prompt and template of `_generate_summary`, `_serialize_for_summary`, `_bound_summary_input`, `_derive_auto_focus_topic`, `_ground_historical_task_snapshot`, `_latest_user_task_snapshot`, `_validate_summary_user_provenance`, `_strip_summary_prefix`, `_with_summary_prefix`, `classify_summary_content`, `_is_synthetic_compression_user_turn`, `_build_static_fallback_summary`, `_summarize_tool_result`, `_redact_compaction_text` |
| `crates/engine/src/agent.rs` | `agent/error_classifier.py`, `agent/conversation_loop.py` | the `thinking_signature` class of `classify_api_error` (a 400 whose lowercased message names `thinking` with `signature`, `cannot be modified`, `must remain as they were`, or `redacted_thinking` and `invalid`) and its recovery: the step sent again at once, one time, with the thinking blocks stripped from the request copy only, never from the stored conversation (`thinking_sig_retry_attempted`) |
| `crates/engine/src/agent.rs` | `agent/turn_context.py` | `compose_user_api_content`: the plugin context appended to the API copy of a string user message after `\n\n`, sent at every step of the turn |
| `crates/engine/src/policy.rs` | `agent/turn_context.py` | the `pre_llm_call` contexts of `build_turn_context`: empty ones skipped, the others joined by `\n\n` in hook order after the caller's C6 `context` (ADR-0031 section 4) |
| `crates/engine/src/agent.rs` | `run_agent.py`, `agent/conversation_loop.py` | interim assistant messages: `_emit_interim_assistant_message` after the assistant message with tool calls is appended and before its calls run, `_interim_assistant_visible_text`, `_normalize_interim_visible_text`, `_interim_text_was_delivered`, `_record_delivered_interim_text` with the delivered texts of one turn (ADR-0031 section 3) |
| `crates/engine/src/policy/turn_reminder.rs` | profile plugin `turn-reminder` (`on_pre_llm_call`) | the owner's own plugin, not the reference agent; scope by the turn origin, text from `engine.turn_reminder_file` (ADR-0022) |
| `crates/engine/src/policy/no_ask_guard.rs` | profile plugin `no-ask-guard` (`on_turn_start`, `on_output`, `_check`, `_write_log`, `MAX_CONSECUTIVE`) and its classifier script `scripts/ask_guard.py` (`ends_with_question`, `_strip_markup`, `tail_of`, `prepare`, `_tag`, `_parse_obj`, `combine`, `classify`, `TAIL_CHARS`, `REPLY_PART_CHARS`, `TIMEOUT`) | the owner's own plugin, not the reference agent; scope by the turn origin, the note and the classifier instructions from `engine.no_ask_guard.*` files, the classifier through a qb route alias (ADR-0023) |
| `crates/engine/src/policy/hardline.rs` | `tools/approval.py`, `tools/terminal_tool.py` | `HARDLINE_PATTERNS` with their descriptions and `re.IGNORECASE \| re.DOTALL`, `_CMDPOS`, `_RM_FLAG_PREFIX`, `_hardline_rm_path`, `_HARDLINE_SYSTEM_DIRS`; `detect_hardline_command` with the steps of the owner's patch 0018 (`_MALFORMED_CMDPOS_SUBST_RE`, `_MALFORMED_SPLIT_RE`, `_MALFORMED_WRAPPER_RE`, `_malformed_crude_variants`, the warn and block modes and the warning line); `_command_parser_limit_exceeded` with `_MAX_DETECTION_COMMAND_CHARS`, `_MAX_SEPARATOR_FREE_COMMAND_CHARS`, `_MAX_DETECTION_SEGMENTS`, `_PARSER_LIMIT_DESCRIPTION` and `_MALFORMED_EXEC_DESCRIPTION`; `_normalize_command_for_detection` with the fullwidth part of its NFKC, `_rewrite_resolved_user_home`, `_fold_home_prefixes`, `_home_prefix_fold_regex`, `_PATH_TOKEN_STOP`, `_PATH_TAIL`; `_hardline_block_result`, `_save_blocked_payload`; the hardline call of `check_all_command_guards` and the blocked result of `terminal_tool` (ADR-0016 section 14) |
| `crates/engine/src/policy/hardline/shell.rs` | `tools/approval.py` | `_command_detection_variants` with its drive-path variant; `_mask_quoted_newlines`, `_mark_command_starts`, `_iter_shell_command_starts`, `_skip_shell_whitespace`, `_scan_dollar_paren_end`, `_scan_backtick_end`, `_read_shell_word`, `_iter_shell_command_word_spans` with `_COMMAND_WRAPPER_WORDS`, `_SUDO_OPTIONS_WITH_ARG` and `_ENV_ASSIGNMENT_RE`; `_deobfuscate_shell_word_for_detection`, `_replace_simple_shell_expansions`, `_replace_simple_command_substitutions`, `_literal_command_substitution_output`, `_is_simple_shell_literal`, `_SIMPLE_SHELL_LITERAL_RE`, `_PARAM_REPLACEMENT_RE`, `_PARAM_DEFAULT_RE`, `_strip_shell_word_syntax`; `_iter_top_level_shell_segments`, `_shell_tokens_with_spans`, `_quoted_grep_pattern_spans`, `_GREP_OPTIONS_WITH_ARG`, `_GREP_SHORT_OPTIONS_WITH_ARG`, `_grep_safe_detection_variant`; the payloads of `_execution_flag_findings` through `_shell_segment_tokens`, `_bash_exec_payload` with `_BASH_OPTIONS_WITH_ARG` and `_BASH_SHORT_OPTION_LETTERS`, `_read_tool_exec_flag` with `_split_option`, `_READ_TOOL_EXEC_FLAGS`, `_READ_TOOL_LONG_OPTIONS_WITH_ARG` and `_READ_TOOL_SHORT_OPTIONS_WITH_ARG` (ADR-0016 section 14) |
| `crates/engine/src/policy/tool_guardrails.rs` | `agent/tool_guardrails.py`, `run_agent.py`, `agent/tool_executor.py`, `agent/display.py`, `agent/agent_init.py` | `ToolCallGuardrailConfig` with its defaults and the `warnings_enabled`, `hard_stop_enabled`, `warn_after`, `hard_stop_after` and `loop_caps` keys of `from_mapping`; `LoopCapConfig` with `max_web_searches` and its default, `from_mapping` and `_non_negative_int` (0 is no limit), `_check_loop_cap` for `web_search` and the cap part of `before_call` ahead of the hard-stop check; `STALL_GUARD_IDENTICAL_CALL_THRESHOLD`, `IDENTICAL_RESULT_STUB_MIN_CHARS`, `_RESULT_STUB_ARGS_PREVIEW_CHARS`, `IdenticalCallObservation`, `observe_call` with its ordinal suffixes, `_build_result_reference_stub` without the spill-path line, the streak and cap counters of `reset_for_turn`; `agent.stall_guards` and `_stall_guards_enabled`; `IDEMPOTENT_TOOL_NAMES` and `MUTATING_TOOL_NAMES` limited to the engine's tools; `ToolCallSignature`, `canonical_tool_args`, `ToolGuardrailDecision.to_metadata`, `ToolCallGuardrailController.reset_for_turn`, `before_call`, `after_call`, `_is_idempotent`, `toolguard_synthetic_result`, `append_toolguard_guidance`, `_tool_failure_recovery_hint`, `_result_hash`, `_sha256`; `_append_guardrail_observation`, `_guardrail_block_result`, `_set_tool_guardrail_halt`, `_toolguard_controlled_halt_response`; the `before_call` block path of the tool executor; the failure signal of `_detect_tool_failure` mapped onto `is_error` and the `terminal` exit code (ADR-0016 section 12) |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py`, `agent/turn_context.py` | the `guardrail_halt` exit after a tool batch with the controlled halt answer as the assistant reply; `reset_for_turn` of the guardrails at the start of a turn |
| `crates/engine/src/agent.rs` | `agent/conversation_loop.py` | `_DROPPED_TOOLCALL_NUDGE_CONTENT` and the dropped tool-call re-prompt: a `tool_use` stop without a call nudged at most 3 times in a row, the reply and the nudge kept out of the store and popped before a final reply, the count reset after tool calls and at the end of the turn |
| `crates/engine/src/agent/empty.rs` | `agent/conversation_loop.py` | `_HOUSEKEEPING_TOOLS`, `_last_content_with_tools` and `_last_content_tools_all_housekeeping`: the visible text of a step whose calls are all housekeeping answers an empty reply before the nudge, a step with another tool clears it, a compaction resets it; `_STALE_MARKER_RE`, the bare tool-call marker dropped from the content of a step with calls; `_EMPTY_TOOL_RESPONSE_NUDGE`, the post-tool nudge `tool -> assistant("(empty)") -> user(nudge)` with its five-message look-back, `_has_inline_thinking`, the thinking-only prefill (2), the empty retry and its backoff, the `(empty)` terminal |
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
| `crates/engine/src/prompt.rs` | `agent/prompt_builder.py`, `tools/memory_tool.py` | `TASK_COMPLETION_GUIDANCE`, `PARALLEL_TOOL_CALL_GUIDANCE`, `MEMORY_GUIDANCE`, the local host block of `build_environment_hints`; the `MemoryStore` defaults `memory_char_limit` 2200 and `user_char_limit` 1375 |
| `crates/engine/src/prompt.rs` | `agent/conversation_loop.py` | `_restore_or_build_system_prompt`: the missing, null, empty and present states and the reuse with history; the working-directory check of `_stored_prompt_matches_runtime` with `host_info_value` and its scan of each `User home directory:` anchor |
| `crates/engine/src/prompt/skills.rs` | `agent/prompt_builder.py`, `agent/skill_utils.py` | `build_skills_system_prompt`, `_build_skills_system_prompt_inner` (the `## Skills (mandatory)` text, the category and skill order, the dedup by name), `_parse_skill_file`, `_build_snapshot_entry` (skill name and category from the path); `extract_skill_description` with `SKILL_PROMPT_DESC_LIMIT`, the `DESCRIPTION.md` pass |
| `crates/engine/src/skills.rs` | `agent/skill_utils.py`, `tools/skills_tool.py` | `EXCLUDED_SKILL_DIRS`, `SKILL_SUPPORT_DIRS`, `iter_skill_index_files`, `is_skill_support_path`, `is_excluded_skill_path`, `PLATFORM_MAP`, `skill_matches_platform_list`, `skill_matches_platform`; the `utf-8-sig` and `errors="replace"` reads |
| `crates/engine/src/skills/frontmatter.rs` | `agent/skill_utils.py` | `parse_frontmatter`: the byte order mark strip, the `---` fences, the fallback for YAML it cannot parse; the YAML subset of ADR-0016 section 10 |
| `crates/engine/src/tools/skills.rs` | `tools/skills_tool.py`, `tools/path_security.py` | `SKILLS_LIST_SCHEMA`, `SKILL_VIEW_SCHEMA`, `MAX_NAME_LENGTH`, `MAX_DESCRIPTION_LENGTH`, `_skill_lookup_path_error`, `_find_all_skills`, `_get_category_from_path`, `_parse_tags`, `_sort_skills`, `skills_list`, `skill_view` (the lookup strategies, the ambiguity, not-found, platform and disabled answers, the linked files, the `..` refusal and the missing-file listing), `_skill_view_with_bump`, `_SKILL_VIEW_DEDUP_CAP`, `_SKILL_VIEW_DEDUP_MESSAGE`, `_record_skill_view`, `_check_skill_view_dedup`, `_skill_view_fingerprint`; `validate_within_dir`, `has_traversal_component`; `agent/skill_utils.py` `is_valid_namespace`, `parse_qualified_name` |
| `crates/engine/src/config.rs` | `agent/skill_utils.py` | `get_disabled_skill_names` with `_normalize_string_set`: the disabled names stripped, blank ones dropped |
| `crates/engine/src/prompt/context.rs` | `agent/prompt_builder.py` | `load_soul_md`, `build_context_files_prompt`, `_find_git_root`, `_agents_md_directory_chain`, `_load_agents_md`, `_load_claude_md`, `_load_cursorrules`, `_truncate_content` with `CONTEXT_FILE_MAX_CHARS`, `CONTEXT_TRUNCATE_HEAD_RATIO`, `CONTEXT_TRUNCATE_TAIL_RATIO` and its marker, the byte order mark strip of `_scan_context_content` |
| `crates/engine/src/prompt/memory.rs` | `tools/memory_tool.py` | `MEMORY_BLOCK_HEADERS`, `ENTRY_DELIMITER`, `_render_block`, `_parse_entries`, the `utf-8-sig` text-mode read of `_read_raw_checked` with its universal newlines, the entry dedup of `load_from_disk` |
| `crates/engine/src/prompt/garden.rs` | `agent/memory_context.py`, `agent/turn_context.py`, `agent/memory_provider.py` | `build_memory_context_block` (the fenced block and its system note around the pointer lines; empty context, no block), the `\n\n` join of `compose_user_api_content`, the static `system_prompt_block` as the core profile block and the per-turn `prefetch` as C7 `pointers` |
| `crates/engine/src/tools/memory.rs` | `tools/memory_tool.py`, `utils.py` | `MEMORY_SCHEMA`, `memory_tool` with the `new_text` alias, `_missing_old_text_error`, `_memory_target_error`, `_read_failed_error`, `_drift_error`; `MemoryStore.add`, `replace`, `remove`, `apply_batch`, `_batch_error`, `_success_response`, `_previews`, `_consolidation_failure`, `_MAX_CONSOLIDATION_FAILURES_PER_TURN`, `_char_count`, `_char_limit`, `_entries_for`, `_set_entries`, `_reload_target` with `skip_drift` for `add`, `_detect_external_drift`, `_file_lock`, `_write_file`; `atomic_write_text` with the link kept by `atomic_replace` |
| `crates/engine/src/config.rs` | `tools/memory_tool.py`, `hermes_time.py` | the `memory.memory_char_limit` and `memory.user_char_limit` settings, the `timezone` setting of `_resolve_timezone_name` |
| `crates/engine/src/tools/web.rs` | `tools/web_tools.py` | `WEB_SEARCH_SCHEMA`, `WEB_EXTRACT_SCHEMA` and their registered handlers (`urls[:5]`); the `limit` clamp of `web_search_tool`; `DEFAULT_EXTRACT_CHAR_LIMIT`, `_get_extract_char_limit` and the `char_limit` clamp of `web_extract_tool`; the `Interrupted` answer and the `Error searching web: ` and `Error extracting content: ` prefixes |
| `crates/engine/src/tools.rs` | `agent/tool_dispatch_helpers.py` | `_UNTRUSTED_TOOL_NAMES`, `_UNTRUSTED_TOOL_PREFIXES` for `mcp_`, `_UNTRUSTED_WRAP_MIN_CHARS`, `_DELIMITER_TOKEN_RE`, `_neutralize_delimiters`, `_maybe_wrap_untrusted` |
| `crates/engine/src/tools.rs` | `tools/ansi_strip.py` | `strip_unicode_tags` with `_UNICODE_TAG_SUB_RE` |
| `crates/engine/src/web.rs` | `tools/web_tools.py` | the result of `web_search_tool`; `web_extract_tool`: the secret and credential pre-checks with their refusals, the SSRF filter, the reordering around invalid and refused items, the trimmed result, `Content was inaccessible or not found`, the final base64 sweep; `_web_extract_url` |
| `crates/engine/src/web.rs` | `plugins/web/exa/provider.py`, `agent/web_search_provider.py`, `hermes_cli/config.py` | `ExaWebSearchProvider.search` and `.extract` with their result and failure shapes, the missing-key error of `_get_exa_client`; `get_provider_env`; the request fields and the `Request failed with status code` text of the Exa SDK `exa_py` 2.10.2 (MIT) the provider calls |
| `crates/contracts/src/env_file.rs` | `hermes_cli/config.py` | `load_env`, `_parse_env_value` |
| `crates/engine/src/mcp.rs` | `tools/mcp_tool.py` | `_DEFAULT_TOOL_TIMEOUT`, `_DEFAULT_CONNECT_TIMEOUT`, the startup order of `register_mcp_servers` and `_discover_and_register_server` with the failed-connect warning, `_register_server_tools` with `_should_register`, `matches_name_filter` and its collision handling, `_select_utility_schemas`, `_UTILITY_CAPABILITY_ATTRS`, `_convert_mcp_schema`, `sanitize_mcp_name_component`, `MCP_TOOL_NAME_PREFIX`, `mcp_prefixed_tool_name`, `_MCP_INJECTION_PATTERNS`, `_scan_mcp_description` (ADR-0026) |
| `crates/engine/src/mcp/client.rs` | `tools/mcp_tool.py`; the MCP SDK `mcp` 2.0.0 (MIT) it pins: `mcp/client/session.py`, `mcp/client/streamable_http.py`, `mcp_types/version.py` | `_MCP_CONTENT_TYPES`, `_preflight_content_type`, the handshake path of `_negotiate_session`, `_advertises_tools`, `_discover_tools`, `_paginate_full_list`, `_MCP_LIST_MAX_PAGES`, the protocol header seeded in `_run_http`; `ClientSession.initialize`, `DEFAULT_CLIENT_INFO`, `HANDSHAKE_PROTOCOL_VERSIONS`, `LATEST_HANDSHAKE_VERSION`; `StreamableHTTPTransport._prepare_headers`, `_handle_post_request`, `_handle_json_response`, `_handle_sse_response`, `_handle_sse_event`, `_resolve_abandoned_request`, `terminate_session`; `_SESSION_EXPIRED_MARKERS` and `_is_session_expired_error` (ADR-0026) |
| `crates/engine/src/mcp/recovery.rs` | `tools/mcp_tool.py` | `_MAX_RECONNECT_RETRIES`, `_MAX_INITIAL_CONNECT_RETRIES`, `_MAX_BACKOFF_SECONDS`, `_BACKOFF_JITTER`, `_jittered`, `_PARKED_RETRY_INTERVAL`, the retry ladder and parked state of `MCPServerTask.run`, `_mark_session_proven`, `_signal_reconnect_and_wait`, `_wait_for_server_session_ready`, `_handle_session_expired_and_retry`, `_CIRCUIT_BREAKER_THRESHOLD`, `_CIRCUIT_BREAKER_COOLDOWN_SEC`, `_bump_server_error`, `_reset_server_error`, the breaker and no-session answers of `_make_tool_handler`, the timeout text of `_run_on_mcp_loop`, `_interrupted_call_result`, the `_sanitize_error` of `_format_connect_error` (ADR-0026) |
| `crates/engine/src/mcp/schema.rs` | `tools/mcp_tool.py`, `tools/schema_sanitizer.py` | `_normalize_mcp_input_schema`; `strip_nullable_unions`, `_const_branch_type`, `collapse_const_unions` |
| `crates/engine/src/mcp/tools.rs` | `tools/mcp_tool.py`, `agent/tool_dispatch_helpers.py` | `_build_utility_schemas`, `_make_tool_handler`, `_make_list_resources_handler`, `_make_read_resource_handler`, `_make_list_prompts_handler`, `_make_get_prompt_handler`, `_MCP_HARD_RESULT_CAP_CHARS`, `_truncate_mcp_text_result`, `_is_reserved_mcp_meta_key`, `_strip_reserved_meta_keys`, the size notes of `_cache_mcp_audio_block`, `_render_mcp_resource_block` without a document cache, `_MCP_RESOURCE_MAX_BYTES`; `_UPSTREAM_ELISION_PATTERNS`, `_ELISION_SCAN_MIN_CHARS`, `_ELISION_SCAN_MAX_CHARS`, `_UPSTREAM_ELISION_NOTICE`, `_detect_upstream_elision`, `_maybe_append_elision_notice` (ADR-0026) |
| `crates/engine/src/tools/redact.rs` | `tools/mcp_tool.py` | `_CREDENTIAL_PATTERN`, `_sanitize_error` |
| `crates/engine/src/tools/glob.rs` | `tools/mcp_tool.py` | the `fnmatch.fnmatchcase` of `matches_name_filter`; the algorithm is CPython's, see [`psf`](#psf) |
| `crates/engine/src/web/url.rs` | `tools/url_safety.py` | `normalize_url_for_request`, `_SENSITIVE_QUERY_PARAM_NAMES`, `sensitive_query_param_name`, `_BLOCKED_HOSTNAMES`, `_CGNAT_NETWORK`, `_is_blocked_ip`, `is_safe_url`; CPython's `urlsplit`, `urlunsplit`, `quote`, `unquote`, `parse_qsl`, the `idna` codec and the `ipaddress` ranges, see [`psf`](#psf) |
| `crates/engine/src/web/page.rs` | `tools/web_tools.py` | `convert_base64_images_to_links`, `_truncate_with_footer`, `_store_full_text`, `MAX_STORED_TEXT_CHARS` |
| `crates/engine/src/config.rs` | `tools/web_tools.py` | the `web.extract_char_limit` setting |
| `crates/engine/src/journal.rs` | `hermes_state_common.py`, `hermes_state.py`, `agent/conversation_compression.py` | the `active` and `compacted` message columns with their defaults and the live-message index; `archive_and_compact` without its watermark, lock holder, model-config patch and counters: the live rows archived as `active=0, compacted=1` and the compacted set inserted as live rows in one transaction; the `active = 1` filter of the live-context load and `include_inactive` of `get_messages`; the in-place branch of `_compress_context` keeping the session id (ADR-0014 section 6) |
| `crates/engine/src/journal/search.rs` | `hermes_state_common.py`, `hermes_state_search.py` | the columns of `FTS_SQL` and `MAX_FTS5_QUERY_CHARS`; `_sanitize_fts5_query` with `_FTS5_SPECIAL_CHARS`; the FTS5 path of `_search_messages_impl`: the `role_filter` and `(active = 1 OR compacted = 1)` filters, `sort` newest and oldest over rank, `snippet(messages_fts, -1, '>>>', '<<<', '...', 40)`, a query FTS5 rejects answered with no hits (differences in ADR-0051 section 4) |
| `crates/memory/src/archive/query.rs` | `hermes_state_common.py`, `hermes_state_search.py` | `MAX_FTS5_QUERY_CHARS`, `_sanitize_fts5_query` with `_FTS5_SPECIAL_CHARS`, the same port as `crates/engine/src/journal/search.rs`; the archive answers a query FTS5 still rejects with no hits |
| `crates/engine/src/journal/history.rs` | `hermes_state.py`, `hermes_state_search.py`, `hermes_state_common.py` | `get_messages_around` with its anchor check and `messages_before`/`messages_after`; the bookend queries of `get_anchored_view` (first and last user and assistant messages with text outside the window); `get_messages` of the read shape; the `list_sessions_rich` fields and recency order and the first-user-message source of `_PREVIEW_RAW_SELECT` (differences in ADR-0051 section 4) |
| `crates/engine/src/tools/session_search.rs` | `tools/session_search_tool.py` | `SESSION_SEARCH_SCHEMA` with its description and parameter texts; the shape choice, clamps and normalisation of `_session_search_impl`; `_discover` with `_DISCOVER_SCAN_LIMIT`, the current-session skip of live hits, the adaptive and full detail and its 4,000 and 1,200 character bounds; `_scroll` with its current-session guard; `_read_session` with its 20 + 10 head and tail; `_list_recent_sessions`; `_shape_message`, `_is_compaction_summary` with `_COMPACTION_PREFIXES`; every result and error text; `_shape_preview` of `hermes_state_common.py` (differences in ADR-0051 section 4) |
| `crates/engine/src/tools/react.rs` | `tools/react_to_message_tool.py`, `tools/send_message_tool.py` | `REACT_TO_MESSAGE_SCHEMA` name, its `emoji` parameter and the description's guidance (when to react, never narrate, one reaction per message replaced by a new one, default target the message the turn answers); the Telegram `message_id` target of `send_message` `action="react"` (differences in ADR-0088) |
| `crates/engine/src/serve.rs` | `gateway/run.py`, `gateway/session_state.py` | busy input in `busy_input_mode: queue`: `_queue_or_replace_pending_event` with `_BUSY_QUEUE_MAX_PENDING` (32), `_enqueue_fifo`, `_queue_depth`, the drain of `_dequeue_pending_event` and `_promote_queued_event` after each run; `TurnState.agent` and `started_ts` as the running turn and its start; one running turn per session key, turns of different keys at once (ADR-0028) |
| `crates/engine/src/serve.rs` | `gateway/run.py`, `gateway/slash_commands.py`, `run_agent.py` | `/stop`: `_handle_stop_command` answering an idle session with no change, `_interrupt_and_clear_session` with `_INTERRUPT_REASON_STOP` (the hard interrupt, the pending queue head consumed and discarded, the rest of the queue kept), `_invalidate_session_run_generation` and `_is_session_run_current` as one interrupt handle per turn, `interrupt` with `hard_cancel` (ADR-0028 section 3) |
| `crates/engine/src/serve.rs` | `gateway/slash_commands.py`, `gateway/run.py`, `gateway/session_state.py` | `/model` with its default scope `--session`: `_handle_model_command` storing the session's override, `ConversationState.model_override`, `_apply_session_model_override` applying it when a run starts, `_rehydrate_session_model_override` reading the persisted override after a restart, the override a new session drops; the model is a qb route alias and the override is C8 `Session.route` (ADR-0028 section 5) |
| `crates/engine/src/serve.rs` | `gateway/run.py`, `gateway/platforms/base.py` | a turn's reply as C6 `turn_events` (ADR-0031): `interim` as `_interim_assistant_cb` sends it without a stream consumer; `final` from the final-response path of `_run_agent_inner` and `_handle_message` with the empty response that `_process_message_background` does not send; no `interim` (`_run_still_current`) and no `final` once `_is_session_run_current` is false after `_invalidate_session_run_generation`; the queue head `_interrupt_and_clear_session` discards answered as `dropped` |
| `crates/engine/src/serve.rs` | `gateway/run.py` | a crashed agent run: the `except` of `_handle_message_with_agent` logging the error and answering the turn as failed, the `finally` of `_run_agent_inner` freeing the session with `_release_running_agent_state`, as a runner panic that ends the turn `failed` and lets the session run its next queued turn (ADR-0042 section 6) |
| `crates/engine/src/serve.rs` | `gateway/owner_inbox.py`, `gateway/owner_inbox_store.py` | the free owner session of `inject` and `is_idle` (ADR-0036 section 5): `Bridge.blocked` as no running or queued turn and a channel holding `inbox_turns` in place of the adapter `Bridge.tick` looks up, `Bridge.tick`'s refusal to create a conversation when the owner session entry is missing as the journal session check; the 15 s default `delay` of `retry` in `owner_inbox_store.py` as `RETRY_AFTER_S`; the re-offer of a `deferred` row in `owner_inbox_store.py` as the new turn a higher `attempt` starts only after a recorded `silent` delivery (ADR-0037 section 3) |
| `crates/engine/src/main.rs` | `cli.py` | `_signal_handler_q`: SIGINT, SIGTERM and SIGHUP of the one-shot mode as `request_hard_interrupt` of the running turn (ADR-0029) |
| `crates/channels/src/bot_api/config.rs` | `plugins/platforms/telegram/adapter.py` | `base_url`/`base_file_url` handling and `_max_doc_bytes` of `TelegramAdapter.__init__`/`connect`, HTTP timeouts of `connect`, `disable_link_previews` |
| `crates/channels/src/bot_api/error.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py`, `agent/redact.py` | `_redact_telegram_error_text`, `_looks_like_polling_conflict`, `_looks_like_auth_error`, `_looks_like_network_error`, `_looks_like_connect_timeout`, `_is_bad_request_error`; `_is_retryable_error`, `_is_timeout_error`, `_RETRYABLE_ERROR_PATTERNS`; `_TELEGRAM_RE` of `redact_sensitive_text` |
| `crates/channels/src/bot_api/client.rs` | `plugins/platforms/telegram/adapter.py` | Bot API calls of `connect`, `_delete_webhook_best_effort`, `send`, `send_typing`, `_set_reaction`, `_clear_reactions`, `send_document`, `send_voice` (`sendVoice`, `sendAudio`), `send_video`, `send_multiple_images` (`sendMediaGroup` with local files), media download paths, `_link_preview_kwargs`, `_notification_kwargs`, caption cut |
| `crates/channels/src/bot_api/types.rs` | `plugins/platforms/telegram/adapter.py` | request fields of the calls above |
| `crates/channels/src/bot_api/poller.rs` | `plugins/platforms/telegram/adapter.py` | `_start_polling_resilient`, `_start_polling_once`, `_handle_polling_network_error`, `_handle_polling_conflict`, `_record_polling_progress` counters; `allowed_updates` |
| `crates/channels/src/bot_api/sender.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | send attempt loop of `TelegramAdapter.send`; `BasePlatformAdapter._send_with_retry` and its delivery-failure notice |
| `crates/channels/src/bot_api/files.rs` | `plugins/platforms/telegram/adapter.py` | the `get_file` and `download_as_bytearray` pair of `_handle_media_message` as the ingress's file source, bounded by the media cap |
| `crates/channels/src/tg/channel.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/run.py` | `connect` in polling mode (`getMe`, `_start_polling_resilient` with `allowed_updates`), `disconnect` with the final offset confirmation of the library's updater; the gateway's adapter start and stop; differences in ADR-0032 |
| `crates/channels/src/tg/outbound.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | `TelegramAdapter.send` (blank text, per-chunk `MarkdownV2` with the plain-text retry, typing re-trigger after interim sends), `send_typing` with its per-chat cooldown; `_keep_typing` with its 1.5 s cancel, running beside the sends; `_send_with_retry` with its formatting fallback; the `MEDIA:` files of a final reply sent after its text (`_process_message_background`), interim texts through the display cleanup; differences in ADR-0032, ADR-0034 and ADR-0052 |
| `crates/channels/src/tg/media.rs` | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | `validate_media_delivery_path` in non-strict mode with `_MEDIA_DELIVERY_DENIED_PREFIXES`, `_MEDIA_DELIVERY_DENIED_HOME_SUBPATHS` and `_path_under_denied_prefix`, `filter_media_delivery_paths`, `_log_safe_path`; the image, voice, video and document routing of `_process_message_background` with `should_send_media_as_audio`, `_TELEGRAM_VOICE_EXTS`, `_TELEGRAM_AUDIO_ATTACHMENT_EXTS`; `_notify_media_delivery_failure`; the photo-to-document fallback of `send_image_file`, the album chunks and per-photo fallback of `send_multiple_images`, the voice and audio choice of `send_voice`; differences in ADR-0052 |
| `crates/channels/src/render/media.rs` | `gateway/platforms/base.py`, `gateway/stream_consumer.py` | `extract_media` and `strip_media_directives_for_display` with `_strip_media_tag_directives` (`_clean_for_display`), with `MEDIA_DELIVERY_EXTS`, `MEDIA_TAG_CLEANUP_RE`, `MEDIA_EXTENSIONLESS_TAG_RE`, `_match_extensionless_path`, `_normalize_media_tag_path`, `_path_lacks_deliverable_extension`, `_merge_spans`, `_mask_protected_spans`, `_mask_json_string_media`, `_AUDIO_EXTS`; `os.path.expanduser`; differences in ADR-0052 |
| `crates/channels/src/c6/session.rs` | `gateway/session.py`, `gateway/slash_commands.py`, `gateway/run.py` | `build_session_key` for a direct chat in the default namespace (`_session_key_namespace`), the platform name as the turn's channel; the C6 calls behind `_handle_reset_command` (generation invalidation as `interrupt`, the reset or new header), `_handle_stop_command`, `_handle_status_command`, `_handle_model_command`; busy input left to the engine's queue (`busy_input_mode: queue`); differences in ADR-0033 |
| `crates/channels/src/tg/link.rs` | `gateway/run.py`, `gateway/config.py` | the failure answer of `_handle_message_with_agent` without its status hint; the transcript echo of `_prepare_inbound_message_text` behind `_should_echo_stt_transcripts` and the `stt_echo_transcripts` switch; differences in ADR-0033 and ADR-0035 |
| `crates/channels/src/tg/follow.rs` | `gateway/run.py`, `gateway/platforms/base.py`, `gateway/display_config.py` | `_interim_assistant_cb` without a stream consumer and `interim_assistant_messages` on for Telegram; the final-response path of `_handle_message_with_agent` with the intentional-silence check before the `(empty)` notice and the failed branch of `_normalize_empty_agent_response`; `_process_message_background` sending nothing for an empty answer and the typing stop before the final send (`_pause_typing_before_finalize`, `_stop_typing_refresh`); `_reply_anchor_for_event` in a private chat (an answer replies to the turn's message, a turn without one sends plain); differences in ADR-0034 |
| `crates/channels/src/tg/inbox.rs` | `gateway/owner_inbox.py`, `gateway/platforms/base.py` | the owner inbox patch: `quiet_config` (no typing, interim message, reaction, failure notice or quota line), `Receipt.agent_result` and `no_model_text` with `_NO_TEXT_PREFIXES` (consumption), the silence set of `Receipt.invoke` with `NO_REPLY_DONE` as `completed_silence`; the `base.py` hunks `@@ -5452`, `@@ -6312`, `@@ -6576`, `@@ -6609` (no reactions or typing for the event, one plain `send` instead of `_send_with_retry`, `delivery_acked` when every part has a message id); differences in ADR-0036 |
| `crates/core/src/prompt.rs` | `gateway/owner_inbox.py` | the model text of `Bridge.tick`: the `policy` expression by `kind`, the `text` expression with its header line and fixed sentences, and the `json.dumps(..., ensure_ascii=False)` tail of the claimed row (ADR-0036 section 4) |
| `crates/core/src/deliver.rs` | `gateway/owner_inbox.py` | the consumer of `start` (the `ENABLED` gate, the non-blocking exclusive `flock` of `consumer.lock`, the fixed log line of a failed tick), the `ENABLED` rule of `pump_once` (new claims only), `Bridge.tick` (renew while a turn is in flight; recover, busy check, claim, inject; `retry` when the turn did not start) and the ack mapping of `Receipt.complete`, with the deviations of ADR-0036 section 11 |
| `crates/channels/src/ingress/update.rs` | `plugins/platforms/telegram/adapter.py` (`_effective_update_message`, handler registration) | fields read by the ingress |
| `crates/channels/src/ingress/text.rs` | `plugins/platforms/telegram/adapter.py` (`_telegram_message_plain_text`, `_telegram_custom_emoji_text`, `_telegram_rich_message_text`, `_rich_blocks_to_plain`, `_extract_rich_reply_text`, `_clean_bot_trigger_text`); `gateway/platforms/base.py` (`get_command`) | local patches 0011, 0012; ADR-0018 |
| `crates/channels/src/ingress/forward.rs` | `plugins/platforms/telegram/adapter.py` (`_forward_label`, `_forward_person_label`, `_forward_chat_label`, `_render_forward_batch`) | local patches 0001, 0016 |
| `crates/channels/src/ingress/media.rs` | `plugins/platforms/telegram/adapter.py` (`_telegram_media_size_allowed`, `_telegram_media_too_large_note`, `_surface_media_cache_failure`); `gateway/platforms/base.py` (`cache_*_from_bytes`, `cache_media_bytes`, type tables); `tools/audio_container.py` | |
| `crates/channels/src/ingress/event.rs` | `plugins/platforms/telegram/adapter.py` (`_enqueue_text_event`, `_can_coalesce`, `_merge_coalesced_parts`, `_enqueue_photo_event`); `gateway/platforms/base.py` (`_merge_caption`, `is_command`) | local patch 0010 |
| `crates/channels/src/ingress/coalesce.rs` | `plugins/platforms/telegram/adapter.py` (`_flush_text_batch`, `_dispatch_inbound`, `_flush_turn_coalesce`, `_flush_turn_coalesce_now`, `_flush_photo_batch`, `_queue_media_group_event`, `_flush_media_group_event`) | local patch 0010; deadlines instead of tasks |
| `crates/channels/src/ingress/prepare.rs` | `gateway/run.py` (`_prepare_inbound_message_text`, `_enrich_message_with_vision`, `_enrich_message_with_transcription`, `_build_document_context_note`, `_format_duration`) | local patch 0001 (forward label above transcripts); image note instead of the vision description (ADR-0033 section 8) |
| `crates/channels/src/ingress/reaction.rs` | `plugins/platforms/telegram/adapter.py` (`_normalize_reaction_event`); profile plugin `reactions-log` | ADR-0018 |
| `crates/channels/src/ingress/command.rs` | `gateway/slash_commands.py` (`_handle_reset_command`, `_handle_stop_command`, `_handle_status_command`, `_handle_model_command`); `gateway/platforms/base.py` (`get_command`); `locales/ru.yaml` | ADR-0018 |
| `crates/channels/src/ingress/pipeline.rs` | `plugins/platforms/telegram/adapter.py` (`_is_user_authorized_from_message`, `_build_message_event`, `_handle_text_message`, `_handle_command`, `_handle_location_message`, `_handle_media_message`, `_handle_sticker`, `_cache_replied_media`, `_observed_media_source`, `_handle_unhandled_message`); `gateway/sticker_cache.py`; `gateway/run.py` (the internal `MessageEvent` of a wake-up, without a message id) | local patch 0011 |
| `crates/channels/src/render/markdown.rs` | `plugins/platforms/telegram/adapter.py` | `TelegramAdapter.format_message`, `_escape_mdv2`, `_TG_EMOJI_RE` (outbound custom emoji) |
| `crates/channels/src/render/plain.rs` | `plugins/platforms/telegram/adapter.py` | `_strip_mdv2` |
| `crates/channels/src/render/tables.rs` | `gateway/platforms/helpers.py`, `agent/markdown_tables.py` | `convert_table_to_bullets`, `_render_table_block`, `TABLE_SEPARATOR_RE`, `is_table_row`, `split_table_row` |
| `crates/channels/src/render/split.rs` | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | `BasePlatformAdapter.truncate_message`, `utf16_len`, `_custom_unit_to_cp`; chunk post-processing of `TelegramAdapter.send`, `_separate_chunk_indicator_from_fence` |
| `crates/channels/src/render/send.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py` | send decisions of `TelegramAdapter.send`, `_should_thread_reply`, `_link_preview_kwargs`, `_notification_kwargs` |
| `crates/channels/src/render/typing.rs` | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | `_keep_typing`, `_stop_typing_refresh`, `pause_typing_for_chat`; `send_typing` cooldown, `_is_transient_typing_error`, `_record_typing_cooldown` |
| `crates/channels/src/render/silence.rs` | `gateway/response_filters.py` | `is_intentional_silence_response`, `_canonical_silence_candidate`, `_strip_edge_silence_punctuation`, `_canonical_silence_candidates`, `LIVE_GATEWAY_SILENT_MARKERS` with the owner inbox patch's `NO_REPLY_DONE` |
| `crates/channels/src/stt.rs` | `tools/transcription_tools.py` | `_transcribe_groq` (request fields, `timeout=30`, `max_retries=0`, `OPENAI_MODELS` replacement, error texts), the checks of `transcribe_audio` and `_transcribe_prepared_audio`: `_validate_audio_source_file`, `_validate_audio_file`, `_validate_audio_file_size`, `SUPPORTED_FORMATS`, `MAX_FILE_SIZE`, the CAF conversion error; `DEFAULT_GROQ_STT_MODEL`, `GROQ_BASE_URL`, the `GROQ_API_KEY` name; the status error text of the client library; differences in ADR-0035 |
| `crates/channels/src/render/pytext.rs` | `plugins/platforms/telegram/adapter.py`, `gateway/platforms/base.py`, `gateway/response_filters.py` | semantics of the `str`, `re` and `unicodedata` primitives (`isspace`, `strip`, `\s`, `\w`, `re.sub` scanning, punctuation categories) the functions above rely on; `os.path.splitext` and `PurePath.suffix` of a file name |
| `crates/core/src/status.rs` | `gateway/slash_commands.py`, `gateway/status.py` | `_handle_status_command` in part: the session key, running or idle, the queue depth and the connected channel, with the route alias for model and provider; the health probe of `resolve_gateway_liveness` as C1 `health`, C6 `is_idle` and C4G `GET /v1/health` (ADR-0040 section 7) |
| `crates/garden/src/search.rs` | `agent/memory_provider.py`, `plugins/memory/query_rewrite.py` | `TRIVIAL_PROMPT_RE` and `is_trivial_prompt` (Russian acknowledgements added, see ADR-0045); the head-and-tail bound of `_bounded_user_message` without its omission marker |

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

- Upstream: https://github.com/python/cpython, modules `Lib/difflib.py` and `Lib/shlex.py`.
- Licence: Python Software Foundation License Version 2.
- `crates/engine/src/tools/difflib.rs` ports `SequenceMatcher` (`find_longest_match`,
  `get_matching_blocks`, `get_opcodes`, `ratio`, the popular-element junking of `__chain_b`),
  `get_grouped_opcodes`, `unified_diff` and `_format_range_unified`.
- Summary of the changes: rewritten in Rust over slices of characters or lines; no `isjunk`
  callback and no `autojunk` switch (always on, as the reference calls it); `unified_diff` takes
  no dates and joins its lines into one string.
- `crates/engine/src/tools/shlex.rs` ports `shlex.shlex.read_token` in POSIX mode with
  `whitespace_split` and `punctuation_chars`, and `shlex.split`.
- Summary of the changes: rewritten in Rust over one string; the punctuation characters and
  whether `#` starts a comment are parameters; no `wordchars`, line numbers, `source` inclusion
  or non-POSIX mode; a lexing error is `None`.

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

<a id="snowball"></a>

## Snowball (notice id `snowball`)

- Upstream: https://github.com/snowballstem/snowball, `algorithms/russian.sbl` as of release
  v3.2.0 (the file is unchanged since its commit of 2018-03-17).
- Licence: BSD-3-Clause.
- `crates/garden/src/morph.rs` starts with `// Port: THIRD_PARTY_NOTICES.md#snowball` and ports
  the Russian stemmer: the regions RV and R2, the endings of `perfective_gerund`, `adjective`,
  `adjectival`, `reflexive`, `verb`, `noun`, `derivational` and `tidy_up`, and `stem` with its
  `ё` to `е` normalisation.
- Summary of the changes: rewritten in Rust over a vector of characters instead of compiled from
  the Snowball language; the stemmer takes one lowercase token.

### Licence text

```
Copyright (c) 2001, Dr Martin Porter
Copyright (c) 2004,2005, Richard Boulton
Copyright (c) 2013, Yoshiki Shibukawa
Copyright (c) 2006-2025, Olly Betts
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

  1. Redistributions of source code must retain the above copyright notice,
     this list of conditions and the following disclaimer.
  2. Redistributions in binary form must reproduce the above copyright notice,
     this list of conditions and the following disclaimer in the documentation
     and/or other materials provided with the distribution.
  3. Neither the name of the Snowball project nor the names of its contributors
     may be used to endorse or promote products derived from this software
     without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

<a id="hf-tokenizers"></a>

## Hugging Face `tokenizers` and `spm_precompiled` (notice id `hf-tokenizers`)

- Upstream: https://github.com/huggingface/tokenizers (`tokenizers/src/models/unigram/model.rs`,
  `tokenizers/src/normalizers/precompiled.rs`, `tokenizers/src/tokenizer/added_vocabulary.rs`,
  `tokenizers/src/pre_tokenizers/metaspace.rs`) and the crate `spm_precompiled` 0.1.4
  (`src/lib.rs`), as the `tokenizers` 0.23.3 Python package runs them.
- Licence: Apache-2.0.
- `crates/infer/src/text/mod.rs`, `charsmap.rs` and `unigram.rs` start with
  `// Port: THIRD_PARTY_NOTICES.md#hf-tokenizers` and port the behaviour of the XLM-RoBERTa
  pipeline (ADR-0087 E2): the split at added tokens with `lstrip` / `rstrip`, the precompiled
  character map's common-prefix lookup and its use per grapheme cluster, `WhitespaceSplit`,
  `Replace` of runs of spaces, `Metaspace` (prepend always, split), and the Unigram
  `encode_optimized` Viterbi with fused unknowns.
- Summary of the changes: rewritten in Rust over arrays read in place from the engine's weight
  file (pieces sorted by their bytes and searched by range narrowing instead of a trie; the
  character map's double array as the model ships it); processed in order and stopped once the
  truncation length is reached; only the parts these checkpoints use.

### Licence text

```
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
```

<a id="unicode-segmentation"></a>

## `unicode-segmentation` and the Unicode Character Database (notice id `unicode-segmentation`)

- Upstream: https://github.com/unicode-rs/unicode-segmentation, release 1.13.3
  (`src/grapheme.rs`, `src/tables.rs`, Unicode 17.0.0), and the Unicode Character Database it
  is generated from.
- Licence: MIT OR Apache-2.0 (used here under MIT) for the crate; Unicode License v3 for the
  data.
- `crates/infer/src/text/grapheme.rs` starts with `// Port: THIRD_PARTY_NOTICES.md#unicode-segmentation`
  and ports the extended grapheme cluster rules of `check_pair` with the look-backs of
  `GraphemeCursor`; `crates/infer/src/text/grapheme_table.rs` is generated from the crate's
  tables (`Grapheme_Cluster_Break`, `Extended_Pictographic`, `InCB`).
- Summary of the changes: the look-backs are carried forward as state over one left-to-right
  pass; the tables are merged into one array of runs.

### Licence text

```
Copyright (c) 2015 The Rust Project Developers

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

```
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```
