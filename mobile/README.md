# Consistent CEO — Mobile

Expo (React Native) companion app for the Consistent CEO gateway. The phone is a
**control surface**: talk or type to your world, browse it as a living map, approve
sensitive actions, see scheduled work, and inspect what the system is doing.

## Screens

Tab labels follow the desktop sidebar where the concept is the same (Chat, Entities, Vault, Today); route file names are unchanged.

| Tab / route | What it does |
|---|---|
| **Chat** (`app/(tabs)/index.tsx`) | Chat with the assistant, streamed. Composer does both **text and voice**, plus an entity picker ("Whole workspace ▾") that sends real scope (`entityId` + `scope`), the session id, recent history and the timezone offset — the same fields desktop sends. Approval cards and operator-feed events (styled by kind, tappable) render inline. |
| **Entities** (`app/(tabs)/brain.tsx`) | Full-bleed animated map of all entities as glowing planets on a GPU-shader night sky (SkSL nebulas + twinkling stars). Tap to select (card + lit relation edges + energy pulses), pinch/pan/double-tap to navigate, **hold ~¼s and drag to move a planet** (positions persist). Search + type filters float on top. |
| **Calendar** (`app/(tabs)/calendar.tsx`) | Month grid with dots; tap a day for its items: open tasks with a `due_at` deadline and cron jobs from `/api/schedule`. |
| **Approvals** (`app/(tabs)/control.tsx`) | Everything waiting on a decision: sign-in requests and action approvals, newest first. Platform status lives at `/system`. |
| **Today** (`app/(tabs)/review.tsx`) | Daily review from `/api/daily-review` plus open missions (swipe for done / snooze). |
| **Voice** (`app/companion.tsx`, modal) | Live full-duplex voice/text session. |
| **Profile** (`app/profile.tsx`, modal) | Who is signed in, which computer this phone is connected to, trusted devices, doors to Settings / Logs / session auth. Opened from the avatar or `consistentworld://profile`. |
| **Logs** (`app/logs.tsx`, modal) | In-memory log feed (gateway calls, system, workers) with filters. Every real gateway attempt and mock fallback is logged here. |
| **Settings** (`app/settings.tsx`, modal) | Theme (system/dark/light), notifications + **test notification with sound**, computer address connect, Privacy (AI data sharing consent, privacy policy, help & support), and **Delete account** (`DELETE /api/auth/account`). |
| **Auth** (`app/auth/[session].tsx`, modal) | Approve/deny a desktop session request from the phone. |
| **Entity workspace** (`app/entity/[id].tsx`) | Per-entity view: overview, notes, files, missions, memory, related entities. |

## Architecture

- **Expo SDK 57 / RN 0.86 (new architecture)**, TypeScript strict, expo-router.
- **Design system**: `src/theme/tokens.ts` — the desktop's warm brown-black surfaces
  (`sveltfront/src/app.css`) + single copper accent, dark default, full light mode. All
  components consume `useTheme()`; the always-dark Entities sky uses the `sky` tokens.
- **State**: zustand (`src/store/appStore.ts`).
- **API layer** (`src/api/`): **real gateway first**. Fixtures answer only in demo
  mode (the built-in default URL — nothing paired yet), and the UI then says "Showing
  sample data" (`DemoNotice`). A paired gateway's failures propagate to honest
  empty/error states; a live empty list is shown as empty. Mappers in `endpoints.ts`
  match the gateway's real shapes — mostly camelCase, but `Task` and `RawCapture`
  are snake_case.
- **Brain map** (`src/components/BrainMap.tsx`): hybrid rendering. Skia paints the
  animated sky (SkSL fragment shader), edges, and lit-sphere planets; **touch stays
  native** (invisible Pressables + RNGH hold-drag), so gestures never depend on canvas
  hit-testing. No Skia objects are ever created inside worklets.
- **Notifications** (`src/lib/notifications.ts`, `src/lib/companion.ts`): foreground
  feed poll + opt-in push. The notifiable kinds mirror `notify.rs::is_notifiable`;
  each banner carries `feedEventId`, so an event that arrives by push and by poll is
  shown once.
- **Logging** (`src/lib/logger.ts`): in-memory ring buffer feeding the Logs screen.
- **AI consent** (`src/lib/aiConsent.ts`, `src/components/AiConsentSheet.tsx`): chat
  send, entity "Ask about…", and both voice entry points await `ensure()` before
  anything leaves the phone for an AI (App Review 5.1.2(i)); revocable in Settings →
  Privacy. Stored in AsyncStorage, so a reinstall asks again.
