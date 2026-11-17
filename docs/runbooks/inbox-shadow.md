# Runbook: `ulei-inbox shadow` beside the live Python inbox

The shadow replays the live Python inbox through the Rust packet checks and store and writes
every difference to a JSONL file. It runs as one pass per timer tick for at least 14 days
before any consumer switches (ADR-0006). The semantics are in ADR-0007 section 7; this page
is how an operator runs it.

Placeholders used below:

| Placeholder | Meaning |
|---|---|
| `<live-root>` | Directory of the live Python inbox: `events.sqlite3` and `producer.key`. |
| `<bin-dir>` | Directory holding the `ulei` and `ulei-inbox` binaries. |
| `<state-dir>` | Private state of the shadow: its Rust store and snapshot. |
| `<out-dir>` | Directory of the divergence file `<out-dir>/divergences.jsonl`. |
| `<user>` | The account that owns `<live-root>` and runs the Python inbox. |

## Prerequisites

- Same uid. The shadow runs as `<user>`, the uid that owns `<live-root>`. It refuses a
  `producer.key` that is a symlink, is owned by another uid, has any group or other mode bit or
  is not 32 bytes, and the live database is 0600, so another account cannot read either.
- Binaries from main. Build both binaries from one main commit and place them side by side:

  ```sh
  cargo build --release --locked -p ulei-core --target x86_64-unknown-linux-musl
  install -m 0755 target/x86_64-unknown-linux-musl/release/ulei \
      target/x86_64-unknown-linux-musl/release/ulei-inbox <bin-dir>/
  <bin-dir>/ulei --version
  ```

  `ulei inbox shadow ...` executes `<bin-dir>/ulei-inbox shadow ...`; it takes the sibling
  from the directory of the running `ulei`, never from `PATH`. Either spelling may be used.
  Record the commit the binaries were built from next to them; a rebuild during the 14 days
  is a new binary under test.
- State and output outside the live root. `<state-dir>` and `<out-dir>` must not lie inside
  `<live-root>`; the shadow refuses them with exit 1 before it writes anything. Both are owned
  by `<user>`, mode 0700, and neither they nor `divergences.jsonl` is a symlink. The shadow
  creates `<state-dir>` 0700 when it is missing and refuses it when it is a symlink, foreign
  or group or other accessible. It does not create `<out-dir>`; create it first:

  ```sh
  install -d -m 0700 <state-dir> <out-dir>
  ```

  `<state-dir>` holds a copy of every packet, payload text included: protect it like the live
  inbox.
- One shadow per live root. One timer, one `<state-dir>` and one `divergences.jsonl` per
  `<live-root>`. Never point a second shadow at the same live root, and never reuse a
  `<state-dir>` for another live root: its snapshot rows would all read as gone from live.
- `jq` on the host, for reading the JSONL below.

## What a pass reads and guarantees

A pass:

1. Refuses `<state-dir>` or `<out-dir>` inside `<live-root>`.
2. Reads `<live-root>/producer.key` read-only. The key is read on every pass and never copied:
   nothing is written under `<state-dir>` as a key file.
3. Opens `<live-root>/events.sqlite3` read-only (`mode=ro`, `query_only`, busy timeout 0) and
   reads all `events` rows in one short read transaction. While it holds that transaction a
   Python commit waits for it, up to the 10 s lock timeout of the Python inbox; the read is
   one `SELECT` of the table.
4. Replays every row into the Rust store under `<state-dir>`, checks each row against the
   snapshot of the previous pass, appends one line per divergence to `divergences.jsonl` and
   then commits the snapshot.

Nothing under `<live-root>` is created, written, renamed or deleted, the journal included. A
busy or hot live database ends the pass with exit 75 before anything under `<state-dir>` or
`<out-dir>` is touched.

## Scheduling

Run the pass as a systemd user `oneshot` service started by a timer every 5 minutes. A
oneshot process exits after its pass, so between ticks nothing of the shadow is running and
it causes no idle wake-ups; a resident loop would wake to poll. systemd does not start the
service again while a pass is still running, so passes never overlap.

`$XDG_CONFIG_HOME/systemd/user/ulei-inbox-shadow.service`:

```ini
[Unit]
Description=ulei-inbox shadow pass over the live Python inbox

[Service]
Type=oneshot
ExecStart=<bin-dir>/ulei-inbox shadow --live-root <live-root> --state <state-dir> --out <out-dir>/divergences.jsonl
SuccessExitStatus=75
UMask=0077
```

`$XDG_CONFIG_HOME/systemd/user/ulei-inbox-shadow.timer`:

