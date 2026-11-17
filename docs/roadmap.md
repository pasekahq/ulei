# Roadmap

Owner decision of 2026-11-17: the ecosystem runs its **own Rust agent loop** and is
**provider-agnostic**. No subscription, vendor or CLI is load-bearing: keys, providers and
models switch by configuration and at runtime, with a fallback chain. This replaces the
earlier plan of an official vendor CLI or SDK engine (old step S6) and reorders ADR-0006:
first a thin vertical slice "the owner's assistant lives on ulei", next to the current Python
runtime, which stays a hot standby; then the remaining steps behind flags. ADR-0009 (sprint U1)
records the change.

## Providers

One `Provider` trait, interchangeable adapters, chosen per turn from a configured chain:

| Adapter | Auth | Role |
|---|---|---|
| Anthropic Messages | subscription OAuth (several accounts, rotation, refresh) | primary |
| OpenAI Responses (ChatGPT/Codex backend) | subscription OAuth (several accounts, rotation, refresh) | primary |
| Anthropic Messages | API key | fallback, last in chain, spend cap |
| OpenAI Responses / Chat Completions | API key | fallback |
| OpenRouter (Chat Completions) | API key | fallback |
| qb (`/v1/pick`, `/v1/report`) | — | optional account picker over the wire, once qb is ready |

A credential pool per provider marks accounts `ok | cooldown(until) | dead`, refreshes OAuth
tokens on expiry or 401, moves to the next account on a usage limit, then to the next chain
entry. Exhaustion of the whole chain yields one owner-visible line with the nearest reset time.

## MVP sprints

| Sprint | Scope | Done when |
|---|---|---|
| **U1** providers and loop | ADR-0009/0010; `crates/engine`: provider trait and canonical message model, sync SSE, Anthropic (OAuth + key), OpenAI Responses (OAuth + key), Chat Completions (OpenRouter), credential pool with rotation and refresh, fallback chain, hot switch of provider/model/key, tool loop with core tools, `ulei-engine run` | recorded-stream tests for every adapter; tool loop passes on a scripted fake provider; live smoke is a manual job outside CI; budgets in CI |
| **U2** the assistant | sessions and journal (C8), compaction, prompt from memory files, skills as files (index + view tool), MCP stdio client, web tools, guard policies, C6 server with a turn queue, interrupt, quota message | a scripted multi-session run with compaction and skills passes; the C6 client passes against the server |
| **U3** owner channel | `ulei-tg`: long poll, owner allowlist, coalescing, text/photo/document/voice (STT), forwards, replies, HTML rendering with plain fallback, `/new /stop /status /model` | end-to-end test fake Bot API → channel → fake engine → reply |
| **U4** inbox delivery and ops | claim → inject → ack/retry loop in core; `ulei.toml`, `ulei status`, systemd user units, musl release, runbook for pilot, cutover and rollback | 0 lost / 0 duplicated events on 10^4 with process kills; runbook reviewed |
| **Pilot** | a separate test bot on the live host for 7 days; the current runtime keeps the owner chat | the scenario checklist is green or filed into U5 |
| **U5** parity | fixes found in the pilot | pilot list closed |
| **Cutover** | owner inbox and the owner bot move to ulei by flag/`base_url`; one-command rollback; the old runtime stays hot for 30 days | 7 days of clean delivery |

The Python side (task hive, gate, scheduled series, scripts, MCP servers) stays as is and is
reached over the wire.

## After the MVP

| Step | Scope |
|---|---|
| P1 | qb as the account picker for every provider; reserve for the chat |
| P2 | C2 MCP facade in Rust (hive, inbox, garden) instead of shell calls |
| P3 | forest archive of old sessions with search |
| P4 | garden C7: memory files become a render of the garden |
| P5 | remaining policies, approval buttons in the channel |
| P6 | inbox producers on the wire client; Python copies removed (old S1 tail) |
| G  | gate split; consent filter moves into the channel glue |
| P8 | task hive in Rust next to the Python one |
| S7 | old runtime archived after 30 days without rollbacks |

Done before the MVP: **S0** skeleton (contracts, CI, ADR 0001–0006) and **S1** owner inbox
store, daemon and shadow in `ulei-core`, C6 client (sprint of 2026-11-16).
