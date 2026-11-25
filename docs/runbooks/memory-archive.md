# Runbook: the archive daemon and the import of the old runtime's history

A manual procedure, never run in CI and never run by an agent: the owner runs it on the deployment
host, as the account that runs the ulei units. It sets up `ulei-memory serve` under its user unit
(ADR-0070), fills the archive with the history of the runtime ulei replaces through
`ulei-memory import`, checks it over C10, and restarts the engine so its tools `archive_search`
and `archive_read` reach the archive. Every step names its command, what it must show, and how to
roll it back. A step that fails stops the run: record the command, its output and exit code and
`journalctl --user -u ulei-memory -n 50`, then roll back from that step upwards.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_USER>` | The account that runs the ulei units. |
| `<ULEI_CHECKOUT>`, `<RELEASE_DIR>` | A checkout of the release's commit and the release directory, as in `docs/runbooks/mvp-pilot-cutover.md` 1.3. |
| `<CONFIG_HOME>`, `<STATE_HOME>`, `<RUNTIME_DIR>` | `$XDG_CONFIG_HOME` (default `$HOME/.config`), `$XDG_STATE_HOME` (default `$HOME/.local/state`) and `$XDG_RUNTIME_DIR` (`/run/user/<uid>`) of `<ULEI_USER>`. |
| `<HISTORY>` | The import file the deployment's converter wrote from the old runtime's history: JSON Lines of C10 `PutRequest`, below; gzip-compressed or not. |
| `<LIBRARY>` | The directory of the document library whose files the import lines name by `bytes_path`; readable by `<ULEI_USER>`. |
| `<BIG_ID>` | The id of an imported item over 1 MiB, for the checks. |
| `<IMPORT_LOG>` | A new file for the import's stderr, outside the history's directory. |
| `<WORD>` | A word that occurs in the imported history and nowhere else, for the checks. |

## 1. Install

The release holds `ulei-memory` and `deploy/install.sh` puts it and `ulei-memory.service` in place
with the other binaries and units (README, "Release and install"):

```sh
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
ls -l ~/.local/bin/ulei-memory "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-memory.service"
```

Expected: the script's `installed` or `unchanged` line for both, and both files listed.

Roll back: nothing to undo; the files stay unused until the unit is enabled.

## 2. `[memory]` in `ulei.toml`

Add the section of `docs/ulei.example.toml` with the paths of the unit's layout (ADR-0042
section 3, ADR-0070): the archive in the unit's state directory, the socket beside
`inbox.socket` and `engine.c6_socket`, where the engine's system unit can reach it (ADR-0069
section 2). `engine.c6_socket` must already be set: the daemon pulls the engine's dialogue from it.

```toml
[memory]
archive = "<STATE_HOME>/ulei/memory/memory.archive"
socket = "<RUNTIME_DIR>/ulei/memory.sock"
```

```sh
~/.local/bin/ulei status; echo "exit $?"
```

Expected: `exit 0` and the lines it printed before, so the file still decodes.

Roll back: remove the section.

## 3. Start the archive

On a host whose engine runs as the system unit (`mvp-pilot-cutover.md` 6.3, case (b)), first
give the archive the clients' drop-in, as for the inbox and the channel:

```sh
d=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-memory.service.d
install -d -m 0755 "$d"
install -m 0644 <ULEI_CHECKOUT>/deploy/systemd/system/client-no-userns.conf "$d/90-system-engine.conf"
```

Then, on every host:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ulei-memory.service
systemctl --user is-active ulei-memory.service
journalctl --user -u ulei-memory -n 5 --no-pager
c10() {
  curl -sS --unix-socket <RUNTIME_DIR>/ulei/memory.sock -X POST \
    -H 'Content-Type: application/json' -d "$2" "http://localhost/v1/$1"; echo
}
c10 health '{}'
```

Expected: `active`; the journal line `ulei-memory ready <RUNTIME_DIR>/ulei/memory.sock`; `health`
answers `{"version":"ulei-memory ...","items":N,"last_sync_at":...}` with `N` the engine's
dialogue messages pulled so far and no `sync_error`. A `sync_error` naming the engine's socket
means the engine was down or unreachable at the pull; the daemon runs anyway and pulls again at
the next `search` or `sync`.

Fail: `failed` or `activating (auto-restart)`. The journal's last line names the cause: a
refused config (exit 2, the unit stays stopped: fix step 2), an archive directory or socket
directory open to other users, or a socket path in use.

Roll back: `systemctl --user disable --now ulei-memory.service`; the archive file stays.

## 4. Import the old runtime's history

The input is JSON Lines, one C10 `PutRequest` per line (`contracts/c10-archive.schema.json`):

