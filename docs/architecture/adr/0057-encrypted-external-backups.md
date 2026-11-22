# ADR-0057: Encrypted external backups: age with one data key wrapped per recovery method, the `ulei-backup` binary, restore by SSH key or password, the device path through age plugins, and their cards

Status: accepted
Date: 2026-11-22
Affects: crates/backup, bin:ulei-backup, dep:age, dep:tar, dep:rusqlite, dep:rustix, dep:ulei-contracts, contract:C9, deny.toml, ci/release.sh, deploy/install.sh, ci/install_run.py, ci/budgets.sh, docs/runbooks

## Context

Encryption of external backups is sprint item d11 (issue:95ead43d2fe53b5d774d). The issue carries
two decisions of the owner of 2026-11-21, quoted by their time:

- 13:25: the archive and the data on the server are not encrypted; only external backups are.
  After a wipe of the server the user restores everything by themself, with an SSH key, with
  biometrics (a passkey or the device) or with a password, ideally several methods at once. The
  candidate is age (ssh-ed25519 recipients, plugins for FIDO2 and the Secure Enclave), with one
  limit: age's password (scrypt) recipient does not mix with other recipients in one file, which
  one data key encrypted separately under each method works around. This record checks that.
- 13:26 to 13:28: we hold no spare key by default; a recovery key held by us and paid backups come
  after the MVP. For the MVP backups are encrypted with the user's keys only.

Its scope items: (1) this record: the format, the data key and its wrap per method, adding or
rotating a method without re-encrypting data, what lies next to the backup (only ciphertext and
the key wraps); (2) the implementation: encrypt a snapshot of ulei data (archive, garden,
journal, the config with no secret in clear) into files for an external target and restore on a
clean machine with any one configured method; at least two methods in code, SSH key and
password; passkey/FIDO2 in code or as an age plugin with a described path; (3) a short restore
runbook, checked by a test. Its check: a fixture snapshot is encrypted, the originals deleted,
restore into an empty directory once with the SSH key and once with the password gives the same
bytes; a wrong key or password is refused with no partial write; the backup holds no plaintext
of the fixture. Out of scope: choosing and uploading to external targets (Google Drive,
S3/R2/B2/Hetzner), the backup wizard and reminders, a recovery key held by us, KMS, Shamir
sharing, paid backups.

What exists on main:

- The stores are SQLite files: the engine's journal `engine.journal` (C8, ADR-0014), rollback
  journal mode with a 10 s busy timeout (`crates/engine/src/journal.rs`); the garden store
  `garden.store` (ADR-0044), WAL mode; the archive (ADR-0051, being built), WAL mode, its path
  key arriving with the `[memory]` keys of C9 (ADR-0051 card 6). All open through `rusqlite`
  0.43.0 with bundled SQLite 3.54.1.
- One `ulei.toml` checked against C9 0.5.0 by `ulei_contracts::config` (ADR-0040, ADR-0041).
  Secrets live in files the config names, never in it: `telegram.token_file`,
  `telegram.stt_env_file`, `engine.web_env_file`, `engine.mcp.*.env_file`,
  `garden.producer_key_file`.
- The release is static musl binaries with one `SHA256SUMS` (`ci/release.sh`), installed by
  `deploy/install.sh` (ADR-0042 section 10). `ulei` has a 512 KiB budget, `ulei-engine`
  4 MiB (ADR-0053), the daemons 2.5 MiB (`ci/budgets.sh`).
- No crate of the workspace encrypts data. `deny.toml` bans `openssl-sys` among others and
  denies advisories by default.

Reference: none. The port map has no entry for backups; nothing here ports agent behaviour.

## Decision

### 1. Places and names

