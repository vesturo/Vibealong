# VRLewds Bridge Core Behavior Spec (v1)

Last updated: 2026-03-06

## 1) Scope

This spec defines the behavior contract for the clean-room bridge core. It is derived from the current production script:

- `scripts/vrl-osc-bridge.mjs`

The contract below is the parity target for initial Rust core extraction. UI, storage, and packaging are out of scope for v1.

## 2) OSC Packet Parsing Contract

### 2.1 Supported packet forms

- Single OSC message packet.
- OSC bundle packet (`#bundle`) containing nested message/bundle chunks.

### 2.2 Supported OSC argument types

- `i` -> signed 32-bit integer
- `f` -> 32-bit float
- `T` -> boolean true
- `F` -> boolean false
- `s` -> OSC string

Any message containing unsupported type tags is treated as invalid and discarded.

### 2.3 String and alignment rules

- OSC strings are null-terminated UTF-8.
- Offsets are aligned to 4-byte boundaries.
- Invalid string termination or alignment overflow invalidates the message.

### 2.4 Bundle behavior

- Bundle timetag is read and ignored for routing logic.
- Chunks are parsed recursively.
- Invalid chunk size (`<= 0` or extends past packet end) stops bundle iteration.
- Valid messages already parsed before an invalid chunk are retained.

### 2.5 Message metadata

Each parsed message exposes:

- `address`: OSC address path string
- `args`: parsed argument list
- `arg_type`: first type tag character (empty if none)
- `arg_types`: full type tag list without leading comma

### 2.6 Numeric extraction

When a mapping evaluates a message:

- First finite numeric argument wins.
- If no numeric exists, first boolean maps to `1` (true) or `0` (false).
- Otherwise the message is non-numeric for mapping purposes.

## 3) Input Mapping Contract

### 3.1 Mapping fields

Per input mapping:

- `address` (must start with `/`)
- `weight` (default `1`)
- `curve` (`linear`, `easeInQuad`, `easeOutQuad`, `easeInOutQuad`)
- `invert` (default `false`)
- `deadzone` clamped to `[0,1]`
- `min` (default `0`)
- `max` (default `1`, auto-adjust to `min + 1` if `max <= min`)

### 3.2 Raw-to-normalized transform

For numeric `raw`:

1. Normalize: `(raw - min) / (max - min)`
2. Clamp to `[0,1]`
3. Apply invert if enabled (`v = 1 - v`)
4. Apply deadzone (`v = 0` if `v < deadzone`)
5. Apply curve
6. Final clamp to `[0,1]`

### 3.3 Composite intensity

- Store latest mapped sample per address.
- Composite uses configured mappings in order-independent weighted average:
  - `sum(sample * abs(weight)) / sum(abs(weight))`
- Missing addresses are skipped.
- If total weight is `0`, composite intensity is `0`.

## 4) Smoothing and Emission Contract

### 4.1 Tick rate

- Tick interval is derived from output `emitHz` (bounded elsewhere).

### 4.2 Attack/release + EMA smoothing

At each tick:

1. `dtMs = max(1, nowMs - lastTickMs)`
2. Select time constant:
   - `attackMs` if `target >= current`
   - `releaseMs` otherwise
3. `lerpAlpha = 1 - exp(-dtMs / max(1, tauMs))`
4. `next = current + (target - current) * lerpAlpha`
5. `next = emaAlpha * next + (1 - emaAlpha) * current`
6. Clamp `current = clamp01(next)`

### 4.3 Peak behavior

- If `current >= peak`, set `peak = current`.
- Else decay by fixed step: `peak = max(current, peak - 0.025)`.

### 4.4 Emit gating

Emit is allowed when either:

- `abs(current - lastSentIntensity) >= minDelta`
- `nowMs - lastSentAtMs >= heartbeatMs`

## 5) Relay Payload Contract (Engine Output)

Core output shape for relay publishing must preserve:

- `seq` (monotonic increasing integer)
- `ts` (client-adjusted timestamp, milliseconds)
- `intensity` (smoothed value `[0,1]`)
- `peak` (peak envelope `[0,1]`)
- `raw` (unsmoothed composite target)
- `source.address` (most recent mapped message address)
- `source.argType` (most recent mapped message first type tag)

## 6) Error/Resilience Baseline

- Parse failures are non-fatal and only affect the current packet/message.
- Invalid mappings are dropped during normalization.
- Numeric normalization never returns NaN to downstream consumers.

## 7) Test Matrix (Required for Parity)

Must include unit tests for:

- OSC message parsing and bundle recursion
- Type support and rejection behavior
- Numeric extraction precedence
- Mapping normalization (invert/deadzone/curve/min-max)
- Composite weighted average (`abs(weight)`)
- Smoothing response (attack/release)
- Peak decay
- Emit gating (`minDelta` and heartbeat)

