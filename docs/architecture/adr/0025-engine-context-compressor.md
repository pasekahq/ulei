# ADR-0025: Engine context compressor

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The reference compacts a long session in `agent/context_compressor.py`
`ContextCompressor.compress`. Its default path (`tail_mode` `legacy`) prunes old tool output,
keeps a head and a token-budget tail, summarizes the middle with a structured template through
`_generate_summary`, updates an earlier summary on the next compaction, and falls back to
`_build_static_fallback_summary` when the summary call fails. The engine reaches models only
through qb (ADR-0009) and stores sessions in C8 (ADR-0014), whose `compact` replaces the live
set in place.

## Decision

`crates/engine/src/compress.rs` and `compress/text.rs` port that default path. `Compressor::compact`
reads the live set of a session, compresses it and stores the result with `Journal::compact`;
the caller gives the route's context window (`Window`) and the unix time. When compaction runs
is decided by its callers, not here.

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

C4G requires `max_tokens`; the summary turn carries the engine's bound of one model turn, never
the summary budget, which stays prompt guidance (`Target ~N tokens`) as in the reference.

### 3. Differences from the reference

| Upstream function | Reference | Engine | Why |
|---|---|---|---|
| `_generate_summary` | `call_llm(task="compression")` on the auxiliary model, with `summary_model`, a retry on the main model (`_fallback_to_main_for_compression`) and provider resolution | one tool-less C4G turn on the configured alias, no system prompt, no session key | Models only through qb (ADR-0009, port map R1, R2); qb owns fallbacks. |
| `_generate_summary` | `aux_interrupt_protection` shields the summary call from an interrupt | the interrupt cancels the turn at qb; the compaction then changes nothing, as on `AuxiliaryExplicitCancellation` | Port map R4: an emergency cancel stops whatever runs. |
| `_generate_summary`, `_is_summary_access_or_quota_error`, `_is_connection_error` | a failed call is classified by the provider exception's type, HTTP status and text | the C4G end of the turn is mapped: `auth` and `quota_exhausted` are the access or quota failure (abort, 60 s); `unavailable`, no vendor of the route answered, is the missing provider (fallback naming `no auxiliary LLM provider configured`, 600 s); `overloaded` is the rate limit (fallback, timeout ladder); the turn deadline is a timeout and a connection failure (abort, timeout ladder); a failed connection or a stream that ends without a terminal event is a connection failure (abort, 30 s); a malformed answer is the JSON error (fallback, 30 s); anything else falls back with 60 s. The error text is still read for the reference's timeout and connection markers | C4G reports a failure class instead of a vendor exception; qb's class is authoritative and its HTTP status informative. |
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
| `compress` | the no-op branches return the pruned list and count an ineffective compaction | nothing is stored and the caller gets `Skip`; no counter | The counter feeds the trigger's anti-thrash guard, which comes with the trigger. |
| `compress` | feasibility skip after an ineffective compaction (`_FEASIBILITY_SKIP_MIDDLE_FRACTION`) | not ported | It reads the counter above. |
| `compress` | `_prune_stale_reasoning_replay` | not ported | No C4G block carries replayed reasoning items. |
| `_generate_summary` | the memory provider section | not ported | The engine has no memory provider; the reference default passes none. |
| `_record_compression_failure_cooldown`, `get_active_compression_failure_cooldown` | the cooldown persisted in the session store | kept in the `Compressor` | C8 has no cooldown column. |

## Consequences

- A compaction runs only when a caller asks; until a trigger calls it, `ulei-engine run` reads
  none of its settings.
- A second compaction of a session that restarted in between finds the earlier summary in the
  live set and updates it, as one that never stopped does.
