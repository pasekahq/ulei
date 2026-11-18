# ADR-0025: Engine context compressor

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The reference compacts a long session in `agent/context_compressor.py`
`ContextCompressor.compress`, triggered in `agent/conversation_loop.py` before an API call whose
prompt reached `threshold_tokens` and after a context-overflow error. Its default path (`tail_mode` `legacy`) prunes old tool output,
keeps a head and a token-budget tail, summarizes the middle with a structured template through
`_generate_summary`, updates an earlier summary on the next compaction, and falls back to
`_build_static_fallback_summary` when the summary call fails. The engine reaches models only
through qb (ADR-0009) and stores sessions in C8 (ADR-0014), whose `compact` replaces the live
set in place.

## Decision

`crates/engine/src/compress.rs` and `compress/text.rs` port that default path and the trigger
state (`_compute_threshold_tokens`, `update_from_response`, `should_compress` with the cooldown
and anti-thrash guards). `Compressor::compact` compresses the stored live set of a session and
stores the result with `Journal::compact`; `Compressor::compress` compresses messages in memory.
A `Compressor` holds the state of the session it is bound to (`bind`); binding another session
drops that state.

`crates/engine/src/agent.rs` ports the triggers of a turn:

- Before every step, the prompt tokens of the last step's C8 usage (input plus cache read and
  write) are compared with the route's threshold; at or above it, and with no cooldown or
  anti-thrash guard holding, the turn's conversation is compacted and the step sends the
  compacted set. At the start of a turn the last stored step gives the usage. Without a reading
  a rough count of the request is used; right after a compaction none.
- A step answered `context_too_long` compacts the conversation and, when the compaction dropped a
  message or cut 5 % of the rough tokens, waits 2 s and sends the step again under the next
  request id. A second `context_too_long` ends the turn with
  `Context length exceeded: max compression attempts (1) reached.`; a compaction without
  progress ends it with `Context length exceeded (N tokens). Cannot compress further.`
  (`Ended::ContextExhausted`, one stderr line and exit 1 from `ulei-engine run`).
- Both triggers share one compaction per turn (`MAX_COMPRESSION_ATTEMPTS`). The first step usage
  after a completed compaction whose prompt tokens are above 0 and under the threshold of the
  window in use (the configured one, else the 256,000 overflow window) gives the turn its
  compaction back; a rough estimate never does. A retried step's own answer comes before any
  such usage, so one overflow still gets one retry. A compaction resets
  the empty-reply and dropped-call recovery of the turn. The turn context is kept out of the
  compressed messages and appended again to the input when the compacted set keeps it.

A compaction inside a turn is stored by that turn's `Journal::append` in one transaction: the
live set, the turn's input and its messages before the compaction are archived
(`active=0, compacted=1`), and the compacted set and the later messages follow as the live set.
Every row and step carries the turn's number, so request ids stay `s<session>-t<turn>-<step>`
and `seq` stays contiguous; a compaction inside the first turn needs no stored turn.

### 1. Message rows

The reference works on OpenAI-shaped rows where each tool result is its own `tool` message. A
C4G user message carrying `tool_result` blocks becomes one row per result, followed by a user
row with the message's other blocks, if any; the rows join back into C4G messages after the
compaction. Every count of the reference (`protect_first_n`, the tail floors, the turn-pair and
tool-group alignment) acts on these rows.

### 2. Settings

| Key | Default | Reference | Value source |
|---|---|---|---|
| `engine.compression.route` | `compact` | `call_llm(task="compression")` on the auxiliary model | port map R2 |
| `engine.compression.timeout` | 450 s | `auxiliary.compression.timeout` | profile value; the reference code resolves it from that key |
| `engine.compression.protect_last_n` | 20 | `protect_last_n=20` | code default, equal to the profile |
| `engine.compression.target_ratio` | 0.2, clamped to 0.10..0.80 | `summary_target_ratio=0.20` | code default, equal to the profile |
| none | 3 | `protect_first_n=3` | code default |
| `engine.compression.threshold` | 0.3 | `compression.threshold`; code default 0.50 | profile value |
| `engine.compression.threshold_tokens` | 300000; 0 sets no cap | `compression.threshold_tokens`; code default none | profile value |
| `engine.routes.<alias>.context_length` | none: no compaction before a step | `model.context_length`, else model metadata, else `DEFAULT_FALLBACK_CONTEXT` 256,000 | C4G routes report no window |
| `engine.routes.<alias>.max_tokens` | 16384, the engine's bound of one step | `model.max_tokens`, `None` reserving nothing | C4G requires `max_tokens`; the reserve is what the step requests |
| none | 64,000 floor, 0.85 of a window the floor fills, 0.75 below 512K | `MINIMUM_CONTEXT_LENGTH`, `_MIN_CTX_TRIGGER_RATIO`, `_SMALL_CTX_WINDOW_LIMIT`, `_SMALL_CTX_THRESHOLD_PERCENT` | code default |
| none | 300 s | `_ANTI_THRASH_RECOVERY_SECONDS` | code default |
| none | 1 | `max_compression_attempts=3` | DoD d1 of the sprint: one compaction and one retry |
| none | 2 s | `time.sleep(2)` before the overflow retry | code default |
| none | 256,000 | `DEFAULT_FALLBACK_CONTEXT` | code default; sizes an overflow compaction of a route with no window |

