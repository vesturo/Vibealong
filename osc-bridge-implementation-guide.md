# VRLewds OSC Bridge - Standalone EXE Implementation Guide

Last updated: 2026-03-05

## 1) Goal

Build a standalone desktop app (Windows-first EXE) that is as user-friendly as OscGoesBrrr, while preserving VRLewds' secure SPS relay flow.

The app should:
- Read OSC from VRChat (and optionally OSCQuery).
- Let creators map/weight sources in UI (no hand-edit required).
- Forward OSC to local tools (Intiface, other local bridges).
- Relay normalized intensity to VRLewds using secure token/session flow.
- Expose clear runtime status, logs, and diagnostics.

---

## 2) Current Implementation (In This Repo)

### 2.1 Current bridge script

Source: `scripts/vrl-osc-bridge.mjs`

What it currently does:
- Loads JSON config (default + deep merge + CLI overrides).
- Validates:
  - `websiteBaseUrl`, `creatorUsername`, `streamKey`
  - OSC listen host/port
  - mapping list (`inputs`) and output smoothing options
- Listens for UDP OSC packets, parses:
  - single messages and bundles (`#bundle`)
  - supported types: `i`, `f`, `T`, `F`, `s`
- Maps configured OSC addresses to normalized intensity:
  - per-input min/max, deadzone, curve, invert, weight
  - weighted composite target intensity
- Smooths output:
  - attack/release + EMA + heartbeat/minDelta
- Sends secure relay events:
  - session token from `/api/sps/session`
  - ingest events to `/api/sps/ingest`
  - handles clock skew re-sync + token refresh
- Optional raw forwarding to local UDP targets (`forwardTargets`)
- Debug modes:
  - log all OSC, only unmapped, only configured, relay ACKs
- OSC discovery mode:
  - appends unique discovered OSC addresses to file

### 2.2 Server-side contract already in place

Relevant files:
- `apps/web/app/api/sps/session/route.ts`
- `apps/web/app/api/sps/ingest/route.ts`
- `apps/web/lib/sps-relay.ts`
- `apps/web/lib/vibealong.ts`
- `apps/web/app/api/streams/[id]/vibealong/route.ts`

Current behavior:
- `POST /api/sps/session`:
  - verifies creator credentials (`creatorUsername` + `streamKey`)
  - requires creator live stream
  - enforces creator/staff vibealong policy
  - issues short-lived signed relay token
  - marks bridge presence
- `POST /api/sps/ingest`:
  - verifies bearer token
  - validates payload + timestamp skew
  - rate limits + monotonic sequence checks
  - confirms stream still live + policy still allowed
  - broadcasts realtime `sps.intensity`
  - refreshes bridge presence heartbeat
- Viewer availability depends on:
  - live stream + policy + recent bridge presence

### 2.3 Current gap

The bridge works technically, but UX is config-file/CLI-centric.
You need a desktop UI app for mass creator adoption.

---

## 3) OscGoesBrrr Parity Baseline (What They Have)

From upstream code (`OscToys/OscGoesBrrr`) and docs:
- Electron desktop app with tray/window UX.
- GUI status panels (VRChat status, Intiface status).
- Advanced text config editor in-app.
- Avatar parameter/debug views.
- OSC receive + proxy forwarding.
- OSCQuery integration/discovery logic.
- Buttplug/Intiface websocket integration.
- Runtime device scanning and toy feature routing.
- Audio input mode support.
- Packaged Windows installer via electron-builder.

Key upstream files reviewed:
- `src/main/main.ts`
- `src/main/OscConnection.ts`
- `src/main/bridge.ts`
- `src/main/Buttplug.ts`
- `src/frontend/components/*`
- `package.json` build config

---

## 4) Feature Parity Matrix (Current VRL vs Target)

### Already done in VRL bridge
- Secure server-authenticated relay session.
- Signed token-based ingest.
- Intensity smoothing pipeline.
- OSC forward targets.
- Input discovery + debug modes.
- Stream/creator policy enforcement server-side.

### Missing for parity / product quality
- Desktop GUI (no CLI dependency).
- Source picker + mapping editor in UI.
- Live status panes (OSC, relay, Intiface, errors).
- In-app logs viewer + export.
- Presets/profile management per avatar/world.
- Intiface direct integration (optional mode in-app).
- Setup wizard + health checks.
- Auto-update + installer polish.

---

## 5) Recommended Standalone Repo Architecture

Create a new repo, e.g. `vrl-osc-bridge-app`.

## 5.1 Monorepo structure

- `apps/desktop`
  - Electron app shell (main process + renderer)
- `packages/core`
  - Pure TS bridge engine (OSC parse/mapping/smoothing)
- `packages/relay-client`
  - `/api/sps/session` + `/api/sps/ingest` client, retries, skew handling
- `packages/intiface`
  - Buttplug websocket client + command abstraction
- `packages/shared`
  - config schema, types, validators, migration utils

This keeps your core bridge logic testable and reusable without Electron lock-in.

## 5.2 Runtime model

Main process responsibilities:
- Config storage and secrets handling.
- Start/stop bridge engine.
- Own sockets/network clients.
- IPC API to renderer.

