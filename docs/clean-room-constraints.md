# Clean-Room Constraints

Last updated: 2026-03-06

This repository must not copy or adapt source code from OscGoesBrrr.

## Allowed

- Behavioral parity goals
- Public protocol behavior observations
- High-level feature comparisons
- Independent implementation from VRLewds-owned specs and code

## Not allowed

- Copying source files, functions, or structure from OGB
- Porting OGB code between languages
- Reusing OGB config grammar as implementation artifact without original design review

## Engineering rules for this repo

- Treat `docs/bridge-core-spec-v1.md` as source of truth for parity behavior.
- Derive implementation details from `scripts/vrl-osc-bridge.mjs` and internal API contracts.
- Keep tests focused on expected behavior, not on matching OGB internals.