```ini
[Unit]
Description=ulei-inbox shadow pass every 5 minutes

[Timer]
OnCalendar=*:0/5
AccuracySec=10s

[Install]
WantedBy=timers.target
```

`SuccessExitStatus=75` keeps a retry-later pass from marking the unit failed; exit 1 and 2
still do. Enable it as `<user>`:

```sh
loginctl enable-linger <user>     # once, so user timers run without a login session
systemctl --user daemon-reload
systemctl --user enable --now ulei-inbox-shadow.timer
systemctl --user start ulei-inbox-shadow.service   # first pass now
systemctl --user list-timers ulei-inbox-shadow.timer
```

Start a manual pass only with `systemctl --user start ulei-inbox-shadow.service`, so it
cannot overlap a timer pass. The first pass on an empty `<state-dir>` replays every row and
has no snapshot to compare with; transitions are checked from the second pass on.

The 14-day count below reads the user journal. Check that it keeps at least 14 days
(`journalctl --user --disk-usage`, `Storage=persistent` and the retention settings of
`journald.conf`); if it does not, raise the retention before the window starts.

## Exit codes

| Exit | Output | Meaning | Operator action |
|---|---|---|---|
| 0 | stdout `ulei-inbox shadow: N rows, M divergences` | The pass completed; `N` live rows read, `M` lines appended. | None when `M` is 0. When `M` is above 0, read the new lines (next section). |
| 75 | stderr `ulei-inbox shadow: live database cannot be read consistently now, retry later: ...` | The live database was locked by a writer, or holds a hot journal a read-only reader may not roll back. State and output are untouched. | None; the next tick retries. If every pass for an hour exits 75, check that the Python inbox is running: its next access rolls a hot journal back. Never delete or edit the journal by hand. |
| 1 | stderr `ulei-inbox: shadow: ...` | The shadow itself failed: a path inside the live root, a key or state path refused (symlink, foreign owner, group or other mode bits, key length), an I/O or SQLite error. | Read the message, fix the cause, start a manual pass and confirm exit 0. An exit 1 restarts the 14-day count. |
| 2 | stderr starting `usage: ulei-inbox serve ...` | The command line is wrong. | Fix `ExecStart` against the usage line below, `daemon-reload`, start a manual pass. |

The usage line, also printed by `ulei-inbox --help`:

```text
usage: ulei-inbox serve --root DIR --socket PATH
       ulei-inbox shadow --live-root DIR --state DIR --out FILE.jsonl
```

Each flag is required exactly once, in any order.

## Reading the JSONL

Each line is one canonical JSON object with exactly these keys, in this order:

```json
{"check":"digest","detail":"digest column differs from the recomputed digest","id":"<event id>","live":"<64 hex>","rust":"<64 hex>","ts":1763400000.5}
```

- `check`: one of the seven names below.
- `detail`: a fixed short text naming the fault.
- `id`: the event id, or `null` when the live `id` column is not text.
- `live`: the value seen in the live inbox; `rust`: the value the Rust side computed or the
  snapshot held. Packet bytes appear only as `sha256:<hex>`.
- `ts`: Unix time of the pass that appended the line; all lines of one pass share it.

| `check` | Meaning | `live` / `rust` | First thing to look at |
|---|---|---|---|
| `packet_bytes` | The stored packet is not JSON, or `canonical(packet)` differs from the stored bytes. | `sha256:` of the stored bytes / of the Rust canonical bytes, or `null` | Whether the producer wrote the row through the Python `canonical()`; a non-canonical row means another writer or a canonical-rule gap in Rust. |
| `digest` | The `digest` column differs from the SHA-256 recomputed over the packet without `mac`. | stored digest / recomputed digest | The same row for a `packet_bytes` record; a digest alone points at the Rust digest rule. |
| `mac` | The MAC does not verify under the live `producer.key`. | stored MAC / `null` | Whether `producer.key` changed since the row was published, then the producer that signed it. |
| `publish` | The Rust store refuses a packet the live inbox accepted, or keeps another id, other bytes or another digest. | stored value or `sha256:` / Rust value or `null` | The refusal reason in `detail`: a Python-accepted value the Rust checks refuse (for example `version` `1.0`). |
| `row` | A column is not valid for C1 `Row` or has the wrong SQLite type, or a row of the snapshot is gone from live. | live value or `null` / `null`, or `null` / snapshot digest for a gone row | The column named in `detail`; for a gone row, what deleted it from `events`. |
| `transition` | The new status is not reachable from the snapshot status along the lease state machine. | new status / snapshot status | The Python call that moved the row (`claim`, `start`, `ack`, `retry`, `recover`). |
| `immutable` | `packet`, `digest` or `created` changed, or `attempts` decreased, since the snapshot. | new value / snapshot value | What rewrote the row in place; the live inbox only inserts these columns once. |

