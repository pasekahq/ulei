# AGENTS.md — `ulei-channels`

Owner channels (Telegram first): ingress, coalescing, delivery.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Bot tokens and chat ids come from config, never from code.
- `ulei.toml` is read only through `ulei_contracts::config`; this crate holds no TOML reader.
- Outbound HTTPS must stay synchronous and small (no tokio/reqwest; see ADR-0001).
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- The owner's guest-gate traffic (ADR-0083) is classified in `ingress::guest` on the raw update,
  before the ingress, by the rules of the reference's `guest-gate-ingress` plugin; the call runs
  on `tg::guest_gate`'s worker thread, never on the loop thread. Log only the update id, the
  category and the outcome; never the text, the gate's URL or its token.
- `notify` (ADR-0097) is the direct path to the owner: Bot API `sendMessage` only, never a C6
  or C1 call, plain text without a parse mode. `ulei-tg notify` stays a thin wrapper of
  `notify::notify` and never changes the daemon's start line; its stderr line and the errors
  carry neither the text nor the token.
- `alert` (ADR-0097 section 5) is the uncertain alerter: per run one C1 `uncertain` call and at
  most one `notify::notify`, never a C6 call or another C1 method; a notice carries neither a
  payload nor the message of a C1 error. Its state file is written atomically and a failed send
  leaves it as it was. `ulei-tg uncertain` stays a thin wrapper of `alert::Alerter::run`, started
  by `ulei-uncertain.timer`.
- `uds` is the one client of the modules' Unix-socket HTTP wire; the C1 and C6 clients build on it.
- Check: `python3 -m ci.broad_check` from the repository root.
