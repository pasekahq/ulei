# Runbook: the garden as a user unit

A manual procedure, never run in CI and never run by an agent: the owner runs it on the deployment
host, as the account that runs the ulei units. It sets up `ulei-garden serve` under its user unit
(ADR-0075), checks it over C7, adds the embedder, moves a garden that runs under a hand-made unit
to the shipped one, restarts the engine so it sees the garden's socket, and checks that a turn
sees the garden. Every step names its command, what it must show, and how to roll it back. A step that fails stops the run: record the
command, its output and exit code and `journalctl --user -u ulei-garden -n 50`, then roll back
from that step upwards.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_USER>` | The account that runs the ulei units. |
| `<ULEI_CHECKOUT>`, `<RELEASE_DIR>` | A checkout of the release's commit and the release directory, as in `docs/runbooks/mvp-pilot-cutover.md` 1.3. |
| `<CONFIG_HOME>`, `<STATE_HOME>`, `<RUNTIME_DIR>`, `<DATA_HOME>` | `$XDG_CONFIG_HOME` (default `$HOME/.config`), `$XDG_STATE_HOME` (default `$HOME/.local/state`), `$XDG_RUNTIME_DIR` (`/run/user/<uid>`) and `$XDG_DATA_HOME` (default `$HOME/.local/share`) of `<ULEI_USER>`. |
| `<INBOX_ROOT>` | `inbox.root` of `ulei.toml`; the inbox creates `producer.key` there at its first start. |
| `<OLD_GARDEN_UNIT>` | The hand-made unit that runs the garden now, if any (`systemctl --user list-units 'run-*' 'garden*'` names it). |
| `<OLD_STORE>`, `<OLD_SOCKET>` | `garden.store` and `garden.socket` of the config that unit runs. |
| `<MODEL_DIR>` | The directory of the encoder engine's weight file `model.ulei-infer` (step 5), for example `<DATA_HOME>/ulei/garden/e5-small`; it holds no ONNX model. |

The unit writes only `<STATE_HOME>/ulei/garden/` and `<RUNTIME_DIR>/ulei/`, and reads every other
path. The engine's system unit (case (b) of `mvp-pilot-cutover.md` 6.1) sees of `<RUNTIME_DIR>`
only `ulei/`, so the socket lies there.

## 1. Install

The release holds `ulei-garden` and its embedder helper `ulei-garden-embed`, both static, and
`deploy/install.sh` puts both and `ulei-garden.service` in place with the other binaries and units
(README, "Release and install"):

```sh
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
ls -l ~/.local/bin/ulei-garden ~/.local/bin/ulei-garden-embed \
  "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-garden.service"
```

Expected: the script's `installed` or `unchanged` line for all three, and the three files listed.

Roll back: nothing to undo; the files stay unused until the unit is enabled.

## 2. `[garden]` in `ulei.toml`

Add the section of `docs/ulei.example.toml` with the paths of the unit's layout, the embedder left
out (step 5 adds it):

```toml
[garden]
store = "<STATE_HOME>/ulei/garden/garden.sqlite3"
socket = "<RUNTIME_DIR>/ulei/garden.sock"
producer_key_file = "<INBOX_ROOT>/producer.key"
core_file = "<STATE_HOME>/ulei/garden/core.md"
```

`inbox.socket` must already be set: the garden publishes its misses there. A `core_file` outside
`<STATE_HOME>/ulei/garden/` needs its directory in a drop-in (`systemctl --user edit
ulei-garden.service`, `[Service]` and `ReadWritePaths=<dir>`).

```sh
~/.local/bin/ulei status; echo "exit $?"
ls -l <INBOX_ROOT>/producer.key
```

Expected: `exit 0` and the lines it printed before, so the file still decodes; the key file with
mode `-rw-------` and `<ULEI_USER>` as its owner.

Roll back: remove the section.

## 3. From a hand-made unit

Skip this step when no garden runs yet. Otherwise stop the old unit first: the store has one
writer, and its socket would be in use.

```sh
systemctl --user stop <OLD_GARDEN_UNIT>
systemctl --user is-active <OLD_GARDEN_UNIT>
ls -l <OLD_STORE>*
```

Expected: `inactive` (or `unknown` for a transient unit that is gone), and the store file, with
`-wal` and `-shm` files beside it or not. Keep the store: it holds the facts and the seedbed.
Move it with its `-wal` and `-shm` files into the unit's state directory, which step 4 would
otherwise create empty:

```sh
install -d -m 0700 <STATE_HOME>/ulei/garden
for f in <OLD_STORE> <OLD_STORE>-wal <OLD_STORE>-shm; do
  [ -e "$f" ] && mv "$f" <STATE_HOME>/ulei/garden/garden.sqlite3"${f#<OLD_STORE>}"
