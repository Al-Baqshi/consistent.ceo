# The vault

One place for every credential the CEO and their entities use: logins, API keys,
cards, notes, and the environment variables each entity runs with.

This document is the reference for how it works and — more importantly — what it
promises and what it does not. If you are changing anything under
`crates/credentials/`, read the threat model first.

> **Setting it up rather than hacking on it?** See [VAULT_SETUP.md](VAULT_SETUP.md)
> — the operator's guide to running the vault as your own password manager with
> your own keys.

---

## What it guarantees

- **Encrypted at rest.** A stolen disk, backup, or container image yields nothing
  without an unlock credential.
- **A bounded unlock window.** Vaults auto-lock after 15 minutes idle, and locking
  wipes the derived key from memory.
- **Per-entity isolation.** One entity's agents resolve one entity's secrets. Two
  entities can hold different `STRIPE_SECRET_KEY` values and neither can read the
  other's.
- **No single point of loss.** Passkeys, device keys, and recovery codes are
  independent doors onto the same key. Losing any one of them loses nothing.
- **Ciphertext-only sharing.** Shared items transit the gateway sealed to the
  recipient's public key; the gateway relays what it cannot read.
- **Brute-force resistance.** Unlock attempts are throttled per principal with
  escalating backoff, on top of a deliberately slow Argon2id KDF.
- **Replaces the authenticator app.** TOTP (RFC 6238) seeds are stored and codes
  generated in-vault, behind the same unlock.
- **Migration in.** Reads Bitwarden / Chrome / 1Password / LastPass / Safari CSV
  exports, so existing passwords come across in one file.

## What it does not guarantee

**This is not Bitwarden's zero-knowledge model, and it cannot be.**

An unlocked vault is readable by the gateway process, because the entire point is
that the gateway injects those credentials into the agents it runs. A design where
only the browser could decrypt would mean no entity could ever use a secret
unattended — which is the product.

Two consequences, both deliberate:

1. Unlock material — a master password, a passkey PRF result, a device key —
   travels to the gateway over TLS.
2. The gateway holds the derived key in memory while the vault is unlocked.

**Treat the gateway host as exactly as sensitive as the vault itself, because it
is.** Do not run it somewhere you would not store the credentials in plaintext.

---

## Key hierarchy

```
  master password ──Argon2id──┐
  passkey (WebAuthn PRF) ─────┤
  device key (OS keychain) ───┼──> wraps ──> user_key
  recovery code ──────────────┘                 │
                   ┌──────────────────────────┬─┴────────────────┐
                   │                          │                  │
             wraps item_key          wraps X25519 private   encrypts payload
                   │                    (share identity)    (envs, items, audit)
             encrypts ItemSecret
```

Nothing encrypts user data with a key derived directly from a credential. Every
unlock method is another *wrapping* of the same random `user_key`. Three things
follow:

1. **Adding or removing a way in rewraps 32 bytes.** Enrolling a passkey does not
   re-encrypt the vault; revoking a lost laptop does not touch one item ciphertext.
2. **Changing the master password is equally cheap**, and leaves passkeys and
   recovery codes working.
3. **No method is privileged.** A vault can be passkey-only or password-only. The
   single invariant is that the *last remaining* method can never be removed.

### Primitives

| Purpose | Algorithm |
|---|---|
| Password stretching | Argon2id, 64 MiB / t=3 / p=4 (OWASP baseline) |
| Symmetric encryption | AES-256-GCM, random 96-bit nonce |
| Key derivation / domain separation | HKDF-SHA256 |
| Sharing to another user | X25519 anonymous sealed box |
| Recovery codes | 160-bit CSPRNG, Crockford base32 |

Argon2 parameters are stored *in each vault file*, so they can be raised later
without locking anyone out of an existing vault.

---

## Ways in, per platform

| Platform | Method | Backed by |
|---|---|---|
| Browser | Passkey | WebAuthn PRF extension |
| iOS / Android | Device key | Keychain / Keystore, `requireAuthentication` |
| Desktop shell | Device key | macOS Keychain, Windows Credential Manager, Secret Service |
| Anywhere | Master password | Argon2id |
| Anywhere | Recovery code | 160-bit printed code |

### Why passkeys only in the browser

The PRF extension is what makes a passkey usable as a *key*, not just an identity:
the authenticator evaluates a pseudorandom function over a stored salt using a
secret that never leaves the hardware. Same authenticator, same salt, same 32
bytes, forever — and nothing else can produce them.

