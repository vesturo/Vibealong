# Phase 0 Status

Date: 2026-03-06

## Completed

- Authored formal core behavior contract:
  - `docs/bridge-core-spec-v1.md`
- Added clean-room implementation policy:
  - `docs/clean-room-constraints.md`
- Created Rust workspace + core crate:
  - `Cargo.toml`
  - `packages/core-rs/*`
- Implemented clean-room core modules:
  - OSC parser: `packages/core-rs/src/osc.rs`
  - Mapping/composite: `packages/core-rs/src/mapping.rs`
  - Smoothing/emit gating: `packages/core-rs/src/smoothing.rs`
  - Minimal engine state flow: `packages/core-rs/src/engine.rs`
- Implemented config normalization parity:
  - Deep-merge defaults + user config
  - Validation and clamps matching JS bridge script behavior
  - CLI debug/discovery override precedence
  - Module: `packages/core-rs/src/config.rs`
- Added app-managed settings foundation (no manual JSON requirement):
  - Typed in-app config normalization path (`normalize_in_app_config`)
  - Secure profile store module (`packages/core-rs/src/app_store.rs`)
  - SQLite for non-secret settings + pluggable secret store for stream key
- Added relay client parity module (`packages/core-rs/src/relay.rs`):
  - session auth + token refresh
  - ingest status handling
  - clock skew re-sync on timestamp rejection
- Added bridge runtime module (`packages/core-rs/src/runtime.rs`):
  - UDP OSC ingest
  - packet forwarding to local targets
  - mapping/smoothing tick loop
  - relay event publishing
- Added app-facing control service (`packages/core-rs/src/service.rs`):
  - profile CRUD integration
  - runtime start/stop by profile
  - runtime status snapshot API
- Added runnable no-JSON MVP CLI:
  - `packages/core-rs/src/bin/vrl_bridge_mvp.rs`
  - profile create/list/show/delete/set-key
  - run profile runtime loop with live status
- Added parity-focused unit tests in each module.

## Verification status

Tests executed successfully:

```bash
cargo test -p vrl-osc-core
```

Current result: all tests passing.

## Parity sprint additions (2026-03-06)

- Added runtime observability in `packages/core-rs/src/runtime.rs`:
  - log history ring buffer
  - avatar parameter cache
  - extended runtime counters (mapped/unmapped/discovery/source host)
  - discovery file writing wired in runtime
- Added app service APIs for logs + avatar values:
  - `runtime_logs(max_lines)`
  - `runtime_avatar_params()`
- Added VRChat diagnostics module:
  - `packages/core-rs/src/diagnostics.rs`
  - registry checks + latest log scanning (OSCQuery port + startup failure)
- Added OSCQuery module:
  - `packages/core-rs/src/oscquery.rs`
  - mDNS discovery + log-port fallback + bulk value fetch
- Added Intiface integration baseline:
  - `packages/core-rs/src/intiface.rs`
  - probe server/device list + scalar test command
  - direct bridge runtime loop (engage/disengage + live intensity push)
  - rule-based routing (device/actuator filters + source + shaping params)
- Rebuilt desktop UI into parity-focused tabs:
  - Setup Wizard, Home, Profiles, Logs, Avatar Debugger, Diagnostics, Intiface
  - file: `packages/core-rs/src/bin/vrl_bridge_desktop.rs`
- Added desktop single-instance process guard:
  - `single-instance` based startup check in desktop binary
- Added background close behavior in desktop UI:
  - close request can minimize to background instead of exiting

## Next implementation steps

1. Wire `AppBridgeService` into desktop IPC layer (Tauri commands or equivalent).
2. Build MVP UI screens (Profiles, Inputs, Outputs, Logs, Home status).
3. Add deterministic replay fixtures for full JS parity confidence.
4. Add startup migrations from legacy JSON config import flow.