| Part | Where | Traces to |
|---|---|---|
| Crate | `crates/backup`, package `ulei-backup`, class 2, depends on `ulei-contracts` only among workspace crates | scope 2; ADR-0001 section 3 |
| Binary | `ulei-backup`, a separate static musl release file; no other binary links the crypto | scope 2; the size of `ulei` and `ulei-engine` |
| Commands | `init`, `method add`, `method remove`, `list`, `create`, `restore` (sections 3, 6, 7) | scope 1, 2 |
| Config | `[backup]` of C9 next minor: `backup.dir` (the local backup set, section 4) and `backup.recipient` (the data key's public recipient, section 3) | scope 2 |
| Secret files of the config | C9 next minor marks each key whose value names a secret file with `"x-secret-file": true`; the generator writes them into `c9::SECRET_FILES` | scope 2 ("the config with no secret in clear") |
| Runbook | `docs/runbooks/backup-restore.md`, written by card 4 | scope 3 |

Rejected:
- Commands inside `ulei` or a daemon: the crypto and SQLite would land in a binary with a 512 KiB
  budget or in a resident process that never needs them.
- A binary of `ulei-core`: every build of that package compiles the crypto crates; a crate of
  its own keeps them out of the core's manifest.

### 2. Format: age v1 through the `age` crate

Every file `ulei-backup` writes for a target is an age v1 file (C2SP age specification), written
and read with the `age` crate 0.12.1, `default-features = false, features = ["ssh"]`.

Reasons: the format is specified and stable; it has native password (scrypt) and X25519
recipients and SSH recipients (ssh-ed25519, ssh-rsa) in the same crate; each payload is
authenticated chunk by chunk with a marked final chunk (section 3, wrong key); the files open with
the stock `age` or `rage` tools, so a restore does not depend on `ulei-backup` (section 7); the
crate is MIT OR Apache-2.0, pure Rust, links no OpenSSL, and built and round-tripped in the
throwaway measurement of section 9.

Rejected:
- OpenPGP: `sequoia-openpgp` 2.4.2 is LGPL-2.0-or-later, which `deny.toml` does not allow; `pgp`
  0.21.1 (MIT OR Apache-2.0) mixes password and key recipients in one message but has no SSH key
  recipient and is a larger format to carry.
- An own format over RustCrypto primitives (ChaCha20-Poly1305, Argon2, X25519): an invented
  format with no stock tool to restore by hand, and its review would be ours.
- Shelling out to the `rage` binary: a further release file, passwords through a child's prompt,
  and the same crate underneath.
- The `plugin` feature of the crate: it adds 132,296 B (section 9) and a plugin host in a binary
  that runs on a server with no authenticator; section 5 reaches devices without it.

### 3. One data key, one wrap per method

- The data key is one age X25519 identity, generated in memory by `ulei-backup init`. Its
  recipient (`age1...`) goes into `backup.recipient`; the identity itself is never written in
  clear to any disk by `ulei-backup`.
- Each snapshot is an age file encrypted to `backup.recipient` alone. The age format gives each
  file its own random 16-byte file key, wrapped to that recipient; the payload key is
  HKDF-SHA-256 of the file key and a per-file nonce. The server needs only the public recipient to
  write new snapshots, so a new snapshot never asks for any method.
- A wrap is one age file `keys/<label>.age` whose plaintext is the data identity in the age
  identity file format (one line), so stock `age -d -i <unwrapped file>` reads it. A label is
  `^[a-z][a-z0-9-]{0,31}$`. Each wrap file is encrypted to exactly one method:
  - `ssh`: one ssh-ed25519 or ssh-rsa public key;
  - `password`: one scrypt stanza, work factor log2 N = 18, written with the crate's
    `set_work_factor(18)`; restore accepts up to 22 with `set_max_work_factor(22)`;
  - `recipient`: one native age recipient (`age1` X25519, `age1tag1`, `age1tagpq1`), the
    device path of section 5.
- Because every wrap holds one recipient, the scrypt rule (an scrypt stanza must be the only
  stanza of a header) never meets another stanza, and every method has its own file to add or
  remove.
- `init --label <l> (--ssh <public key file> | --password | --recipient <r>) [...]` takes one or
  more methods, writes their wraps and prints the `backup.recipient` line for the owner to put
  into `ulei.toml`. It refuses a `backup.dir` whose `keys/` already holds a wrap. With one method
  it prints one line on stderr that the set has a single method.
- `method add --label <l> (--ssh <key> | --password | --recipient <r>) --unlock (--ssh-key
  <private key file> | --password | --identity <file>)` unwraps the data identity with an existing
  method, checks that its recipient equals `backup.recipient`, writes the new wrap to
  `keys/<l>.age.part`, syncs it and renames it. No snapshot is read or written: snapshots made
  before the new method open with it.
- `method remove --label <l>` deletes the wrap file and refuses to delete the last one.
  Rotating a method (a new password, a new SSH key) is `method add` then `method remove`, with no
  re-encryption.
- `list` prints each wrap's label and the stanza type of its header (`ssh-ed25519`, `ssh-rsa`,
  `scrypt`, `X25519`, ...), and each snapshot's name and size. It needs no method.
