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
| `crates/engine/src/tools/terminal.rs` | `tools/terminal_tool.py` | `TERMINAL_SCHEMA`, `TERMINAL_TOOL_DESCRIPTION`, the command, timeout and workdir checks of `terminal_tool` |
| `crates/engine/src/tools/files.rs` | `tools/file_tools.py` | `READ_FILE_SCHEMA`, `WRITE_FILE_SCHEMA`, `PATCH_SCHEMA`, `_handle_write_file`, the replace-mode checks of `patch_tool`, the `write_file_tool` result |
| `crates/engine/src/tools/files.rs` | `tools/file_operations.py` | `write_file`: atomic replace keeping the mode, post-write verification, `WriteResult` |
| `crates/engine/src/tools/search.rs` | `tools/file_tools.py` | `SEARCH_FILES_SCHEMA`, `_handle_search_files`, `search_tool` and its truncation hint |
| `crates/engine/src/tools/search.rs` | `tools/file_operations.py` | `search`, `_search_files`, `_search_files_rg`, `_search_with_rg`, `normalize_search_pagination`, `SearchResult.to_dict` |
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