| Field | Value |
|---|---|
| `source` | `^[a-z][a-z0-9_-]{0,63}$`, one name per kind of history, never `dialogue` (the engine's own) nor a connector's name: for example `legacy-dialogue` and `legacy-library`. |
| `external_id` | The record's id in the old runtime, at most 512 bytes, stable: the same record gets the same id on every run of the converter, and a changed record a new one. |
| `who` | Optional: the author as the old runtime names it (`user`, `assistant`, a name). |
| `at` | Optional: Unix seconds of the record, a JSON number. |
| `kind` | The media type of the original bytes, for example `application/json` or `text/markdown; charset=utf-8`. |
| `text` | The text to index, extracted by the converter; the archive parses no format. |
| `bytes_b64` | The original record's bytes, standard base64 with padding. |
| `bytes_path` | Instead of `bytes_b64`: the absolute path of a file under `<LIBRARY>` holding the original, which `ulei-memory import` reads; the daemon never opens it. Exactly one of `bytes_b64` and `bytes_path`. |
| `id` | Optional: the original's lowercase hex SHA-256 (`sha256sum`); the import checks it before it sends anything and refuses the line when it differs. |

An original of any size goes in (ADR-0074). A line whose `put` body fits 1 MiB goes as one
request; any other goes in parts of 512 KiB, and the archive indexes the first 256 KiB of its
`text`. Prefer `bytes_path` for library files: a `bytes_b64` over 1 MiB is decoded into an
unlinked temporary file under `$TMPDIR` (default `/tmp`), which then needs room for the largest
such original. The members but `bytes_b64` of one line must stay under 4 MiB. Writing the
converter is the deployment's business, outside this repository. Check the first line before the
run:

```sh
zcat -f <HISTORY> | head -n 1 | cut -c 1-300
zcat -f <HISTORY> | wc -l
```

Run the import (it reads `memory.socket` from the same `ulei.toml`):

```sh
c10 health '{}'                                   # note "items" as BEFORE
zcat -f <HISTORY> | ~/.local/bin/ulei-memory import 2> <IMPORT_LOG>; echo "exit $?"
tail -n 1 <IMPORT_LOG>
grep -c ': refused ' <IMPORT_LOG>
```

Expected: `exit 0` and the last line `import: <lines> lines, <stored> stored, <duplicate>
duplicate, <refused> refused` with `<lines>` the count of `wc -l`; every refused line is one log
line `import: line <n>: refused <code>` (`invalid_request`: not an import line or bad base64, or
an upload whose parts did not hash to its id; `conflict`: the `(source, external_id)` is stored
with other bytes; `too_large`: the members but `bytes_b64` over 4 MiB), and a line refused before
any request adds its reason: `import: line <n>: refused invalid_request: bytes_path <path>: No
such file or directory (os error 2)` or `...: the original's SHA-256 is <hash>, not the line's
id <id>`. A progress line `import: at line <n>: ...` comes every 10,000 lines. Each small line
is one request of about 10 ms, so 100,000 lines take about 20 minutes; an original in parts takes
one request per 512 KiB, so about 3 GB of library takes some minutes more. The daemon serves the
engine and the connectors between requests.

Pass: exit 0 and the refused lines are those the converter expects (fix and re-run the converter
for the others; a re-run is safe, below).

Fail, exit 1: the daemon could not be reached after four tries over 7 s. The log's line before
the last reads `import: stopped at line <n>: <reason>; lines 1 to <m> are done, resume with
--skip <m>`. Bring the unit back (step 3), then resume:

```sh
zcat -f <HISTORY> | ~/.local/bin/ulei-memory import --skip <m> 2>> <IMPORT_LOG>; echo "exit $?"
```

A whole repeated run is safe too: every line already stored answers `duplicate` and changes
nothing. Exit 2 is a usage error of the command line.

Roll back: the archive keeps every item; an item is removed only by the owner's
`ulei-memory delete <id> --reason <text>` (crates/memory README). To start over, stop the unit and
move the archive file and its `-wal` and `-shm` files aside before step 3.

## 5. Check the archive over C10

```sh
c10 health '{}'
c10 search '{"q":"<WORD>","source":"legacy-dialogue","limit":3}'
c10 read '{"id":"<an id of the search>"}' | cut -c 1-300
```

Expected: `items` is BEFORE plus the `stored` of the import's last line, give or take dialogue
messages the engine wrote meanwhile; `search` answers hits with `entries` of that source, a
`snippet` with `>>>` and `<<<` around `<WORD>` and `"synced":true`; `read` answers the original
as `bytes_b64` with its `kind`.

An item over 1 MiB is read by range only; a whole `read` of it answers `too_large`. Read one back
in ranges of 512 KiB and compare its SHA-256 with its id:

```sh
off=0
while :; do
  c10 read "{\"id\":\"<BIG_ID>\",\"offset\":$off,\"length\":524288}" >/tmp/range.json
  n=$(python3 -c 'import base64,json,sys; d=json.load(open("/tmp/range.json")); b=base64.b64decode(d["bytes_b64"]); sys.stdout.buffer.write(b); print(len(b), file=sys.stderr)' 2>&1 >>/tmp/big.out | tail -n 1)
  [ "$n" -eq 0 ] && break
  off=$((off + n))
done
sha256sum /tmp/big.out; echo "<BIG_ID>"; rm -f /tmp/big.out /tmp/range.json
```

Expected: the two hashes are equal.

Fail: no hit for a word that is in the history: the converter's `text` field misses it.

## 6. Restart the engine

The engine reads `memory.socket` at start, so restart it once the archive serves. With the
engine's user unit:

```sh
systemctl --user restart ulei-engine.service
```

with its system unit (case (b)), as root: `systemctl restart ulei-engine.service`. Then, as
`<ULEI_USER>`:

```sh
~/.local/bin/ulei status; echo "exit $?"
```

and, to the owner bot: `Search the archive for <WORD> and quote what you find.`

Expected: `exit 0` with the session `idle`; the reply quotes the imported record. The tool asks
the archive only when the garden and the journal's history find nothing, so pick a `<WORD>` they
do not hold.

Fail: the reply says the archive is not configured: `memory.socket` is unset in the file the
engine reads, or the engine was not restarted; it says the archive did not answer: the engine's
unit cannot reach `<RUNTIME_DIR>/ulei/memory.sock` (case (b) without the drop-in of step 3).

Roll back: remove `[memory]` from `ulei.toml` and restart the engine; its archive tools then answer
that the archive is not configured.
