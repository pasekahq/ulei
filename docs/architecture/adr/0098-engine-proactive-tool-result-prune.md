# ADR-0098: Engine proactive prune of old tool results

Status: accepted
Date: 2026-11-27
Affects: crates/engine, contract:C8, contract:C9
Amends: ADR-0025 (a second, model-free rewrite of the live set)

## Context

The reference prunes old tool results between compactions, without a model call:
`agent/context_compressor.py` `ContextCompressor.prune_tool_results_only`, called from the
post-tool gate of `agent/conversation_loop.py` when the compaction gate did not fire, with
`proactive_prune_tokens`, `proactive_prune_min_result_chars` and
`proactive_prune_min_reclaim_tokens` (`agent/agent_init.py`). The owner's profile sets
`compression.proactive_prune_tokens: 120000` and leaves the other two at 8000 and 4096. The
port of ADR-0025 runs `_prune_old_tool_results` only inside a full compaction, and a route
without `context_length` compacts only after `context_too_long`, so a session of the engine
re-sends every old tool result verbatim on every step until it compacts. In the reference the
prune took about 25,000 tokens off the prompt each time it fired at 120,000 (126,000 to 98,000,
121,000 to 94,000).

## Decision

### 1. The prune

`Compressor::prune` ports `prune_tool_results_only`. `Agent::before_step` calls it before every
step that did not compact, with or without a window, when the last message is the results of
the step's tool calls and the turn holds no prefill or nudge (the reference's post-tool gate,
the `elif` branch of the compaction gate). It does nothing unless:

1. `proactive_prune_tokens` is above 0 and the prompt size the compaction gate reads (the last
   reported prompt tokens, a rough count of the request without one, none right after a
   compaction) is at least it;
2. the conversation has more rows (ADR-0025 section 1) than `protect_last_n` plus the head and
   one;
3. the rough tokens of the rows (`budget_tokens`, the tail walk's estimate) reached the runway of
   the last prune (section 2).

It then runs the three passes of `_prune_old_tool_results` with a tail of `protect_last_n` rows
by count, not by the token budget, and no pressure pass: equal tool results of at least 200
characters anywhere become `[Duplicate tool output — same content as a more recent call]`
except the newest; tool results longer than `proactive_prune_min_result_chars` before the tail
become their one-line `_summarize_tool_result` descriptions; long arguments of the tool calls
before the tail are cut. The prune is kept only when a pass replaced a tool result (cut
arguments alone do not count, as in the reference) and the rough tokens fell by at least
`proactive_prune_min_reclaim_tokens`. A kept prune becomes the turn's history as a compaction's
set does (`Live::replace`), is stored by the turn's `Journal::append` as a compacted set
(archived originals, origin `compaction`), and writes a `compaction` trace line with trigger
`proactive_prune` (ADR-0092). It sends no model turn and resets none of the turn's recovery.

### 2. The runway and C8 `PruneRearm`

A kept prune sets its runway, as the reference's `_proactive_prune_rearm_tokens`: the rough
tokens after it plus the most of the tokens it reclaimed, `proactive_prune_tokens` and
`proactive_prune_min_reclaim_tokens`. A compaction clears it. The reference keeps it in the
session row's model config so that it survives a restart; `ulei-engine run` runs one turn per
process (ADR-0025), so without a durable copy every turn would prune again and break the
prompt cache from the oldest newly pruned message on. The runway is stored as C8 0.10.0
`PruneRearm`, a row `prune_rearm.<session>` of the journal's `settings` table, written in the
transaction that stores the pruned set and removed in the transaction of any compaction
(`Journal::append` with a compacted set of a compaction, `Journal::compact`). `ulei-engine
run` restores it with the last usage when it binds the compressor. The journal's schema
version stays 9: an older engine opens the file and never reads the row.

### 3. Settings (C9 0.14.0)

| Key | Default | Reference | Value source |
|---|---|---|---|
| `engine.compression.proactive_prune_tokens` | 120000; 0 never prunes | `compression.proactive_prune_tokens`; code default 0 | profile value |
| `engine.compression.proactive_prune_min_result_chars` | 8000; at least 200, 0 is the default | `proactive_prune_min_result_chars` 8000, floored at `_PRUNE_MIN_CHARS` | code default, equal to the profile |
| `engine.compression.proactive_prune_min_reclaim_tokens` | 4096; 0 keeps any prune | `proactive_prune_min_reclaim_tokens` 4096 | code default, equal to the profile |

Each `ULEI_ENGINE_COMPRESSION_PROACTIVE_PRUNE_*` variable wins over its key (ADR-0040).

### 4. Differences from the reference

| Upstream function | Reference | Engine | Why |
|---|---|---|---|
| `prune_tool_results_only` | the capability gate on a session store without `archive_and_compact`; a failed commit keeps the original transcript | none; the pruned set is stored with the turn, whose failure fails the turn as for a compaction | C8 always stores a compacted set in the turn's transaction. |
| `_prune_old_tool_results` | ghost-skill defense for just-loaded `skill_view` results | not ported, as for the compaction's prune (ADR-0025 section 3) | Same reason. |
| `update_model`, `_clear_durable_proactive_prune_rearm` | a model switch clears the runway | kept | The runway does not depend on the route's window. |
| `conversation_loop.py` | the prune after every tool batch, the rows stamped `_DB_PERSISTED_MARKER` | the prune before the next step, stored with the turn | The engine stores a turn once, at its end (ADR-0025). |

## Consequences

- A session on a route without `context_length` no longer carries every old tool result of
  more than 8000 characters once its prompt reaches 120,000 tokens; the full results stay in the
  journal as archived messages and in the history search.
- Each kept prune breaks the prompt cache from its oldest rewritten message on, at most once per
  runway, as in the reference.
- The journal grows by one archived copy of the live set per kept prune, as per compaction.
