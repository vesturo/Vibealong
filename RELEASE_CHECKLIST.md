# Vibealong Release Checklist

Use this for every stable release.

## Pre-release

- [ ] Confirm app version bump.
- [ ] Confirm `CHANGELOG.md` updated.
- [ ] Confirm no secrets in repo/config defaults.
- [ ] Confirm local test pass:
  - [ ] Login flow (viewer)
  - [ ] Login flow (creator)
  - [ ] Relay connection
  - [ ] Intiface forwarding
  - [ ] Disconnect/reconnect behavior
- [ ] Confirm default environment points to stable production domain.
- [ ] Confirm docs reflect current behavior.

## Tag and publish

- [ ] Create tag `vX.Y.Z`
- [ ] Push tag to GitHub
- [ ] Verify workflow completes successfully
- [ ] Verify release assets exist:
  - [ ] `Vibealong-vX.Y.Z.exe`
  - [ ] `Vibealong-vX.Y.Z-windows-x64.zip`
  - [ ] `SHA256SUMS.txt`
- [ ] Verify checksums against downloaded files.

## Post-release

- [ ] Smoke test downloaded EXE on clean Windows machine.
- [ ] Announce release with short changelog.
- [ ] Track first 24h errors/regressions.
