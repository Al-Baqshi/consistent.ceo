# Security

Consistent CEO runs on people's own computers and can act on their files, so
security reports get priority.

## Reporting a vulnerability

Please report privately, not in a public issue:

- **Preferred:** GitHub → this repository → Security → *Report a
  vulnerability*.
- **Or email** hello@consistent.ceo with "SECURITY" in the subject.

Include what you found, how to reproduce it, and what an attacker gains. You
will get an answer within a week. Please give a reasonable window to fix it
before you publish.

## What is in scope

- The gateway's HTTP API and authentication: sessions, API keys, passkeys,
  device pairing (`crates/gateway/src/pairing.rs`), the relay.
- The directory jail for unattended work and the approval gate for
  outward-facing actions.
- The vault and credential storage ([docs/VAULT.md](docs/VAULT.md) has the
  threat model).
- The desktop shell and phone app's handling of tokens and keys.

## The security model in brief

- The gateway listens on `127.0.0.1` by default. Phones reach it through
  Tailscale or a relay you run, both of which forward to loopback.
- A phone pairs by scanning a single-use code shown only on the computer's
  screen, then the owner approves it there after matching six digits. The
  phone signs in afterwards with an Ed25519 key that never leaves it.
- A gateway bound to a network address refuses to start without an explicit
  token decision, so it is never accidentally open.
- Nothing is sent to the project's authors. There is no telemetry.

## Supported versions

Fixes land on `main` and ship in the next release. Only the latest release is
supported.
