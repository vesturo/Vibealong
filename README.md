# Vibealong Companion

Desktop companion app for VRLewds Vibealong.

Vibealong Companion receives control signals from VRLewds and forwards them to local toy/control software (for example Intiface-compatible setups), with creator and viewer auth flows handled through VRLewds Identity.

## Why this repo is public

- Transparency for creators and viewers.
- Verifiable release artifacts.
- Clear issue reporting and security disclosure path.

## Download

- Stable Windows builds are published in GitHub Releases.
- Download only from Releases in this repository.
- Verify checksums from `SHA256SUMS.txt` before running binaries.

## Quick start (dev)

Requirements:
- Rust stable toolchain
- Windows (primary target)

Run desktop app:

```bash
cargo run -p vrl-osc-core --bin vibealong
```

Build release binary:

```bash
cargo build -p vrl-osc-core --bin vibealong --release
```

Output:

```text
target/release/vibealong.exe
```

## Release process

- Tag format: `vX.Y.Z` (example: `v0.1.0`)
- Push tag
- GitHub Actions builds and publishes:
  - `Vibealong-vX.Y.Z.exe`
  - `Vibealong-vX.Y.Z-windows-x64.zip`
  - `SHA256SUMS.txt`

Detailed operator checklist: [`RELEASE_CHECKLIST.md`](./RELEASE_CHECKLIST.md)

## Security

Please see [`SECURITY.md`](./SECURITY.md) for vulnerability reporting.

## License

This repository is **source-visible for transparency only**.
No permission is granted to use, copy, modify, distribute, or run this software.

See [`LICENSE`](./LICENSE) for full terms.