done
ls -l <STATE_HOME>/ulei/garden/
rm -f <OLD_SOCKET>
```

Expected: `garden.sqlite3` with mode `-rw-------`, with its `-wal` and `-shm` if they were there.
A store on another file system may stay where it is instead: point `garden.store` at it and give
its directory to the unit in a drop-in as in step 2. If the old unit was a transient
`systemd-run` unit, nothing else is left of it; a unit file of its own: `systemctl --user disable
<OLD_GARDEN_UNIT>` and remove the file.

Roll back: move the files back to `<OLD_STORE>` and start `<OLD_GARDEN_UNIT>` again, after step 4's
roll back.

## 4. Start the garden

On a host whose engine runs as the system unit (`mvp-pilot-cutover.md` 6.3, case (b)), first
give the garden the clients' drop-in, as for the inbox, the channel and the archive; without it
the garden could not reach `inbox.socket` and publishes no miss:

```sh
d=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-garden.service.d
install -d -m 0755 "$d"
install -m 0644 <ULEI_CHECKOUT>/deploy/systemd/system/client-no-userns.conf "$d/90-system-engine.conf"
```

Then, on every host:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ulei-garden.service
systemctl --user is-active ulei-garden.service
journalctl --user -u ulei-garden -n 5 --no-pager
c7() {
  curl -sS --unix-socket <RUNTIME_DIR>/ulei/garden.sock -X POST \
    -H 'Content-Type: application/json' -d "$2" "http://localhost/v1/$1"; echo
}
c7 health '{}'
c7 search '{"q":"<a word of a stored fact>","no_miss":true}'
```

Expected: `active`; the journal line `ulei-garden ready <RUNTIME_DIR>/ulei/garden.sock`; `health`
answers `{"version":"ulei-garden ...","dense":"off","failed_proposes":0,"unpublished_misses":N}`;
`search` answers `facts`, the stored fact among them after step 3, without `lexical_only`: the
dense side is off, not down, so every answer is lexical by the config. A fact with an
`archive:` `source_ref` (lifted by the night from the history) ranks below a fact of other
evidence for the same question, its `score` a tenth of its rank's (`history_weight` 0.1,
ADR-0081); it is still first when nothing else matches. `unpublished_misses` above
0 that does not fall within a minute means the garden cannot reach `inbox.socket`.

Fail: `failed` or `activating (auto-restart)`. The journal's last line names the cause: a refused
config (exit 2, the unit stays stopped: fix step 2); a `garden.producer_key_file` that does not
exist yet (exit 1, restarted until the inbox has created it); a store or socket directory open to
other users, a socket path in use (step 3: the old unit still runs).

Roll back: `systemctl --user disable --now ulei-garden.service`; the store stays.

## 5. Add the embedder

The model runs only in the helper `ulei-garden-embed`, which the daemon starts as its child inside
the unit. The release's helper is a static binary with the encoder engine (ADR-0104): it reads the
engine's weight file `model.ulei-infer` in `<MODEL_DIR>`, which the release does not ship. Put it
under `<DATA_HOME>/ulei/garden/`, which the unit reads. A host whose garden already runs the
earlier ONNX helper takes the same steps, in this order: the weight file first, with the helper
from `<RELEASE_DIR>`, then the helper and the config.

### 5.1 The weight file

Download the pinned e5-small checkpoint (every file checked by SHA-256), convert it with the
release's helper exactly as CI does, and verify the result:

```sh
g=<DATA_HOME>/ulei/garden
install -d -m 0700 "$g" <MODEL_DIR>
python3 <ULEI_CHECKOUT>/ci/encoder_model.py checkpoint e5 "$g/e5-checkpoint"
gzip -dc <ULEI_CHECKOUT>/ci/encoder-ref/miracl-ru-en-docs3.u32.gz > "$g/e5-order.u32"
<RELEASE_DIR>/ulei-garden-embed convert --checkpoint "$g/e5-checkpoint" \
  --token-order "$g/e5-order.u32" --pooling mean --query-prefix 'query: ' \
  --passage-prefix 'passage: ' --act-block 32 --tokenizer-pipeline whitespace-split \
  --out <MODEL_DIR>/model.ulei-infer
<RELEASE_DIR>/ulei-garden-embed verify <MODEL_DIR>/model.ulei-infer; echo "exit $?"
ls <MODEL_DIR>
```

