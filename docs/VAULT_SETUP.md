# Setting up your vault

A guide for running the Consistent vault as your own password manager, on your
own machine or server, with your own keys. If you only want to know how the vault
works internally, read [VAULT.md](VAULT.md) instead — this is the operator's
guide.

The whole idea: **one vault holds every credential you and your AI entities use,
you unlock it once with a passkey or your face, and everything else stays sealed.**
No third party holds your keys, because you are the third party.

---

## The five-minute version

1. Run the gateway (desktop app, Docker, or `cargo run`).
2. Open the vault, pick a master password. Write down the recovery code it shows
   you — this is the one screen you cannot get back.
3. Add a passkey (browser) or your face (phone/desktop) so you rarely type the
   password again.
4. Import your existing passwords from a CSV export.
5. That's it. Turn on the agent vault (below) if you want your AI entities to use
   these credentials.

Everything after this is detail for people who want to get it exactly right.

---

## 1. Run it

### Desktop app (simplest)

Get it from the **[Download page](/download)** — it detects your OS and hands you
the right installer (macOS Apple Silicon/Intel, or Windows). The app bundles the
gateway and opens the vault in a native window. Your vault file lives at
`~/.consistent-world/vaults/`. Nothing leaves your machine.

> **Publishing your own downloads.** Installers are built by
> `.github/workflows/release-desktop.yml` when you push a version tag (e.g.
> `git tag v0.1.0 && git push --tags`) and attached to a GitHub Release under
> stable names, so the download buttons resolve via
> `releases/latest/download/<name>`. GitHub blocks anonymous downloads on
> **private** repositories — to let the public download directly, either make the
> repo (or its releases) public, or host the installers on your own CDN and set
> `VITE_DOWNLOAD_BASE` to that URL when building the web app.

### Docker / server

```bash
docker compose up -d
```

The vault is served at `/api/vault/*` behind the gateway's bearer auth. Point the
web app or mobile app at your gateway's URL.