Inside an embedded webview this is not dependable. WKWebView needs
associated-domain entitlements before it surfaces platform passkeys, WebView2
behaves differently, and WebKitGTK largely does not implement PRF. Shipping a
button that works on one of three platforms is worse than not shipping one, so the
desktop shell uses an OS-keychain device key instead.

React Native has no WebAuthn at all, so mobile does the same — with
`requireAuthentication`, which forces a biometric check before the key can be read.

**The desktop guarantee is weaker than the phone's** and is documented as such: it
is "this machine, while its owner is logged in", not a biometric prompt per unlock.

### Why the passkey ceremony is not server-verified

Registration and login ceremonies exist to convince a server that a challenge it
issued was signed by a credential it trusts. The vault's PRF flow is not doing
that — the signature is never checked. What matters is only the PRF output, whose
security comes from the authenticator plus the user-verification gesture.

The operation that *does* need authority is guarded server-side instead: enrolling
a new way in requires an already-unlocked vault, so a stolen session cannot quietly
add an attacker's authenticator.

---

## Scoped environments

Secrets live under a scope: `global`, or `entity:{id}`.

```
entity:acme  ->  OPENAI_API_KEY  (the entity's own key, if set)
             ->  OPENAI_API_KEY  (global, otherwise)
```

Resolution falls back **exactly one step**, never a chain. A deeper hierarchy would
mean predicting which value wins requires knowing the whole tree, and the cost of
guessing wrong with credentials is charging the wrong customer's card.

### Agent runtime resolution

When an agent runs a task for an entity, its API key resolves in this order:

1. **The entity's own vault value** — deliberately set for this entity and nothing
   else, so it outranks everything.
2. **The process environment** — kept above the vault's global scope because it is
   the only source available in containerised deploys, where there is no keychain
   and nobody to unlock a vault interactively.
3. **The vault's global scope**, then **the OS keyring**.

Vaults are per-principal, but a scheduled task has no request and therefore no
principal. Rather than guess — "any unlocked vault" would serve one tenant's keys
to another tenant's agents — the deployment nominates one explicitly:

```bash
CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL=<principal-id>
```

**Unset is the default**, and means the vault is not consulted at all; resolution
behaves exactly as it did before this existed. That is right for a shared
deployment, and a one-line opt-in for a personal brain box.

A locked vault is an ordinary state, not an error: resolution falls straight
through to env and keyring rather than stalling an unattended run.

---

## Surviving ten years

This is the part that matters most and gets tested least, so it is spelled out.

### Recovery codes

Every vault is born with one, shown once at creation. A passkey lives in one
vendor's keychain; if that is the only way in, the vault is one lost phone or one
closed Apple account away from being landfill.

160 bits in Crockford base32 — 32 characters, eight groups of four. Decoding
forgives exactly the errors people make transcribing from paper (case, dashes,
spaces, `O`→`0`, `I`/`L`→`1`) and rejects everything else rather than guessing.

There is no server-side copy. If it is not written down, it does not exist.

### Portable backups

`POST /api/vault/export` produces a passphrase-encrypted archive that is
**deliberately independent of the vault's key hierarchy**. It carries decrypted
items re-encrypted under a fresh Argon2id key, so restoring needs nothing but the
passphrase — no passkey, no device, no copy of the original file.

That independence is the only property that actually survives a decade of laptop
changes. It is also why the passphrase is the only thing between a stolen backup
and every secret in it: **use a different one from your master password.**

Importing is **additive**. Items are appended with fresh ids and fresh keys, and a
variable that already exists is left alone. An import can never destroy what the
current vault holds — the behaviour you want at 2am when you are not sure which
copy is newer.

### Format policy

The on-disk format is versioned and JSON, so it stays inspectable with `jq` long
after any particular binary is gone.

- **Migration is automatic and one-way-safe.** v1 and v2 files are upgraded on
  first unlock. The version is raised *only after a successful unlock*, so an
  upgrade can never strand a file the caller could already read.
- **Upgrades keep a copy.** The first write at a new format version copies the old
  file to `<vault>.v<N>.bak` and never overwrites it. Atomic writes protect against
  crashes; this protects against a *bug in the new writer*, which is the failure
  atomicity does not cover.