Expected: `convert` prints one JSON line whose `content_sha256` is the `digest` of `E5` in
`ci/encoder_model.py`; `verify` prints `OK <MODEL_DIR>/model.ulei-infer: content sha256 <the same
digest>, <bytes> bytes, ...` and `exit 0`; `<MODEL_DIR>` holds only `model.ulei-infer` (a directory
that also holds `onnx/model_quantized.onnx` is refused). Then remove the checkpoint and the order
file: `rm -r "$g/e5-checkpoint" "$g/e5-order.u32"`.

Fail: a download or `convert` that fails names its cause on one line; `verify` exit 3 means a
corrupted file: delete it and convert again.

Roll back: `rm -r <MODEL_DIR>`; nothing reads it yet.

### 5.2 The helper and the config

On a host whose `ulei.toml` already has `[garden.embedder]`, keep both for the roll back first:

```sh
cp -p ~/.local/bin/ulei-garden-embed <DATA_HOME>/ulei/garden/ulei-garden-embed.before
cp -p <CONFIG_HOME>/ulei/ulei.toml <DATA_HOME>/ulei/garden/ulei.toml.before
```

Step 1 installs the release's helper; on a host past step 1, run it again: it checks every file
against `SHA256SUMS` before it copies, and renames the helper into place, so a running helper keeps
its file until the daemon restarts it. Then check that the installed helper is the release's:

```sh
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
<ULEI_CHECKOUT>/deploy/install.sh --verify <RELEASE_DIR>
cmp ~/.local/bin/ulei-garden-embed <RELEASE_DIR>/ulei-garden-embed && echo same
file ~/.local/bin/ulei-garden-embed
```

Expected: an `installed` or `unchanged` line for `~/.local/bin/ulei-garden-embed`, the `verified`
line, `same`, and `static-pie linked`.

Then set `[garden.embedder]` in `ulei.toml`:

```toml
[garden.embedder]
enabled = true
helper = "<HOME>/.local/bin/ulei-garden-embed"
model_dir = "<MODEL_DIR>"
ort_library = "<DATA_HOME>/ulei/garden/onnxruntime/libonnxruntime.so"
```

with `<HOME>` the home of `<ULEI_USER>`. `model_dir` is the key that changes: it names the
directory of the weight file, not an ONNX model directory. `ort_library` stays because C9 requires
it with the embedder enabled; the release's helper ignores it and the file need not exist, as it
ignores `threads` (ADR-0087 open question 13). The helper's model identity changes with the weight
file, so the worker embeds every stored fact again in the background. Restart and check:

```sh
systemctl --user restart ulei-garden.service
systemctl --user is-active ulei-garden.service
c7 health '{}'
systemctl --user show -p MemoryCurrent -p MemoryPeak -p TasksCurrent ulei-garden.service
```

Expected: `active`; `health` answers
`"dense":"ready"` (`ulei status` has no garden line; `health` is the check). The first
`model_check` in `health` passes once the worker has verified the file after the start. From then
on a `search` without `lexical_only` comes from both sides; one with `"lexical_only":true` means the
helper is down, which `health` shows as `"dense":"down"` with the reason in `dense_error` until the
daemon restarts it. `vectors_missing` in `health` falls to 0 as the worker embeds the stored facts,
one per tick while no query arrives. `MemoryPeak` stays below the unit's `MemoryMax=3G` (ADR-0075
section 3).

Fail: the unit restarts every few seconds and the journal holds `garden: embedder: <helper>:
<reason>`. With the embedder enabled a helper that does not get ready stops the start (ADR-0047
section 2), so the garden is down, not lexical, until the cause is fixed or the embedder is
turned off; a weight file the helper finds corrupted instead starts the daemon lexical-only with
`"dense":"down"` (ADR-0087 section 8). A `MemoryMax` kill shows as `oom-kill` in `systemctl --user
status ulei-garden`.

Roll back: with the files kept above, put both back and restart; without them, set `enabled =
false` (or remove `[garden.embedder]`) and restart:

