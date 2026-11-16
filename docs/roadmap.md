# Roadmap

The ecosystem replaces the current Python runtime around its agent loop step by step
(strangler fig). Every step puts a Rust implementation next to the old path, runs it in
shadow mode for at least 14 days, then switches by a flag. Details: ADR-0006.

| Step | Scope | Done when |
|---|---|---|
| **S0** skeleton | workspace, contracts C1–C7, CI gates, ADR 0001–0006 | CI green; a cross-module dependency turns CI red; module map regenerates without diff |
| **S1** owner inbox | `ulei-core` inbox over the existing `events.sqlite3` and C1 packet; UDS publish/claim/ack per C1/C6; shadow next to the Python copy; consumers move to the wire client behind a flag; dead cron patches dropped | the three Python copies are gone; 0 lost / 0 duplicated events over 14 + 7 days; the inbox daemon within its budgets |
| **S2** sessions and cold archive | forest archive of old sessions from a consistent snapshot, monthly read-only archives with FTS and sha256 manifests; `forest_search` | search over archive ≥ current session search on the eval set |
| **S3** providers via qb | chat provider chosen through qb; chat off subscription OAuth | provider switch by flag; ToS-clean path |
| **S4** owner channel | Telegram channel in front of the runtime (coalescing, rich inbound, forwards) | Telegram patches of the runtime removed |
| **S5** memory and skills | garden + retrieval + skills served by the ecosystem | runtime memory unused |
| **S6** tool loop | official Agent SDK engine behind C6 | runtime patches at 0 |
| **S7** runtime retired | old runtime archived | 30 days without it |
| **G** gate split | gate becomes a clean class-1 product; glue lives here | gate repo has no owner-specific code |

S1 is the first sprint after this skeleton.
