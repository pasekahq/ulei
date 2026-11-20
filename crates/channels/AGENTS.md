# AGENTS.md — `ulei-channels`

Owner channels (Telegram first): ingress, coalescing, delivery.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Bot tokens and chat ids come from config, never from code.
- `ulei.toml` is read only through `ulei_contracts::config`; this crate holds no TOML reader.
- Outbound HTTPS must stay synchronous and small (no tokio/reqwest; see ADR-0001).
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