```sh
cp -p <DATA_HOME>/ulei/garden/ulei-garden-embed.before ~/.local/bin/ulei-garden-embed
cp -p <DATA_HOME>/ulei/garden/ulei.toml.before <CONFIG_HOME>/ulei/ulei.toml
systemctl --user restart ulei-garden.service
c7 health '{}'
```

Expected: `health` answers `"dense":"ready"` with the earlier helper, or `"dense":"off"` with the
embedder off. The worker deleted the earlier model's vectors when the model changed, so with the
earlier helper it embeds every stored fact again; the facts themselves are untouched.

## 6. Restart the engine

The engine reads `garden.socket`, `garden.core_file` and `garden.core_char_limit` when it starts
(ADR-0077); restart it after the garden serves, so it starts with the socket in place. With the
engine's user unit:

```sh
systemctl --user restart ulei-engine.service
```

with its system unit (case (b)), as root: `systemctl restart ulei-engine.service`. Then, as
`<ULEI_USER>`:

```sh
~/.local/bin/ulei status; echo "exit $?"
```

Expected: `exit 0` with the session `idle`. In case (b), with `pid` the engine's main pid as in
`mvp-pilot-cutover.md` 6.4, as root `nsenter -t "$pid" -m -- ls -l <RUNTIME_DIR>/ulei/garden.sock
<STATE_HOME>/ulei/garden/core.md` lists both: the engine's unit sees the socket and the core
profile.

Roll back: nothing beyond step 4's roll back and a restart of the engine.

## 7. Check that a turn sees the garden

`<ENGINE_JOURNAL>` is `engine.journal` of `ulei.toml`; `<ENTITY>` is the name of an entity of a
stored fact, `<PINNED>` the text of a pinned one (none pinned: the core profile is empty and has
no block). The checks read; the two owner messages are ordinary turns of the owner's session.

```sh
cat <STATE_HOME>/ulei/garden/core.md
c7 pointers '{"q":"What do you know about <ENTITY>?","private":true}'
```

Expected: the core profile lists `<entity>: <fact>` entries joined by `§` lines, `<PINNED>` among
them; `pointers` answers at least one pointer whose `hint` names `<ENTITY>` and holds no fact
text. No pointer: the gate of `garden.api.pointer_min_cosine` and `pointer_min_lexical` is above
this store's scores (`garden-gold.md`); the engine then gets none either.

Send the owner's main channel one message, then the next:

1. `What do you know about <ENTITY>? First quote the garden pointers you got with this message.`
2. `Use garden_search for <ENTITY> and quote its answer.`

Then:

```sh
sqlite3 <ENGINE_JOURNAL> "SELECT substr(system_prompt, instr(system_prompt, 'GARDEN CORE PROFILE')) FROM sessions ORDER BY id DESC LIMIT 1"
sqlite3 <ENGINE_JOURNAL> "SELECT message FROM messages WHERE message LIKE '%garden_search%' ORDER BY rowid DESC LIMIT 2"
journalctl --user -u ulei-engine --since '-10 min' --no-pager | grep -c 'ulei-engine: garden'
```

Expected: the reply to the first message quotes `- there is a fact about <ENTITY> (...)` (the
pointers are sent with the turn and never journaled, so only the reply shows them); the first
query prints the block `GARDEN CORE PROFILE (pinned facts about the owner, read-only) [...]`
with `<PINNED>`; the second shows the model's `garden_search` call and its result with `facts`,
each with its `ref`, `source_ref` and `valid_from`; the count is `0`. In case (b) use `journalctl
-u ulei-engine` as root.

Fail: a line `ulei-engine: garden pointers: garden timed out; the turn runs without them` on
every owner turn: the garden answers `pointers` slower than 150 ms (compare the `pointers` p50 of
the `ulei-garden` budgets; with the embedder on, the helper's first query after a start is
slower). `garden socket: ...`: the engine does not reach the socket (step 6). `ulei-engine: garden
core profile: ...`: the engine cannot read `garden.core_file`. No `garden_search` in the tool
calls: `garden.socket` is unset for the engine.

Roll back: the engine has no switch of its own for the garden. Stop the garden (step 4's roll
back): every owner turn then runs without pointers with one stderr line, and the two tools answer
that the garden is not reachable. The core profile block stays while `core.md` exists; to drop
it, remove `core_file` from `[garden]`, delete the file and restart the garden and the engine.
