# vrl-osc-core

Clean-room Rust core for Vibealong OSC bridge behavior.

## Current scope

- OSC packet parsing (message + bundle)
- Supported arg types: `i`, `f`, `T`, `F`, `s`
- Numeric extraction rules
- Input mapping and weighted composite intensity
- Smoothing (attack/release + EMA + peak envelope)
- Emit gating (`minDelta`, `heartbeatMs`)
- Minimal engine state update flow
- In-app config normalization (typed model, no manual JSON required)
- Secure app profile store:
  - Non-secret settings in SQLite
  - Stream key in pluggable secret store (keyring/in-memory)
- Relay client parity:
  - session auth, token refresh, ingest status handling, clock skew re-sync
- Runtime loop:
  - OSC UDP receive, forwarding, smoothing tick, relay publish
- App control service:
  - profile CRUD, runtime start/stop, runtime snapshots

## Behavior source of truth

- [`docs/bridge-core-spec-v1.md`](../../docs/bridge-core-spec-v1.md)
- Existing script implementation: `scripts/vrl-osc-bridge.mjs`
- Backend MVP notes: [`docs/mvp-backend.md`](../../docs/mvp-backend.md)

## Run tests

```bash
cargo test -p vrl-osc-core
```

## MVP CLI (No JSON)

Binary:

```bash
cargo run -p vrl-osc-core --bin vibealong-mvp -- --help
```

Example flow:

```bash
# create profile
cargo run -p vrl-osc-core --bin vibealong-mvp -- profile create --name Default --creator-username Vee --stream-key your_stream_key

# list profiles
cargo run -p vrl-osc-core --bin vibealong-mvp -- profile list

# run bridge by profile id
cargo run -p vrl-osc-core --bin vibealong-mvp -- run <profile_id>
```

## Desktop app

```bash
cargo run -p vrl-osc-core --bin vibealong
```

Release build:

```bash
cargo build -p vrl-osc-core --bin vibealong --release
```

## Bundled Font Licenses

- Third-party font notices: [`THIRD_PARTY_LICENSES.md`](./THIRD_PARTY_LICENSES.md)
