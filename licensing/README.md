# Optional licensing code

Pierdesk is free software (GPL-3.0-only). The default build, `cargo build`, is the free community
edition: it has no license gate, never contacts a license server, and has no activation UI.

The code in this directory is kept behind the Cargo feature `licensing`, which is **off by default**.
It exists so that a future official build can be produced from the same source tree:

- `protocol/` — the license wire format and Ed25519 signature checks shared by client and server.
- `server/` — a small reference license server built on that protocol.
- `test-public-key.txt` — the public verification key used by test builds. It is a public key only.

To build with the gate enabled, pass the feature and the server address at compile time:

```powershell
$env:PIERDESK_LICENSE_ENDPOINT = 'https://license.example.com'
cargo build --features licensing
```

Without `PIERDESK_LICENSE_ENDPOINT` the feature build falls back to `https://license.invalid`, which can never
resolve, so such a build cannot reach any server.

Private keys, deployment files and account/payment services are not part of this repository.