A row with a `packet_bytes`, `digest` or `mac` record is not published, so each fault gives
one record.

Records repeat. Only rows with no divergence in a pass enter the snapshot, so a divergent row
is compared with its last valid state and gives the same records on every pass, with a new
`ts`, until it is valid again. A gone row repeats on every pass. A pass interrupted after
appending its lines and before committing its snapshot (a crash or a failed snapshot commit) repeats those
lines on the next pass. Group by fault, not by line:

```sh
jq -r '[.check, (.id|tostring), .detail] | @tsv' <out-dir>/divergences.jsonl | sort | uniq -c
```

No payload text ever appears: packet bytes are given only as `sha256:` hashes, `detail` is a
fixed text, and a `mac` record never shows the recomputed MAC, which would be a valid
signature for the stored bytes. To see the packet behind a record, open the live row by `id`
read-only as `<user>`.

A divergence that cannot clear by itself (a gone row, a changed `created`, a status left a
terminal state) keeps repeating. After it is triaged and fixed, reset the shadow: stop the
timer, move `<state-dir>` and `divergences.jsonl` aside, recreate `<state-dir>` 0700 and
start again. A reset starts a new 14-day window.

## Monitoring and the 14-day exit criterion

The shadow passes when, over 14 consecutive days:

- no divergence line is appended, and
- no pass exits 1, and
- at least one pass completes with exit 0 on every day.

The last condition rules out a stopped timer that passes by silence.

Day of the newest line and lines per day (UTC):

```sh
jq -r '.ts | floor | strftime("%Y-%m-%d")' <out-dir>/divergences.jsonl | sort | uniq -c | tail -n 15
```

The window can only start after the `ts` of the last line. A missing file means no line was
ever appended.

Exits 1, retries and completed passes from the journal:

```sh
journalctl --user -u ulei-inbox-shadow.service --since "14 days ago" -o cat > shadow.log
grep -c '^ulei-inbox: ' shadow.log                            # exit 1
grep -c '^usage: ' shadow.log                                 # exit 2
grep -c 'retry later' shadow.log                              # exit 75
grep -c '^ulei-inbox shadow: [0-9]* rows' shadow.log          # exit 0
journalctl --user -u ulei-inbox-shadow.service --since "14 days ago" -o short-iso \
  | grep 'ulei-inbox shadow: [0-9]* rows' | cut -c1-10 | sort | uniq -c   # passes per day
```

The window holds when the first two counts are 0, the per-day list has 14 days, and the
JSONL has no line with `ts` inside the window. Retries (75) do not break the window; a
growing share of them means passes collide with Python commits and the cadence is worth
revisiting.

## Limits

- Rule-4 rows. A packet with a float `source_ts` below 1e-4 skips the byte, digest and MAC
  recompute, because Rust cannot reproduce its canonical bytes (ADR-0007 section 5, rule 4).
  The Rust store refuses it as expected and no record is written; its `row`, `transition`
  and `immutable` checks still run. A fault in the bytes, digest or MAC of such a row is not
  seen.
- Multi-edge passes. Between two passes a row may move along several legal edges, so the
  shadow checks only that the new status is reachable from the snapshot status. An illegal
  step in between that ends in a reachable status is not seen. A row may change its other
  columns without a status change when its status lies on a cycle (`pending`, `leased`,
  `processing`, `deferred`); any change of a `done` or `uncertain` row is a `transition`
  record.

## Stopping and removing

Nothing below touches `<live-root>`; the Python inbox keeps running throughout.

```sh
systemctl --user disable --now ulei-inbox-shadow.timer
systemctl --user stop ulei-inbox-shadow.service      # waits for a running pass
rm $XDG_CONFIG_HOME/systemd/user/ulei-inbox-shadow.service \
   $XDG_CONFIG_HOME/systemd/user/ulei-inbox-shadow.timer
systemctl --user daemon-reload
```

Keep `divergences.jsonl` and the journal counts as the evidence of the window. Then remove
`<state-dir>`, which holds packet copies with payload text, and the binaries in `<bin-dir>`
if nothing else uses them. Never remove or edit anything under `<live-root>`. Disable linger
(`loginctl disable-linger <user>`) only if no other user unit needs it.

## After the window

Passing the 14 days does not switch anything. The switch of consumers to the Rust inbox is a
separate decision and Hive card, made behind a flag after at least 14 days of shadow
(ADR-0006); it cites the JSONL and journal counts of the window. Keep the timer running
until that card says otherwise.
