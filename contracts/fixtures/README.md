# Fixtures

Synthetic data only. The C1 packets are signed with the public test key
`000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f` (hex), so an
implementation can verify the HMAC-SHA256 over the canonical JSON (sorted keys, separators
`,` and `:`, UTF-8, no ASCII escaping) of the packet without `mac`.
`c1/golden/` pins canonical JSON, digest, HMAC and the `events.sqlite3` rows and schema to the
Python inbox byte for byte.