**If your server is reachable from the internet, read the [Security](#5-security-for-a-reachable-server)
section before you store anything real.**

### From source

```bash
cargo run -p consistent-world-gateway
```

---

## 2. Create your vault the right way

### The master password

It is the root of everything. There is no "forgot password" link, by design —
a reset link would be a way in for anyone who compromises your email.

- The creation screen shows a live strength meter (real crack-cost estimation,
  not a character-class rule). Aim for **Strong** or better.
- A long passphrase — four or five unrelated words — beats a short scramble of
  symbols, and is far easier to type on a phone.
- The vault never sends the password anywhere it can avoid, and stores only a key
  *derived* from it (Argon2id, 64 MiB). It cannot reconstruct the password.

### The recovery code — do not skip this

At creation you are shown a 32-character recovery code, once. It opens the vault
if you lose your password **and** every enrolled device.

- Write it on paper. Put it somewhere a house fire and a hard-drive failure will
  not reach at the same time — a real drawer, a safe, a trusted person.
- It is not stored anywhere the server can retrieve. If you close that screen
  without saving it, you have a vault with one fewer way back in.
- You can mint additional recovery codes later from **Security → New recovery
  code**. Old ones keep working until you rotate the vault.

---

## 3. Set up convenient unlock

Typing a strong master password every time is friction, and friction is why
people pick weak passwords. Add a fast unlock, then let the master password
become the thing you use once a month.

| You are on… | Add this | Where |
|---|---|---|
| A browser | A **passkey** (Face ID, Touch ID, Windows Hello, or a security key) | Security → Add a passkey |
| iPhone / Android | **This device** (biometric-gated key in the OS keychain) | Security → Add this device |
| The desktop app | **This computer** (key in the OS keychain) | Security → Add this computer |

Each of these is independent. Enrolling your phone does not touch your browser
passkey; revoking a lost laptop does not affect anything else. The master password
and recovery code always remain as fallbacks — and the vault will not let you
remove your last remaining way in.

### Passkeys need a real domain

Passkeys are bound to the domain you serve the app from. On `localhost` they work
for local testing out of the box. To use them on your own domain, set:

```bash
PASSKEY_RP_ID=vault.example.com
PASSKEY_RP_ORIGIN=https://vault.example.com
```

For passkeys **in the mobile app**, you also need to publish the platform
association files at that domain over HTTPS:

- iOS: `https://vault.example.com/.well-known/apple-app-site-association`
- Android: `https://vault.example.com/.well-known/assetlinks.json`

and the matching associated-domains entitlement in the app build. Until then, the
mobile app falls back to its device-key unlock, which needs none of this.

---

## 4. Bring your existing passwords

You already have passwords somewhere. Move them in one file.

1. Export a CSV from your current manager:
   - **Bitwarden** — Tools → Export vault → `.csv`
   - **Chrome / Edge / Brave** — Settings → Passwords → Export
   - **1Password** — File → Export → CSV
   - **LastPass** — Advanced → Export
   - **Safari / iCloud Keychain** — Passwords → Export
2. In the vault, **Passwords → Import**, drop the file in.
3. You get a preview: how many logins were found, which rows will be skipped and
   why. Nothing is stored until you confirm.
4. Confirm. Your TOTP (2FA) seeds come across too, so you can retire your
   authenticator app.

**Then delete the CSV.** It is a plaintext copy of everything you own. The import
does not need it after it runs.

Unlisted manager? Export a CSV anyway — the importer matches columns by meaning,
so anything with recognisable name/URL/username/password columns usually comes in.

---

## 5. Security for a reachable server

If your gateway is only ever on `localhost` or a private network, the defaults are
fine. If it is reachable from the internet, treat it as seriously as you would any
password manager server:

- **The gateway host is as sensitive as the vault.** An unlocked vault is readable
  by the gateway process — it has to be, so it can feed credentials to your
  entities. Do not run it on a box you would not trust with the plaintext.
- **Terminate TLS in front of it.** Unlock material crosses the wire; it must
  cross an encrypted one. Put it behind a reverse proxy with a real certificate.
- **Unlock attempts are already throttled** per user (escalating backoff, capped
  at 15 minutes), on top of the deliberately slow Argon2id. You do not need to add
  your own, but do not remove it.
- **Vaults auto-lock after 15 minutes idle**, wiping the key from memory. Lock
  manually when you walk away.
- **Back up.** See below.

---

## 6. Never lose the vault

Three independent safety nets, in order of how much you should rely on them:

1. **The recovery code** (§2). Your primary "everything is gone" escape.
2. **Encrypted backups.** Security → Backup → Download. This produces a file
   encrypted under a passphrase *of its own* — use a different one from your master
   password, so a stolen backup and a stolen vault are not one compromise. It
   restores with nothing but that passphrase: no device, no passkey, no copy of the
   original. Store one somewhere durable and off the machine.
3. **The vault file itself** at `~/.consistent-world/vaults/{you}.json`, encrypted
   at rest. If you back up your home directory, this comes along — but it needs a
   working unlock method to open, which is why (1) and (2) exist.

Restoring a backup is **additive**: it only adds what is missing and never
overwrites what is already there, so restoring the wrong file at 2am cannot
destroy your current vault.

---

## 7. Let your AI entities use these credentials

This is the part that makes it more than a password manager. Your entities can run
with credentials from the vault, scoped so each entity only sees its own.

- In **Environment**, pick a scope: **Global** for shared defaults, or add an
  entity to give it its own space. An entity's own value for a key shadows the
  global one — so two entities can bill two different Stripe accounts from the same
  gateway, and neither can read the other's.
- To let **unattended agent runs** resolve these (a scheduled task has no logged-in
  user to unlock a vault), nominate which vault backs them:

  ```bash
  CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL=<your-principal-id>
  ```

  Leave it unset (the default) and the vault is not consulted for agent runs at
  all — the right choice on a shared deployment. Set it on your personal box and
  your entities inherit your keys, with per-entity overrides.

A locked vault never stalls an agent: resolution falls through to the process
environment and OS keyring, so a run in progress does not hang waiting for you to
type a password.

---

## Quick reference

```bash
# Vault files (one per user, 0600, encrypted at rest)
~/.consistent-world/vaults/{principal_id}.json

# Passkeys on your own domain
PASSKEY_RP_ID=vault.example.com
PASSKEY_RP_ORIGIN=https://vault.example.com

# Let unattended agent runs use your vault (opt-in)
CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL=<principal-id>
```

| I want to… | Where |
|---|---|
| Add a fast unlock | Security → Add passkey / device / computer |
| Get a new recovery code | Security → New recovery code |
| Import from another manager | Passwords → Import |
| Store a 2FA seed | Open an item → Two-factor field |
| Give an entity its own keys | Environment → add entity → set values |
| Make a backup | Security → Backup → Download |
| Change the master password | Security → Change master password |
