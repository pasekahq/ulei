# ADR-0006: Strangler order and "Hermes patches only shrink"

Status: accepted
Date: 2026-11-16
Affects: crates/runtime-adapters
Amended by: ADR-0009 (step order per the roadmap, pilot on a test bot instead of the 14-day chat shadow).

## Decision

Hermes is replaced around its loop, never by a home-grown LLM loop on subscription OAuth:

| Step | What | Contracts |
|---|---|---|
| S0 | this skeleton | C1-C7 |
| S1 | owner inbox in `ulei-core` (from Hermes patch 0014), shadow next to the Python copy; drop dead cron patches 0002-0006 | C1, C6 |
| S2 | session store with cold archive (forest) | C6, C8 |
| S3 | providers through qb; chat off Anthropic OAuth | C4 |
| S4 | owner channel (Telegram) | C9 |
| S5 | memory and skills | C7 |
| S6 | tool loop on the official Agent SDK engine | C6 |
| S7 | Hermes retired | — |
| G | gate split (parallel, after the A2A MVP) | C5 |

Rules:
- every switch is a flag or a `base_url`, after at least 14 days in shadow mode;
- no destructive migrations: Hermes data is not deleted until the new path served its term;
- the number of lines in the Hermes patch series never grows from one step to the next;
- only `crates/runtime-adapters` knows about Hermes (enforced by `cargo xtask arch`).
