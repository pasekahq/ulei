# ADR-0024: `sha2` for the engine's tool-call guardrails

Status: accepted
Date: 2026-11-18
Affects: crates/engine, dep:sha2

## Context

The `tool-guardrails` policy of `ulei-engine` ports the reference's per-turn tool-call loop
guardrail (`THIRD_PARTY_NOTICES.md#ref-agent`, `agent/tool_guardrails.py`). It identifies a call
by its tool name and the SHA-256 of its canonical arguments, and a result by the SHA-256 of its
canonical JSON or text. The `args_hash` of a call reaches the model in the result of a blocked
call. std has no SHA-256; ADR-0010 lets each dependency of the engine in only with its own
record.

## Decision

### 1. Dependency

| Crate | Features | Why |
|---|---|---|
| `sha2` 0.11 (workspace, as in ADR-0007) | none | SHA-256 of the canonical arguments and results, lowercase hex as the reference writes `args_hash`. Already in the workspace lock and the license allow list. |

### 2. Measured size

Release profile of ADR-0001, rustc 1.100.0, `x86_64-unknown-linux-gnu` (dynamically linked to
glibc, stripped), measured as in ADR-0010 section 3:

| Build | Size |
|---|---|
| `ulei-engine` with `tool-guardrails` and `sha2` | 2,194,744 B |
| Same binary before them | 2,152,760 B |

Of the 41,984 B, the portable SHA-256 compression function of `sha2` takes about 10.6 KB and its
SHA-NI variant about 0.7 KB; the rest is the policy, its hash maps and the canonical JSON writer.

## Consequences

- The `args_hash` of a blocked call equals the reference's for arguments of strings, integers,
  booleans, null, and arrays and objects of them.
- About 0.4 MiB of the static budget of ADR-0010 is left.