- **Unknown fields are preserved.** Fields written by a newer build round-trip
  through an older one instead of being silently dropped. Running a new version on
  a laptop and an older one on a server must not destroy data.
- **Unknown versions are refused, not guessed at.** A file from the future reports
  an error rather than being partially misread.
- **Writes are atomic and durable.** Write to a temp file, `fsync`, tighten to
  `0600`, `rename`, then `fsync` the directory. Atomicity is about ordering, not
  persistence: without the `fsync` a rename can reach disk while the data it points
  at is still in the page cache, and a power cut leaves a file that is intact,
  current, and full of zeroes.

### Known bounds

- **AES-GCM nonces are random and 96 bits.** The payload is resealed under the same
  key on every write. The birthday bound is comfortable — 10 years at one write per
  second is ~2^28 writes, giving a collision probability around 2^-40 — but if the
  vault ever becomes a high-frequency write target, move to XChaCha20-Poly1305 and
  its 192-bit nonces.
- **The audit log is capped at 500 entries.** It lives inside the encrypted payload,
  which is rewritten in full on every change, so it cannot grow without bound.
- **Item names, folders, and scope names are encrypted**, but the *number* of items
  and the file size are not. The vault does not hide metadata from someone holding
  the file.

---

## API

All routes sit behind bearer auth and operate on the calling principal's own vault.
`423 Locked` is flow control, not an error — it is the signal to show the unlock
prompt, and is distinct from `401`, which means the session expired.

### Lifecycle
| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/vault/status` | Includes `unlockMethods`, readable while locked |
| `POST` | `/api/vault/init` | **Returns `recoveryCode` — show it once** |
| `POST` | `/api/vault/unlock` | Master password |
| `POST` | `/api/vault/unlock/key` | Passkey PRF result or device key |
| `POST` | `/api/vault/unlock/recovery` | Printed recovery code |
| `POST` | `/api/vault/lock` | |
| `POST` | `/api/vault/change-password` | Rewraps 32 bytes; leaves other methods working |

### Ways in
| Method | Path |
|---|---|
| `GET` / `POST` | `/api/vault/unlock-methods` |
| `DELETE` | `/api/vault/unlock-methods/{id}` |
| `GET` | `/api/vault/random-key` |
| `POST` | `/api/vault/recovery-code` |

### Items
| Method | Path |
|---|---|
| `GET` / `POST` | `/api/vault/items` |
| `GET` / `PUT` / `DELETE` | `/api/vault/items/{id}` |
| `PUT` | `/api/vault/items/{id}/metadata` |

Listing returns metadata only. The secret half requires a second call, so a UI can
render a list without pulling every password over the wire.

### Secrets and environments
| Method | Path | Notes |
|---|---|---|
| `GET` / `POST` | `/api/vault/secrets` | `?scope=` defaults to `global` |
| `GET` / `DELETE` | `/api/vault/secrets/{key}` | `?resolve=true` follows the fallback |
| `GET` | `/api/vault/scopes` | Scopes holding at least one variable |
| `GET` | `/api/vault/env` | Full resolved environment for a scope |

### Durability and sharing
| Method | Path |
|---|---|
| `POST` | `/api/vault/export` / `/api/vault/import` |
| `GET` | `/api/vault/audit` |
| `GET` | `/api/vault/health` — weak/reused/stale password report (value-free) |
| `GET` | `/api/vault/public-key` |
| `POST` | `/api/vault/share` / `/api/vault/share/accept` |

---

## Operations

```bash
# Where vault files live (one per principal, 0600)
~/.consistent-world/vaults/{principal_id}.json

# Nominate the vault backing unattended agent runs (opt-in)
CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL=<principal-id>

# Passkey relying party — must match the domain the apps are associated with
PASSKEY_RP_ID=consistent.ceo
PASSKEY_RP_ORIGIN=https://consistent.ceo
```

Mobile passkeys additionally need `apple-app-site-association` (iOS) and
`assetlinks.json` (Android) served over HTTPS at that domain, with the matching
associated-domains entitlement in the app.

---

## Testing

```bash
cargo test -p consistent-world-credentials   # 101 tests
```

The migration tests are the ones to keep honest. `write_v2_vault` spells its JSON
out by hand rather than serializing from the current structs — a test that
round-trips through today's types proves only that the build agrees with itself,
and would happily pass while a serde rename made every real file on disk
unreadable. When you add a format version, write its fixture the same way.
