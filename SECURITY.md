# Security Policy

## Reporting a vulnerability

Do not open public issues for suspected security vulnerabilities.

Report privately with:
- Steps to reproduce
- Impact
- Affected version/build
- Optional proof-of-concept

Suggested contact:
- security@vrlewds.com

If email is not yet active, use a private maintainer channel and mark it as `SECURITY`.

## Scope

In scope:
- Authentication/token handling
- Local secret storage
- Companion login callback flow
- Local IPC/listener exposure
- Relay/session misuse
- Unsafe defaults in networking/config

Out of scope:
- Bugs that do not affect confidentiality, integrity, or availability

## Disclosure timeline targets

- Initial response: within 72 hours
- Triage complete: within 7 days
- Patch or mitigation: as fast as possible, severity-based
- Public advisory: after fix is available