Renderer responsibilities:
- Setup wizard / mapping UI.
- Status dashboards.
- Debug/log panel.
- Profile management.

Core engine responsibilities:
- OSC ingest + parse.
- Mapping pipeline.
- Smoothing + emission.
- Forward targets + relay publisher hooks.

---

## 6) Config Model (for UI + persistence)

Start from your existing JSON fields and make them schema-versioned:

- `version`
- `server`:
  - `websiteBaseUrl`
  - `creatorUsername`
  - `streamKey` (secret, encrypted at rest)
- `osc`:
  - listen host/port
  - optional OSCQuery auto-discovery on/off
- `inputs[]`:
  - address, label, enabled, weight, curve, invert, deadzone, min, max
- `output`:
  - emitHz, attackMs, releaseMs, emaAlpha, minDelta, heartbeatMs
- `forwardTargets[]`
- `intiface`:
  - mode (`off` | `proxy-only` | `direct`)
  - host/port, TLS flag
- `debug` + discovery settings

Add migration support (`v1 -> v2`) so config survives future changes.

---

## 7) Security Model (Must Keep)

- Never expose `streamKey` in renderer logs or crash dumps.
- Store secrets encrypted on disk (Windows DPAPI via `safeStorage` or `keytar`).
- Keep relay token short-lived (already server-side).
- Keep monotonic seq + timestamp logic client-side.
- Retry with exponential backoff, but do not spam ingest.
- Clear bridge status if session auth fails repeatedly.
- Provide explicit "Disconnect / Stop relay" action.

---

## 8) Phase Plan to Ship

## Phase 0 - Extract and stabilize core (1-2 days)
- Move logic from `scripts/vrl-osc-bridge.mjs` into `packages/core` + `packages/relay-client`.
- Keep existing behavior identical.
- Add unit tests for parser, mapping, smoothing.

## Phase 1 - Desktop shell MVP (3-5 days)
- Electron app with:
  - Start/Stop button
  - status indicators (OSC, relay session, ingest ack)
  - config form for server + OSC + mappings
  - log window
- Build Windows portable + installer.

## Phase 2 - Parity features (5-10 days)
- Source discovery UX:
  - list recently seen OSC addresses
  - one-click add mapping
- Forward target manager UI.
- Profiles/presets (per-avatar).
- Import/export config.
- Health checks (clock skew, stream online, auth errors).

## Phase 3 - Intiface integration mode (optional but high value)
- Add native Buttplug websocket client in app.
- Allow:
  - pass-through only (current ecosystem compatibility)
  - direct toy control mode
- Device list and live test panel.

---

## 9) API Compatibility Contract (Keep Stable)

Bridge app should continue using:
- `POST /api/sps/session`
- `POST /api/sps/ingest`

Payload compatibility:
- Keep `seq`, `ts`, `intensity`, `peak`, `raw`, `source.address`, `source.argType`.
- Preserve current rate behavior (`emitHz`, minDelta, heartbeat) to avoid server surprises.

---

## 10) Suggested UI Information Architecture

Main tabs:
- `Home`
  - big status cards: OSC, Relay, Stream, Forwarding
  - start/stop + quick actions
- `Inputs`
  - discovered addresses
  - mapping list editor with live values
- `Outputs`
  - smoothing graph + intensity meter
  - forward targets + intiface controls
- `Logs`
  - filter by OSC / Relay / Errors
  - copy/export
- `Settings`
  - account + server URL + update channel + diagnostics

---

## 11) Build & Packaging Recommendation

Stack:
- Electron + Vite/React (or Electron + Webpack if you prefer parity with OGB).
- `electron-builder` for `.exe` + auto-updates later.

Deliverables:
- `VRLewdsBridge-Setup.exe`
- optional portable zip build for power users.

---

## 12) Migration Strategy from Current Script

- Keep `scripts/vrl-osc-bridge.mjs` as fallback for one release cycle.
- In new app, add "Import legacy JSON config".
- If import succeeds:
  - validate and migrate fields
  - show diff/preview before save
- Emit matching logs so support docs remain easy.

---

## 13) Acceptance Criteria for "Parity Achieved"

- New user can install EXE and get first relay event live in <5 minutes.
- No manual file editing required for common setup.
- OSC source can be discovered and mapped from UI.
- Relay auth/ingest failures are understandable in UI.
- Forwarding to local OSC targets works reliably.
- Optional Intiface mode functions without external helper.
- Packaged installer updates cleanly between versions.

---

## 14) Immediate Next Step

Before creating the new repo, do one extraction PR in this repo:
- move bridge logic into reusable TS module (no behavior changes),
- keep script as thin wrapper.

That gives you production-tested core logic you can lift into the new desktop repo with minimal risk.

---

## References

- OscGoesBrrr repository: https://github.com/OscToys/OscGoesBrrr
- OGB docs (getting started): https://osc.toys/docs/getting-started/
- OGB docs (using app): https://osc.toys/docs/using-oscgoesbrrr/
- OGB docs (advanced settings): https://osc.toys/docs/advanced-settings-guide/