A route's settings live in the section `[engine.routes.<alias>]`; the environment variable is
`ULEI_ENGINE_ROUTES_<ALIAS>_<FIELD>` with every character other than a letter or digit as `_`.

C4G requires `max_tokens`; the summary turn carries the engine's bound of one model turn, never
the summary budget, which stays prompt guidance (`Target ~N tokens`) as in the reference.

### 3. Differences from the reference

| Upstream function | Reference | Engine | Why |
|---|---|---|---|
| `_generate_summary` | `call_llm(task="compression")` on the auxiliary model, with `summary_model`, a retry on the main model (`_fallback_to_main_for_compression`) and provider resolution | one tool-less C4G turn on the configured alias, no system prompt, no session key | Models only through qb (ADR-0009, port map R1, R2); qb owns fallbacks. |
| `_generate_summary` | `aux_interrupt_protection` shields the summary call from an interrupt | the interrupt cancels the turn at qb; the compaction then changes nothing, as on `AuxiliaryExplicitCancellation` | Port map R4: an emergency cancel stops whatever runs. |
| `_generate_summary`, `_is_summary_access_or_quota_error`, `_is_connection_error` | a failed call is classified by the provider exception's type, HTTP status and text | the C4G end of the turn is mapped: `auth` is the access failure (abort, 60 s); `quota_exhausted` is the access or quota failure and, as its 429 is in `_is_timeout`, a timeout (abort, timeout ladder); `unavailable`, no vendor of the route answered, takes the generic path (fallback naming the error, 60 s); the 600 s rung of `_SUMMARY_FAILURE_COOLDOWN_SECONDS` is for a missing provider configuration, which no C4G class reports, so it has no producer and is not ported; `overloaded` is the rate limit (fallback, timeout ladder); the turn deadline is a timeout and a connection failure (abort, timeout ladder); a failed connection or a stream that ends without a terminal event is a connection failure (abort, 30 s); a malformed answer is the JSON error (fallback, 30 s); anything else falls back with 60 s. The error text is still read for the reference's timeout and connection markers | C4G reports a failure class instead of a vendor exception; qb's class is authoritative and its HTTP status informative. |
| `_generate_summary` | the empty-content error names `provider` and `model` | names `provider=qb` and the route alias | The engine knows only the alias. |
| `_summarize_tool_result` | branches for every reference tool | branches for the engine's tools and the generic line | Only the engine's tools exist. |
| `_prune_old_tool_results`, `_summarize_tool_result`, `_generate_summary`, `_build_static_fallback_summary` | ghost-skill defense: just-loaded `skill_view` results spared, `[SKILL_PRUNED: ...]` markers written and re-injected | not ported; the template keeps its `## Pruned Skills` section | Skill-view pruning markers are outside the ported default path of this module. |
| `_prune_old_tool_results` pass 1 | duplicates found by the first 12 hex digits of an MD5 | duplicates found by equal content | Same result without a hash dependency. |
| `_estimate_msg_budget_tokens`, `estimate_messages_tokens_rough` | the content plus the Python `repr` of each tool call or of the message; reasoning fields charged | the content plus the JSON of each `tool_use` block, or the JSON of the C4G message; `thinking` blocks not charged | C4G messages have no OpenAI shape; `thinking` is an opaque vendor payload. `estimate_tokens_rough` and its CJK rule are ported as they are. |
| `_strip_historical_media`, the image branches of the prune, serialize and estimate helpers | image parts replaced or counted | not ported | No engine message carries an image. |
| `compress` | summary rows flagged with `_compressed_summary` and `_compressed_summary_has_user_turn` | no flags; summaries are recognized by their content and the provenance by the no-user sentinel | C8 stores C4G messages only, as the reference's session store drops the flags. |
| `_strip_summary_prefix` | also strips `_HISTORICAL_SUMMARY_PREFIXES` | strips the current and the legacy prefix | No journal holds a handoff of an older build. |
| `_is_synthetic_compression_user_turn` | knows the continuation texts, the step-limit request, the loop's nudges and the todo and background-process markers | knows the continuation texts, the step-limit request, the engine's two nudges and the background-process prefix | The other texts have no engine source. |
| `compress` phase 4 | appends a compaction note to a system message at index 0 | not reached | The system prompt is stored apart from the live set (ADR-0014), as on the reference's automatic path. |
| `compress` | the no-op branches return the pruned list and count an ineffective compaction | nothing is stored and the caller gets `Skip`; no counter | A skip changes no message; the anti-thrash guard counts the compactions whose next real prompt stayed over the threshold, as `update_from_response` does. |
| `compress` | feasibility skip after an ineffective compaction (`_FEASIBILITY_SKIP_MIDDLE_FRACTION`) | not ported | It reads the counter above. |
| `compress` | `_prune_stale_reasoning_replay` | not ported | No C4G block carries replayed reasoning items. |
| `_generate_summary` | the memory provider section | not ported | The engine has no memory provider; the reference default passes none. |
| `_record_compression_failure_cooldown`, `get_active_compression_failure_cooldown`, `_persist_ineffective_compression_count`, `_persist_fallback_compression_streak` | the cooldown and the anti-thrash counters persisted in the session store | kept in the `Compressor` of the session; `ulei-engine run` runs one turn per process, so they last one turn | C8 has no column for them. |
| `conversation_loop.py` context overflow | up to `max_compression_attempts` (3) compactions per turn, shared with the pre-API and post-tool gates and re-armed by `_should_rearm_compression_budget` when the first real prompt count after a completed compaction is under the threshold | one (`MAX_COMPRESSION_ATTEMPTS`), re-armed the same way, and the text names it: `Context length exceeded: max compression attempts (1) reached.` | DoD d1 of the sprint: `context_too_long` compacts once and retries once, a second one ends the turn with one line. |
| `conversation_loop.py` pre-API gate, post-tool gate, `turn_context.py` prologue preflight | the pre-API and prologue gates compare a rough request estimate, deferred to real usage by `should_defer_preflight_to_real_usage`; the post-tool gate compares `last_prompt_tokens` | one gate before every step on the last step's usage, a rough count of the request only without a reading | DoD d1 of the sprint: the threshold is read from the last step's C8 usage. |
| `get_model_context_length` | an unknown window resolves to `DEFAULT_FALLBACK_CONTEXT` (256,000) and compacts at its threshold | a route without `context_length` runs no compaction before a step; an overflow compaction sizes its tail and summary by 256,000 | C4G routes report no window; a guessed one would compact a small route too late or a large one too early. |
| `conversation_loop.py` context overflow | `get_context_length_from_provider_error` lowers the window, `parse_available_output_tokens_from_error` and `is_output_cap_error` lower `max_tokens` or fail fast | not ported | `context_too_long` is a C4G class without the provider's text or limits. |
| `conversation_loop.py`, `conversation_compression.py` | the compression lock and its refunded attempts, `_should_skip_model_call_for_reference_handoff`, the status messages | not ported | One engine process runs one turn of a session; the engine has no status channel. |
| `_compress_context` | refuses a compressed transcript whose rough tokens grew, after `salvage_grown_transcript`, and counts an ineffective compaction | not ported | The growth it catches comes from the todo refresh and user-turn anchoring that run after `compress`; the engine adds nothing after it. |

## Consequences

- `ulei-engine run` compacts a long session on its own before a step and recovers one
  `context_too_long` per turn; a route needs `engine.routes.<alias>.context_length` for the
  first.
- The step's `max_tokens` is the route's `engine.routes.<alias>.max_tokens`, the reserve the
  threshold subtracts.
- A second compaction of a session that restarted in between finds the earlier summary in the
  live set and updates it, as one that never stopped does.
