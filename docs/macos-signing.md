# Signing and notarizing the macOS app

Every downloader currently meets *"Apple could not verify Consistent CEO is free
of malware"* and has to go into System Settings to launch the app. That warning
is not about the code — it is Gatekeeper reporting that no Apple-issued
certificate vouches for the build. Clearing it takes two things: a **Developer
ID Application** certificate, and a trip through Apple's **notary service**.

Once both are in place, `desktop/scripts/release-macos.sh` does the whole
release in one command and downloaders see no warning at all.

## Why not the Mac App Store

The desktop app cannot ship there, and it is worth being explicit about why so
the question does not get reopened.

The Mac App Store requires **App Sandbox**. A sandboxed app may only execute
binaries inside its own bundle, and may only reach files the user picks through
a system panel. This app does the opposite by design: it spawns the operator's
own `claude` and `codex` CLIs from `/usr/local/bin`, and reads real project
folders by absolute path. Sandboxing it would remove the product.

Direct distribution with Developer ID + notarization is the supported Apple
route for exactly this kind of app, and it produces the same clean install.
(The iOS companion is unaffected — that ships through the App Store normally.)

## One-time setup

Apple account: `hello@baqshi.com`. All of this is done once; certificates last
five years.

### 1. Create the Developer ID Application certificate

A certificate signing request has already been generated on this machine, with
the private key kept out of the repo:

```
~/.consistent-signing/developer_id.csr   # upload this
~/.consistent-signing/developer_id.key   # never leaves the machine
```

**This step needs a browser, and cannot be automated.** Apple restricts creating
Developer ID certificates to the **Account Holder**, a role that App Store
Connect API keys cannot hold — attempting it with the account's key returns:

```
403 FORBIDDEN_ERROR — This operation can only be performed by the Account Holder.
```

So do it in the portal, signed in as the account holder (`hello@baqshi.com`):

1. <https://developer.apple.com/account/resources/certificates/add>
2. Choose **Developer ID Application** → **Continue**. Not any of the iOS
   Distribution certificates already on the account: those sign iOS apps for
   the App Store and cannot sign a Mac app for distribution outside it.
3. If it offers a sub-CA, choose **G2 Sub-CA** — that is the intermediate
   `import-signing-cert.sh` fetches.
4. Upload `~/.consistent-signing/developer_id.csr` (a copy is placed in
   `~/Downloads/` for the file picker; otherwise press ⇧⌘G and paste the path).
5. Save the issued certificate as
   `~/.consistent-signing/developerID_application.cer`.

The account's Team ID is **RK5R4HLLRB** (shown under the account name in the
developer portal). The release script does not need it — the App Store Connect
API key covers notarization — but `notarytool store-credentials` does.

`~/.consistent-signing/create_devid_cert.py` remains useful: run it to *check
for and download* an existing Developer ID certificate. It only fails on
creation.

> Apple caps Developer ID certificates at five per account, and they last five
> years — do not burn them on experiments.

### 2. Import the certificate and its chain

```sh
desktop/scripts/import-signing-cert.sh
```

This pulls Apple's intermediate CA (without it the chain does not validate and
codesign refuses the identity even though the leaf imported fine), imports the
certificate and its private key, and prints the resulting identity along with
the **Team ID** — read off the certificate subject, so it never needs looking
up separately.

### 3. Notary credentials

Nothing to do. The release script submits with that same App Store Connect API
key, which needs neither an app-specific password nor a Team ID.

To use a different credential, either export `APPLE_API_KEY_PATH`,
`APPLE_API_KEY` and `APPLE_API_ISSUER`, or store a keychain profile and set
`NOTARY_PROFILE`:

```sh
xcrun notarytool store-credentials consistent-notary \
  --apple-id hello@baqshi.com --team-id <TEAM_ID> --password <app-specific-password>
```

### 4. Update-signing key (auto-update)

Installed apps only accept an update signed with the key whose public half is
`plugins.updater.pubkey` in `desktop/src-tauri/tauri.conf.json`. The private
half lives at `~/.consistent-signing/updater.key` and never enters the repo.
It was created with:

```sh
cd desktop && npx tauri signer generate --ci -p "" -w ~/.consistent-signing/updater.key
```

**Back up `~/.consistent-signing/`** (password manager or an encrypted drive).
Losing `updater.key` means every installed copy stops updating; generating a
new one means shipping a new public key, which only reaches users who
reinstall by hand.

## Building for this machine

```sh
cd desktop && npm run build:signed
```

Signs with the Developer ID and stops — no notary round trip. Use this for the
copy you run yourself.

It is not optional in practice. An ad-hoc signed app has no stable identity:
every rebuild changes its code directory hash, so a macOS keychain ACL can never
match it and **"Always Allow" does not stick**. The app asks for the login
keychain password again on the next launch, and the next. Signing with a real
certificate is what makes that grant persist. `npm run build` still produces an
unsigned bundle, and an unsigned bundle will keep prompting.

## Releasing

```sh
desktop/scripts/release-macos.sh
```

It builds, signs every binary in the bundle, submits the DMG to Apple, waits for
the ticket, staples it, runs the same Gatekeeper assessment a downloader's Mac
will run, and copies the result to the landing repo under the stable filename
the site links to. Then commit and push the landing repo to deploy.

Pass `--skip-notarize` to produce a signed-but-not-notarized build for local
testing.

## What is actually signed

Four binaries live in the bundle and each needs its own signature — the notary
service rejects the app if any one of them is unsigned:

| Binary | Role |
| --- | --- |
| `consistent` | the Tauri shell |
| `consistent-world` | the gateway sidecar (also serves the embedded dashboard) |
| `consistent-whisper` | on-device transcription |
| `consistent-dictate` | dictation helper |

`desktop/src-tauri/entitlements.plist` carries the hardened-runtime
entitlements: audio input for voice mode, local network client/server for the
gateway, and library-validation disabled so the sidecars load. App Sandbox is
deliberately absent — see above.

## Verifying a build the way a user's Mac does

```sh
spctl -a -vvv -t install "path/to/Consistent-CEO-macOS-Intel.dmg"
xcrun stapler validate "path/to/Consistent-CEO-macOS-Intel.dmg"
```

A notarized build reports `accepted` and `source=Notarized Developer ID`.

## Note on locally built copies

A build produced on this machine has no `com.apple.quarantine` attribute, so it
launches without a prompt even when unsigned. That is why the app can be
installed here with no warning while a downloaded copy of the *same build* is
blocked — Gatekeeper only enforces on files that arrived from a browser. Do not
read a clean local launch as evidence that downloaders are fine.
