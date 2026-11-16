# ADR-0004: Split of the A2A gate into a product and glue (step G)

Status: accepted
Date: 2026-11-16
Affects: contract:C5

## Decision

The A2A gate is a class-1 candidate that fails condition 4 today: its repository contains a
Hermes plugin, a copy of our inbox, search over the owner's library, persona and the owner's
Telegram facade. Step G moves those parts here (`ulei-glue` gate glue, C1 instead of the inbox
copy, C7 instead of private search, the channel from S4) and leaves the product with the wire
profile, preview -> consent -> single release, ledger, replay protection and the guest sandbox.

Until G, `contracts/c5-gate.openapi.json` is a **draft** derived from `docs/WIRE-V04.md` of the
gate repository; after G it becomes a vendored copy pinned in `ecosystem.lock` (with an ADR
`Affects: lock:guest-gate`). The gate stays Python (security code with a running MVP); it talks
to this repository only over the wire.