- A wrong key or password: an SSH key that matches no `ssh-*` stanza, or a password whose scrypt
  key fails the stanza's authentication, leaves age with no file key and `decrypt` returns an
  error. `ulei-backup` tries every wrap of the method's type, then exits 1 with one stderr line
  naming the method's type and no path of the backup's content, and writes nothing.
- The fixed work factor: the crate's default picks about one second on the wrapping host and
  accepts up to that plus 4 on the restoring host (`src/native/scrypt.rs`), so a wrap made on a
  fast server could be refused on a slower laptop. 18 and 22 are the Go reference's defaults
  (`scrypt.go`), so the stock tools open the wrap too. The scrypt step uses r = 8
  (`src/primitives.rs`), 128 · 8 · 2^18 B = 256 MiB of memory during a password unwrap.

Rejected:
- One multi-recipient age file per snapshot: the password method cannot join it, so the password
  would need a second copy of every snapshot, and adding a method would re-encrypt every snapshot.
- A raw symmetric data key with our own wrap format: an invented format again, and no stock-tool
  restore.
- One wrap file for all non-password methods: adding or removing one method rewrites the file
  that the others depend on.
- A fresh data key per snapshot, wrapped per method: every new snapshot would need every method,
  including the password and the user's devices, at backup time on the server.
- The crate's default scrypt work factor: wraps made on one host could be refused on another.

### 4. What lies next to the backup

`backup.dir` holds exactly the set that goes to an external target:

```
keys/<label>.age                  one wrap per method (section 3)
snapshots/ulei-<YYYYMMDDTHHMMSSZ>.age   one snapshot per run of `create` (UTC time)
```

