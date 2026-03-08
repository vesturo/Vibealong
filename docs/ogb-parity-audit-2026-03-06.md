# OscGoesBrrr Parity Audit (Clean-Room)

Date: 2026-03-06  
Audited OGB snapshot: `6a3421f0a755e990238b2d75b71b66430f029929`

## Scope and constraints

- This audit compares behavior and user-facing capability only.
- No source code from OGB may be copied or adapted.
- VRLewds bridge keeps secure SPS relay requirements as non-negotiable.

## Current VRLewds bridge state

Implemented today:

- In-app profile management (SQLite + keyring secret storage)
- Runtime start/stop (Engage/Disengage)
- OSC UDP ingest + packet forwarding
- Mapping + weighted composite + smoothing
- Secure session/ingest relay client
- Auto-disengage safety for repeated throttle/auth/relay failures
- Basic desktop UI for profile CRUD and runtime snapshot

## Progress update (after parity sprint pass)

Newly implemented in clean-room Rust app:

- Runtime observability upgrades:
  - in-app log stream ring buffer
  - avatar parameter cache for debugger panel
  - richer runtime counters (mapped/unmapped, discovery writes, source host)
- VRChat diagnostics module:
  - Windows registry checks for OSC/self/everyone interaction flags
  - latest VRChat log scan for OSC startup failure + OSCQuery port hint
- OSCQuery module:
  - mDNS browse attempt + VRChat-log fallback probing
  - bulk value fetch and extraction utility
- Intiface module:
  - status probe (server info + device list)
  - feature parsing (scalar/linear/rotate)
  - scalar test command path
- Intiface direct bridge runtime:
  - engage/disengage direct mode
  - continuous intensity push loop with reconnect/probe cadence
  - command counters + runtime status in UI
  - advanced route rules:
    - device name filter
    - actuator type filter
    - source selection (`intensity` or named avatar param)
    - per-route scale/idle/min/max/invert shaping
- Desktop UI rewrite:
  - tabs: Setup Wizard, Home, Profiles, Logs, Avatar Debugger, Diagnostics, Intiface
  - engage/disengage retained
  - no-manual-JSON workflow retained
- Desktop single-instance guard (prevents duplicate bridge process)
- Background close behavior:
  - close-to-background option intercepts window close and minimizes instead of exiting

## Parity matrix vs OGB

`Complete`

- Desktop app launchable as EXE
- Bridge engage/disengage control
- OSC receive + parse + relay output path
- Raw OSC forwarding to local targets

`Partial`

- Runtime status UI (significantly improved; still not full OGB toy-routing diagnostics)
- Profile/config editing (we have typed form; OGB has advanced text config model)
- Discovery/debug config fields (runtime wiring now present; UI controls still basic)
- OSCQuery support (discovery/fetch present, but not yet runtime-integrated as primary ingest)
- Intiface support (probe/list/test plus continuous direct bridge; advanced source-to-feature routing still incomplete)
- Intiface support (probe/list/test + continuous direct bridge + rule-based routing now present)
- Setup wizard UX (baseline checklist flow implemented, not yet full guided flow)

`Missing`

- Toy/device feature routing model (bind type/id/key, linear/rotate/vibrate behavior parity)
- Audio FFT input mode
- Setup/diagnostic wizard UX
- Tray UX still missing (background/minimize and close interception are now present)
- Auto-update channel and installer polish

## What this means for "full OGB feature parity"

We currently have a secure bridge core MVP, not OGB-level product parity.  
Core transport/security is strong; ecosystem integration and operator UX are the major gaps.

## Recommended implementation order

1. Observability + Diagnostics foundation
   - Runtime event bus and structured log stream
   - UI tabs: Logs, OSC diagnostics, relay diagnostics
   - VRChat config/log checks
2. OSCQuery parity pack
   - OSCQuery discovery + fallback to direct UDP
   - Avatar parameter cache and debugger panel
3. Intiface parity pack
   - Buttplug client, scan lifecycle, device/feature model
   - Safe rate limiting and command scheduler
4. Source expansion pack
   - Audio FFT source
   - Advanced source routing controls in UI
5. Product hardening
   - Installer/update flow
   - Tray/minimize behavior
   - Startup health checks and recovery

## Security requirements to keep while adding parity

- Never store `stream_key` in plaintext profile storage.
- Never emit secrets to logs/UI.
- Preserve relay seq monotonicity and timestamp skew handling.
- Keep anti-throttle disengage behavior enabled by default.
