# MVP Backend (No JSON Workflow)

Date: 2026-03-06

## Goal

Provide a runnable, security-focused backend MVP where:

- profiles are managed in app code (not JSON files),
- secrets are not persisted in plain config storage,
- runtime can start/stop from a selected profile,
- OSC -> mapping/smoothing -> secure relay path is operational.

## Implemented modules

- `packages/core-rs/src/app_store.rs`
  - SQLite persistence for non-secret profile settings.
  - Secret storage abstraction for stream keys.
  - Keyring-backed production secret store + in-memory test store.
- `packages/core-rs/src/config.rs`
  - Typed in-app config normalization (`normalize_in_app_config`).
  - Legacy JSON path retained only for migration.
- `packages/core-rs/src/relay.rs`
  - Session and ingest flow parity with token handling, clock skew re-sync, and status-specific failures.
- `packages/core-rs/src/runtime.rs`
  - UDP OSC receive loop, raw forwarding, mapping/smoothing tick loop, relay publish loop.
- `packages/core-rs/src/service.rs`
  - App-facing lifecycle API: profile CRUD, runtime start/stop, runtime snapshots.

## Security posture in MVP

- `stream_key` is never stored in the SQLite profile table.
- `stream_key` is stored via secret backend (`keyring` in production path).
- Runtime validation occurs before startup (`normalize_in_app_config`).
- Relay token/session data is ephemeral runtime state.

## How app layer should use this

1. Open `AppConfigStore` with a keyring secret store.
2. Create `AppBridgeService` with `DefaultRelayPublisherFactory`.
3. Upsert profiles from UI forms and store stream key through service.
4. Start runtime for active profile with `start_profile(profile_id)`.
5. Display health/status from `runtime_snapshot()`.
6. Stop runtime with `stop_runtime()` on user action/app shutdown.

## Runnable today

A no-JSON CLI MVP is available:

- Binary source: `packages/core-rs/src/bin/vrl_bridge_mvp.rs`
- Help:
  - `cargo run -p vrl-osc-core --bin vrl_bridge_mvp -- --help`
- Profile create/list/run commands are implemented on top of `AppBridgeService`.

## Remaining for desktop MVP

1. Tauri/Electron shell (or chosen desktop host) command wiring.
2. UI screens:
   - profile editor,
   - mapping editor,
   - runtime status/logs,
   - start/stop controls.
3. Relay/intensity diagnostics panel and user-friendly error copy.
4. Installer/update channel and crash-safe startup handling.