Nothing else: no manifest, no recipient file, no plaintext index. The names show the method
labels and the snapshot times. An age header in clear shows each stanza's type and, for SSH, a
4-byte tag (the first bytes of the SHA-256 of the public key, `src/ssh.rs`); it shows no key,
password or content. In-flight files are `*.part` in the same directory and are never uploaded
(the target card's business, out of scope here). The directory is created with mode 0700 and the
files with 0600.

Rejected:
- A plaintext manifest of methods and contents: metadata in clear that restore does not need,
  since `list` reads the headers.
- Keeping `backup.recipient` in the set: the server needs it, the target does not; it travels
  inside each snapshot's `ulei.toml`.

### 5. Passkey, FIDO2 and the device: an age-plugin path, not code

The device methods use age plugins on the user's own device. `ulei-backup` contains no FIDO2,
CTAP, PC/SC or Secure Enclave code and no plugin host. Two paths:

- A plugin whose recipient is a native age recipient. `age-plugin-fido2-hmac` 0.5.0 with the
  credential kept in a separate identity yields a native `age1...` recipient; `age-plugin-se`
  0.2.1 yields `age1tag1...` recipients, native since age v1.3.0 and in the crate since 0.12.0
  (`age::tag::Recipient`, encryption only). The owner generates the recipient on the device, and
  on the server runs `ulei-backup method add --label <l> --recipient <r> --unlock ...`; no
  authenticator is needed to wrap. Restore on the device: `age -d -i <plugin identity file>
  keys/<l>.age > <data key file>` with the plugin installed and the authenticator touched, then
  `ulei-backup restore --identity <data key file>` or the stock path of section 7.
- A plugin whose recipient needs the plugin or the authenticator to encrypt
  (`age-plugin-yubikey` 0.5.1 recipients `age1yubikey1...`; `age-plugin-fido2prf`, whose
  symmetric PRF secret needs the passkey at every encryption). The owner makes the wrap on the
  device: unwraps the data identity with an existing method using the stock tool
  (`age -d keys/<existing>.age`, a password or SSH key), encrypts it with
  `age -e -r <plugin recipient> -o <l>.age`, deletes the clear copy, and copies `<l>.age` into
  `keys/` on the server. `method add` refuses a plugin recipient with one line naming the runbook.

Reasons: the server where wraps are made usually has no authenticator, and a device is only
needed when a wrap is made and at restore, never per snapshot (section 3); FIDO2 in code would
bring a C library (libfido2) or a USB HID stack into a static musl binary for a path the server
does not use; the plugins are maintained separately and open with the stock tools on the device.
The runbook of card 4 describes both paths.

Rejected:
- FIDO2 hmac-secret or WebAuthn PRF in `ulei-backup`: the C library or USB stack above, and a
  stanza format of our own.
- The crate's `plugin` feature in `ulei-backup`: 132,296 B and a plugin host for a server that has
  no authenticator; the device runs the stock tool with the plugin.
- No device method in the MVP: 13:25 names biometrics among the methods.

Not provided: an SSH key held by `ssh-agent` (rage does not support it) and FIDO2-backed SSH keys
(`sk-ssh-ed25519@openssh.com`, refused by the crate as a hardware key); a user with such a key
uses a device or password method.

### 6. The snapshot

`ulei-backup create` writes one snapshot. Its plaintext is one tar stream (the `tar` crate 0.4.46,
which the stock `tar` reads):

| Entry | Content |
|---|---|
| `ulei.toml` | the config file `ulei_contracts::config` found, byte for byte |
| `stores/engine.journal`, `stores/garden.store`, `stores/memory.archive` | a consistent copy of each store whose key the config sets |
| `secrets/<key>` | each file named by a key in `c9::SECRET_FILES` that the config sets, `*` replaced by the item name (`secrets/engine.mcp.<name>.env_file`) |

- A store or secret key that the config sets and whose file is missing or unreadable fails the
  run with one stderr line naming the key; no snapshot file is left. A key the config does not set
  is not in the snapshot (the archive before its card lands).
- Consistency: each store is copied with `VACUUM INTO '<private temporary file>'` on a read-only
  connection of the locked `rusqlite`. SQLite: "The VACUUM INTO command is transactional in the
  sense that the generated output database is a consistent snapshot of the original database"
  (sqlite.org/lang_vacuum.html). The garden and the archive are in WAL mode, where "readers do not
  block writers and a writer does not block readers" (sqlite.org/wal.html). The journal is in
  rollback mode: while the copy holds its SHARED lock, an engine commit "might have to wait until
  those SHARED locks clear" (sqlite.org/lockingv3.html), up to the engine's 10 s busy timeout. The
  budget of card 5 bounds that window.
- Each store is its own point in time; the three are not one instant. The archive pulls the
  journal by cursor and idempotently (ADR-0051 section 3), and the garden refers to sources by
  ref, so a restored set converges after the daemons start.
- The temporary copies live in `backup.dir/.tmp-<pid>/` (mode 0700) and are deleted after the
  run, on success or failure. The tar stream is encrypted as it is written to
  `snapshots/<name>.age.part`, which is synced and renamed.
- Secrets are inside the encrypted payload only; no secret, config line or store byte is written
  in clear to `backup.dir`.

Rejected:
- Copying the store files with a file copy: a torn copy under a running writer.
- The SQLite online backup API: it restarts when another process writes during a stepped copy,
  and a copy in one step holds the same lock as `VACUUM INTO`; it also needs the crate's `backup`
  feature.
- Secrets left out of the snapshot: the restore would need every token reissued, against 13:25
  ("restores everything by themself").
- One age file per store: more files to upload and match, no gain in confidentiality.
- Compression: not in this record; the stores are copied as pages.

### 7. Restore on a clean machine

1. Install the release (`deploy/install.sh`) and copy the backup set (one snapshot and one or more
   `keys/*.age`) from the target to any directory.
2. `ulei-backup restore --from <set directory or snapshot file> --into <empty or absent directory>
   (--ssh-key <private key file> | --password | --identity <age identity file>)`. Without a
   snapshot file it takes the newest `snapshots/*.age`. `--identity` takes a native age identity
   file, such as a data key unwrapped on a device (section 5). It needs no `ulei.toml`.
3. It unwraps the data identity (section 3), then decrypts the snapshot as a stream into
   `<into>/.restore-<pid>/` and unpacks the tar there. Only after the age stream has authenticated
   its final chunk ("Streaming decryption MUST signal an error if it reaches end of file without
   successfully decrypting a final chunk", C2SP age) and the tar has ended does it move the entries
   into `<into>`; on any error it deletes the staging directory and exits 1 with one stderr line,
   leaving `<into>` as it was. Entries with absolute paths or `..` are refused.
4. The result is the layout of section 6: `ulei.toml`, `stores/`, `secrets/`, files 0600. The
   runbook places each file at the path its key names in `ulei.toml` (or edits the paths), then
   starts the units.

The same restore with the stock tools, for a machine without the release:
`age -d -i <ssh private key> keys/<l>.age > <data key file>` (or `age -d` with a password), then
`age -d -i <data key file> snapshots/<name>.age | tar -x -C <dir>`.

Rejected:
- Restore straight into the paths of `ulei.toml`: it would overwrite whatever is there and spread
  a partial write across the disk on failure.
- Decrypting into the final directory as the stream goes: a tampered or truncated snapshot is
  detected only at its bad chunk, after earlier entries were written.

### 8. Budgets

A new `ulei-backup` section in `ci/budgets.sh`, driven by a stdlib `ci/backup_run.py` on the
runner disk with a synthetic `ulei.toml`, a 256 MiB synthetic journal, small garden and archive
stores and fixture keys generated at run time:

- static musl binary: 2,621,440 B (the daemons' limit);
- peak RSS (VmHWM) of `create`: 16,384 kB;
- peak RSS of `restore --ssh-key`: 16,384 kB; of `restore --password`: 300,000 kB (the scrypt
  step of section 3 needs 256 MiB);
- the SHARED lock window of `VACUUM INTO` on the 256 MiB journal: 5,000,000 us, half the engine's
  busy timeout.

Expected size: about 2.0 MB static (section 9).

### 9. External facts checked

Checked on 2026-11-22.

| Fact | Source |
|---|---|
| `age` 0.12.1 and `rage` 0.12.1, released 2026-07-14, MIT OR Apache-2.0, MSRV 1.74; age 0.12.0 (2026-07-13) added the native `tag` and `tagpq` recipients, encryption only; the rage repository was last pushed 2026-08-20 | crates.io/api/v1/crates/age, crates.io/api/v1/crates/rage, the crate's `CHANGELOG.md`, api.github.com/repos/str4d/rage |
| Features of age 0.12.1: `ssh` pulls `aes`, `aes-gcm`, `bcrypt-pbkdf`, `cbc`, `ctr`, `curve25519-dalek`, `num-traits`, `rsa` ^0.9; `plugin` pulls `which`, `wsl`; always: `i18n-embed` 0.16, `i18n-embed-fl`, `rust-embed`, `p256` 0.13, `hpke` 0.12, `ml-kem` 0.2, `scrypt` 0.11, `x25519-dalek` 2 | crates.io/api/v1/crates/age/0.12.1/dependencies |
| age supports ssh-ed25519 and ssh-rsa recipients, not `ssh-agent`; encrypted OpenSSH keys (`aes256-ctr`, `aes256-cbc`, `aes256-gcm@openssh.com`) are read; hardware (`sk-`) SSH keys are refused | github.com/str4d/rage README; the crate's `src/ssh.rs`, `src/ssh/identity.rs` |
| "An scrypt stanza, if present, MUST be the only stanza in the header", and identities "MUST reject headers where an scrypt stanza is present alongside any other stanza"; the crate's `Encryptor::with_recipients` returns `EncryptError::MixedRecipientAndPassphrase` for such a mix | github.com/C2SP/C2SP `age.md` (editor's copy, read 2026-11-22); the crate's `src/protocol.rs` |
| A file key is 16 bytes per file; payload key HKDF-SHA-256(file key, nonce, "payload"); 64 KiB ChaCha20-Poly1305 chunks with a final-chunk flag | C2SP `age.md` |
| The crate's scrypt default targets about 1 s on the encrypting host and accepts the target plus 4 when decrypting; `set_work_factor`, `set_max_work_factor`; scrypt r = 8, p = 1 | the crate's `src/native/scrypt.rs`, `src/primitives.rs` |
| Go age: scrypt recipient work factor 18 ("1s on a modern machine"), identity maximum 22; latest release v1.3.2 of 2026-08-29 | raw.githubusercontent.com/FiloSottile/age/main/scrypt.go, api.github.com/repos/FiloSottile/age/releases/latest |
| `RecipientPluginV1::new` and `IdentityPluginV1::new` find `age-plugin-<name>` on `$PATH` (`which`) and return `ResolveError` when it is missing | the crate's `src/plugin.rs` |
| `age-plugin-fido2-hmac` v0.5.0 (2026-05-11), Go, MIT, needs libfido2, marked experimental until 1.0; with a separate identity it gives a native age recipient; decryption loads the identity into memory | github.com/olastor/age-plugin-fido2-hmac README, api.github.com releases |
| `age-plugin-yubikey` v0.5.1 (2026-04-08), Rust, repository license reported as Apache-2.0 | api.github.com/repos/str4d/age-plugin-yubikey |
| `age-plugin-se` v0.2.1 (2025-12-27), Swift, MIT; keys bound to one Mac (macOS 14), Touch ID or passcode; encrypting works on Linux; `age1tag1` recipients need no plugin with age v1.3.0 or later; its docs advise a backup key such as a password-protected age key | github.com/remko/age-plugin-se README |
| `age-plugin-fido2prf` (typage repository, Go): passkey and FIDO2 PRF, symmetric, so the credential is needed at encryption; stanza `age-encryption.org/fido2prf`; article of 2025-07-14 | words.filippo.io/passkey-encryption |
| `rsa` 0.9.10 is affected by RUSTSEC-2023-0071 (Marvin timing attack, 2023-11-22), patched in `>= 0.10.0` (released 2026-11-08); rage's main branch moves age to `rsa` 0.10.0-rc.18, not released | rustsec.org/advisories/RUSTSEC-2023-0071, crates.io/api/v1/crates/rsa, raw.githubusercontent.com/str4d/rage/main/age/Cargo.toml |
| `crypto-bigint` 0.5.5, through `p256` 0.13 and `elliptic-curve` 0.13 of age 0.12, is affected by RUSTSEC-2026-0387 (timing variability, 2026-10-23), patched in `>= 0.7.6` | github.com/rustsec/advisory-db `crates/crypto-bigint/RUSTSEC-2026-0387.md`; `cargo tree -i` of the throwaway build |
| `basic-toml` 0.1.10, a build-time dependency of `i18n-config` 0.4.8 under age's `i18n-embed-fl`, is marked unmaintained by RUSTSEC-2026-0374 (2026-09-26), no patched version | advisory-db `crates/basic-toml/RUSTSEC-2026-0374.md`; crates.io/api/v1/crates/i18n-config/0.4.8/dependencies |
| Licenses of the throwaway graph (164 crates): each is or offers one of `deny.toml`'s allowed licenses; none is on its ban list; no `openssl-sys`, `ring` or `tokio` | `cargo metadata` and `cargo tree` of the throwaway build |
| `tar` 0.4.46 (2026-05-18), MIT OR Apache-2.0, MSRV 1.63; RUSTSEC-2026-0067 and -0068 are patched in `>= 0.4.45` | crates.io/api/v1/crates/tar, advisory-db |
| `sequoia-openpgp` 2.4.2 (2026-10-14) is LGPL-2.0-or-later; `pgp` 0.21.1 (2026-11-11) is MIT OR Apache-2.0 | crates.io/api/v1/crates/sequoia-openpgp, crates.io/api/v1/crates/pgp |
| VACUUM INTO gives a consistent snapshot and is an alternative to the backup API; the target file must not exist or be empty (page of 2025-07-12); WAL readers and writers do not block each other (page of 2026-08-25); in rollback mode a writer waits for SHARED locks to clear (page of 2025-05-31) | sqlite.org/lang_vacuum.html, sqlite.org/wal.html, sqlite.org/lockingv3.html |
| `rusqlite` 0.43.0 has a `backup` feature with no further dependency | `Cargo.toml` of the locked crate |
| Measured: throwaway programs with the workspace's release profile (`opt-level = "s"`, fat LTO, `panic = "abort"`, strip), target `x86_64-unknown-linux-gnu`, rustc 1.100.0. A baseline reading stdin and a file: 293,328 B. With age 0.12.1 (`ssh`) and tar 0.4.46, encrypting and decrypting with ssh, scrypt and X25519 and packing and unpacking tar: 929,824 B (+636,496 B). Adding the `plugin` feature: 1,062,120 B (+132,296 B). Adding `rusqlite` 0.43.0 bundled with the repository's `LIBSQLITE3_FLAGS` and a `VACUUM INTO` on a read-only connection: 1,918,648 B. Round trips with an ed25519 key, a 3072-bit RSA key (both made by `ssh-keygen` at run time) and a password matched byte for byte; the age file held no byte of a marker string the tar held | throwaway builds, not committed |

`ulei-backup` is expected at about 2.0 MB static: the measured 1.79 MB without the plugin feature,
the C9 decoder and `rustix`'s termios calls, and musl's static libc in place of the dynamic glibc
of the measurement.

### 10. Dependencies and `cargo deny`

- `crates/backup`: `ulei-contracts` (path), `age = { version = "=0.12.1", default-features =
  false, features = ["ssh"] }`, `tar = { version = "0.4.46", default-features = false }`,
  `rusqlite` (workspace), `rustix` (workspace, plus the `termios` feature for the password prompt
  with echo off; stdin that is not a terminal is read as one line).
- `deny.toml` gains three `[advisories] ignore` entries, each with its reason in the file:
  RUSTSEC-2023-0071 (the RSA private key is used once, locally, by the owner's command, with no
  remote party timing it), RUSTSEC-2026-0387 (the affected `p256` code runs only for `tag`
  recipients; its secret is the wrap's ephemeral scalar on the owner's machine), RUSTSEC-2026-0374
  (a build-time TOML reader of the crate's message catalogue, not in the binary). Each entry goes
  when an `age` release depends on a patched version; the card that bumps `age` removes it.
- Duplicate RustCrypto generations (`sha2` 0.10 and 0.11, `hmac` 0.12 and 0.13, `rand_core`) are
  allowed by `multiple-versions = "warn"`.

Rejected:
- `age` 0.11.5 (2026-07-14): free of `p256` and its advisory, but without native `tag`
  recipients, which section 5 uses for the Secure Enclave.
- Patching or forking `age` to drop `rsa`: a crypto fork to maintain for one advisory whose attack
  needs a remote timing oracle.
- `rpassword` for the prompt: a further crate where the locked `rustix` has the calls.

### 11. Implementation cards, in order

Each card merges alone with the workspace green, tests on synthetic data with fixture keys
generated at test time (`ssh-keygen` into a temporary directory, passwords as literals), commits
nothing key-like, and rewrites in place every document that describes what it changes. A card
that adds a crate, a dependency or a contract version carries the short ADR the ADR gate needs,
pointing here. "Next minor" is the next free version when the card lands.

1. C9 `[backup]` and secret files (d11; about 60 min). C9 next minor: `backup.dir`,
   `backup.recipient`; `"x-secret-file": true` on `telegram.token_file`, `telegram.stt_env_file`,
   `engine.web_env_file`, `engine.mcp.*.env_file`, `garden.producer_key_file`; the generator writes
   `c9::SECRET_FILES`; `docs/ulei.example.toml`; CHANGELOG. Crates: `crates/contracts`, `xtask`.
   Check: fixtures and round trip, `cargo xtask contracts`; the decoder accepts a `[backup]`
   section and refuses an unknown key in it.
2. The data key and its methods (d11; about 90 min). Crate `crates/backup`, binary `ulei-backup`
   with `init`, `method add`, `method remove`, `list` (section 3); dependencies `ulei-contracts`,
   `age` (`ssh`), `rustix` (`termios`); the three `deny.toml` ignores of section 10; the module
   map. Check: each of an ed25519 wrap, an RSA wrap, a password wrap and a native-recipient wrap
   unwraps to an identity whose recipient equals `backup.recipient`; the password wrap's header
   is one `scrypt` stanza with work factor 18; `method add` with a valid unlock makes a wrap that
   opens an age file the test encrypted to `backup.recipient` before it, and leaves that file's
   bytes unchanged; `method add` unlocked with a wrong SSH key or a wrong password exits 1 with
   one stderr line and creates no file; `method remove` refuses the last wrap; a plugin recipient is refused;
   `cargo deny check` green.
3. `ulei-backup create` (d11; about 90 min). The snapshot of section 6: `VACUUM INTO` copies,
   the tar stream, the age stream to `backup.recipient`, `.part` and rename, the temporary
   directory removed; dependencies `rusqlite`, `tar`. Check: a snapshot of a fixture while another
   connection holds the garden store in a write transaction completes and opens consistent; a
   store key set to a missing file fails with the key named and leaves no snapshot and no
   temporary directory; the no-plaintext check: marker strings planted in each fixture store,
   `ulei.toml` and each secret file occur in no file under `backup.dir` (a byte search over the
   files, not `grep -I`, which skips binary files).
4. `ulei-backup restore`, the wipe test and the runbook (d11; about 90 min). Restore of section 7
   with `--ssh-key`, `--password`, `--identity`; `docs/runbooks/backup-restore.md`: getting the
   set, the restore command for each method, placing the files, the stock-tool path, and the two
   device paths of section 5. Check: the wipe test, once per method: take a fixture snapshot with
   `create`, keep a copy of the files the snapshot holds, delete the fixture, restore into an empty
   directory once with the SSH key and once with the password, compare every file byte for byte
   with the kept copy; the same with `--identity` given the data key unwrapped by the test, which
   stands for a device unwrap; the wrong-key refusal: a wrong SSH key, a wrong password and a
   snapshot truncated by one chunk each exit 1 and leave the target directory as it was, with no
   staging directory; an entry with `..` is refused.
5. Release and budget (d11; about 60 min). `ulei-backup` in `ci/release.sh` (`BINS`, the build),
   `deploy/install.sh`, `ci/install_run.py`; `ci/backup_run.py` and the `ulei-backup` section of
   `ci/budgets.sh` with the limits of section 8; the measured size reported. Check: the jobs
   `release` and `budgets`.

## Consequences

- External backups need one of the user's methods to open; nobody else holds a key, and a user who
  loses every method loses the backups (13:26 to 13:28).
- The server keeps the data key's public recipient only; a new snapshot needs no method, and
  adding or rotating a method touches one file in `keys/`.
- Removing a method does not revoke wraps already copied to a target; revoking means a new data
  key in a new `backup.dir` with a new `backup.recipient`, and the old snapshots stay readable by
  the old wraps.
- Every file at a target is an age file; the stock `age` or `rage` and `tar` restore without the
  ulei release.
- A password restore needs 256 MiB of memory for scrypt.
- The workspace carries three advisory ignores until `age` releases with `rsa` 0.10 and newer
  `p256` and `i18n-embed` dependencies.
- Not decided here: the schedule of `create`, the number of snapshots kept in `backup.dir`,
  uploading to targets, the wizard and reminders (the issue's out of scope), and owner files the
  config names besides the stores and secrets (prompt files, skills, the inbox root, the web
  store).