- **Store pack**: the App Store submission notes, privacy answers and review notes
  are kept by the maintainer, outside this repository.

## Running it

Requires the **custom dev client** (Skia et al. — Expo Go will not work).

```bash
cd mobile
npm install                      # see note below if EACCES on ~/.npm
npx expo run:ios                 # first time only: builds + installs the dev client

# Day-to-day: Metro on 8088 (port 8081 is used by another project on this machine)
npx expo start --dev-client --port 8088
```

Then open the app; if the dev launcher shows, load
`exp+consistent-world://expo-development-client/?url=http%3A%2F%2F127.0.0.1%3A8088`
(e.g. via `xcrun simctl openurl booted "<url>"`).

- Deep links: `consistentworld://brain`, `consistentworld://profile`, etc.
- Gateway: defaults to `http://127.0.0.1:18800` (demo mode); pair or change it in
  Settings. In demo mode the app runs on sample data and says so.
- Pairing: Settings → Scan to connect, against the QR from the dashboard
  (Settings → Your phone) or `consistent-world pair`. A QR with a `nonce`
  pairs by key with no password (`app/pair.tsx`, `src/lib/pairingKey.ts`,
  protocol in `crates/gateway/src/pairing.rs`). `expo-crypto` is a native
  module: rebuild the dev client (`npx expo run:ios`) after pulling this.
- Building without a paid Apple account: `npx expo run:ios --device` signs with
  a free personal team (the build expires after 7 days; rebuild to renew).
  Android: `npx expo run:android`.
- npm EACCES on `~/.npm`: pass `--cache /tmp/npm-cache` (root-owned cache on this machine).

## Layout

```
mobile/
  app/                 expo-router routes (tabs + modals above)
  src/api/             client (real-first + fallback), endpoints, types, mock fixtures
  src/components/      design system + BrainMap, MessageBubble, ApprovalCard, …
  src/components/companion/  RecordingStrip, EntityPickerSheet
  src/lib/             color, haptics, logger, notifications, responsive
  src/store/           zustand app store
  src/theme/           tokens + ThemeProvider
```

Known gaps: voice transcription is mocked (gateway `/api/voice/session` exists for
later); entity-workspace detail aggregation is mock until the gateway exposes a
workspace endpoint; iPad sidebar layout exists but is lightly tested.

## Building & submitting (EAS)

Builds are managed by [EAS Build](https://docs.expo.dev/build/introduction/); the
project is already registered (`app.json` → `extra.eas.projectId`, `owner: "devbaqshi"`).
`ios/` and `android/` are gitignored — every build regenerates the native project
fresh from `app.json` (Continuous Native Generation), so app.json is the only file
that needs editing for identifiers, permissions, icons, etc.

```bash
cd mobile
npm install -g eas-cli   # or: npx eas-cli <command>
eas login                # account: devbaqshi
```

### iOS

Prerequisites: an active Apple Developer Program membership, and an App Store
Connect app record for bundle id `com.devbaqshi.consistent-world` (EAS can create
it for you on first submit if none exists).

```bash
eas build --profile production --platform ios
# First run: choose "Let EAS handle it" when asked about credentials — it
# generates/stores the distribution certificate + provisioning profile for you.

eas submit --platform ios --latest
# Prompts for Apple ID, App Store Connect app, and team the first time; answers
# are then saved into mobile/eas.json under submit.production.ios
# ({"appleId": "...", "ascAppId": "...", "appleTeamId": "..."}) for future runs.
```

Once in App Store Connect, add internal testers to skip beta review and get the
build on TestFlight immediately.

### Android

Prerequisites: a Google Play Console developer account, an app entry for package
`com.devbaqshi.consistentworld` (`app.json` → `android.package`), and a Google
Play service account JSON key with API access to that app
([setup guide](https://docs.expo.dev/submit/android/#generate-a-google-service-account-key)).

```bash
eas build --profile production --platform android
# First run: choose "Let EAS handle it" to generate/store the upload keystore.

eas submit --platform android --latest
```

`eas submit` for Android needs the service account key path in `mobile/eas.json`:

```json
{
  "submit": {
    "production": {
      "android": {
        "serviceAccountKeyPath": "./google-service-account.json",
        "track": "internal"
      }
    }
  }
}
```

Keep the key file out of git (add it to `.gitignore`) — store the real file
locally or pass it via `EAS_SUBMIT_ANDROID_SERVICE_ACCOUNT_KEY_PATH` in CI.

### Both platforms in one shot

```bash
eas build --profile production --platform all
eas submit --platform all --latest
```
