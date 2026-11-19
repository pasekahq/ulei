# Roadmap

Owner decisions of 2026-11-17:

1. The ecosystem runs its **own Rust agent loop** (`ulei-engine`): tool loop, tool calls,
   sessions, compaction, skills as files, memory as the owner's file library. No vendor CLI or
   SDK is the engine.
2. **All model access goes through qb** (vladmesh/quota-balancer). The agent loop knows exactly
   one provider: the qb model gateway, over a local unix socket and a versioned contract (C4G).
   Subscription OAuth logins, API keys, OpenRouter, account rotation, quotas, resets, fallback
   chains and switching of provider/model/key all live in qb and its config. ulei has **no**
   vendor adapter, vendor endpoint or vendor credential; CI enforces this. Anything qb lacks is
   built in qb's own sprints, not here.
3. First a thin vertical slice "the owner's assistant lives on ulei", next to the current Python
   runtime, which stays a hot standby; then the remaining steps behind flags. This reorders
   ADR-0006; ADR-0009 (sprint U1) records it.

## Shape of the slice

```
Telegram ── ulei-tg (channels) ──C6 turn──▶ ulei-engine ◀──C6 inject── ulei-inbox (core)
                                              │ tool loop, tools, sessions (C8),
                                              │ compaction, prompt from memory files,
                                              │ skills as files, policies, MCP client
                                              ▼ C4G model gateway (UDS, NDJSON stream)
                                            qb gateway ── qbd (pick/report, leases)
                                              └─ vendors: subscription OAuth, API keys, OpenRouter
```

The model route is chosen per request by an alias (`chat`, `compact`, ...). Changing the
provider, model, key or the fallback chain is a qb config change (hot reload); ulei only picks the
alias, also per session at runtime (`/model <alias>`).

## MVP sprints (ulei)

| Sprint | Scope | Done when |
|---|---|---|
| **U1** agent loop against qb | ADR-0009 (slice, own loop, qb-only), ADR-0010 (`ulei-engine` crate and deps); C4G gateway contract draft with fixtures; architecture rule "no vendor endpoints or SDKs in ulei"; qb client over UDS with a scripted fake qb; tool loop with core tools (terminal, read_file, write_file, patch, search_files) and a policy hook; C8 session journal; `ulei-engine run` | the loop passes scripted multi-step tool runs, errors, quota exhaustion and resume after restart on the fake qb; a provocation with a vendor host turns CI red; budgets in CI |
| **U2** the assistant | compaction through qb; prompt from memory files; skills index + view tool; MCP stdio client; web tools; guard policies; C6 server with a turn queue, interrupt and status; owner-visible quota line from qb errors; per-session route alias | a scripted multi-session run with compaction, skills and MCP passes; the C6 client passes against the server |
| **U3** owner channel | `ulei-tg`: long poll, owner allowlist, coalescing, text/photo/document/voice (STT), forwards, replies, HTML with plain fallback, `/new /stop /status /model` | end-to-end test fake Bot API → channel → fake engine → reply |
| **U4** inbox delivery and ops | claim → inject → ack/retry in core; `ulei.toml`, `ulei status`, systemd user units, musl release; runbook for pilot, cutover and rollback; live smoke against a real qb gateway | 0 lost / 0 duplicated events on 10^4 with process kills; runbook reviewed |
| **Pilot** | a separate test bot on the live host for 7 days; the current runtime keeps the owner chat | the scenario checklist is green or filed into U5 |
| **U5** parity | fixes found in the pilot | pilot list closed |
| **Cutover** | owner inbox and the owner bot move to ulei by flag/`base_url`; one-command rollback; the old runtime stays hot for 30 days | 7 days of clean delivery |

## Needed from qb (built in qb's sprints)

| qb step | Scope |
|---|---|
| G1 | model gateway role: owns the C4G gateway contract; credential store; Anthropic Messages with subscription OAuth (refresh) and API key; stream translation; quota windows from responses into qb's policy; rotation across logins |
| G2 | OpenAI Responses with subscription OAuth and API key; Chat Completions for OpenRouter and other keys; cross-vendor fallback chains; route aliases with hot reload; spend caps for keys; login import and `qb login` |

ulei develops against a scripted fake of the C4G contract until G1 ships; the live smoke is in U4.

The Python side (task hive, gate, scheduled series, scripts, MCP servers) stays as is and is
reached over the wire.

## After the MVP

| Step | Scope |
|---|---|
| P1 | C2 MCP facade in Rust (hive, inbox, garden) instead of shell calls |
| P2 | forest archive of old sessions with search |
| P3 | garden C7: memory files become a render of the garden |
| P4 | remaining policies, approval buttons in the channel |
| P5 | inbox producers on the wire client; Python copies removed (old S1 tail) |
| G  | gate split; consent filter moves into the channel glue |
| P6 | task hive in Rust next to the Python one |
| S7 | old runtime archived after 30 days without rollbacks |

Done before the MVP: **S0** skeleton (contracts, CI, ADR 0001–0006) and **S1** owner inbox
store, daemon and shadow in `ulei-core`, C6 client (sprint of 2026-11-16).
