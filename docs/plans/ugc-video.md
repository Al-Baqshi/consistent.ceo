# Plan — UGC video inside Consistent CEO

> **Written:** 2026-11-15 · `main` @ `384ff44`. Decision by the owner on
> 2026-11-15: video making becomes a feature of Consistent CEO, not a separate
> hosted product. The idea engine already exists as the private
> `Al-Baqshi/ugc` repo ("CreativeUGC"); this plan brings it in.
> Status of each step lives in the checklist at the end. A fresh session
> should be able to pick this up from this file alone.

## Why here

CreativeUGC's hardest input is context: who the audience is, what they feel,
the brand's voice, what is true about the product. Consistent CEO already holds
that per entity (profile, goals, notes, missions, memory). So "tell it the idea"
needs no cold start. Distribution, billing and approvals also already exist.
A standalone "type and get videos" site would be the same engine with a weaker
funnel and the same costs; build it second, if at all.

## What ships, in order

### 1. Built-in skill `ugc-video` ✅ (this change)

`crates/gateway/skills/ugc-video/SKILL.md`, registered in `skills.rs`. The
whole CreativeUGC method (venture card → idea test → five hooks → agreed
concept → audio-first script → produce → learn), written against the gateway's
chat tools (`recall_recent`, `create_note`, `create_mission`, `create_task`,
`update_entity`, `set_reminder`, `shell`). Works in two modes:

- **Thinking only** (any machine): steps 0–4, plus a production bundle note.
- **Full** (CreativeUGC toolkit installed, `CREATIVEUGC_DIR`): renders the
  motion page, voices one take, re-times the picture to the words, muxes.

Unattended `claude` runs read the same file by path, so a mission task
"make video <slug>" follows it without chat.

### 2. Voice provider layer in CreativeUGC (ElevenLabs or VoiceStudio)

In `Al-Baqshi/ugc`: `tools/voice.py` (one call → `take.mp3` + `words.json`
whichever provider), `tools/vs.py` (VoiceStudio helper beside `tools/el.py`),
`provider` field in `tools/voices.json`, `.mcp.json` entry for VoiceStudio's
MCP, `voice.py doctor` that says which provider is live. ElevenLabs path is
unchanged (`/with-timestamps` on `eleven_v3`); VoiceStudio path is
`POST :3900/v1/audio/speech` then `POST :3900/v1/audio/transcriptions`
(`verbose_json`, `timestamp_granularities[]=word`) for the timings.
Assessment and limits: `ugc/research/2026-10-20-voicestudio-integration.md`
(default VoiceStudio model is non-commercial; permissive engines for ads;
no NZ accent by design, clone a consenting Kiwi; Apple Silicon only).

### 3. Linux-ready toolkit

The four Swift/AVFoundation tools (`mux`, `tempo`, `slice`, `motion/encode`
+ `extract`/`frames`/`sheet`/`preview`) get ffmpeg equivalents behind the
same CLI so the toolkit runs where the gateway runs (Linux server, Docker),
not only on a Mac. Headless Chrome for the motion render is already
cross-platform.

### 4. Toolkit as a Consistent CEO install

`CREATIVEUGC_DIR` setting; an installer step (`install.sh` / onboarding) that
clones the toolkit and installs Node 20+, Python 3, ffmpeg, Chrome. The skill
checks for it and degrades to bundle mode when absent.

### 5. Approval in the product

The concept step (hooks → pick → paragraph → yes) becomes a normal approval
item (`approvals` surface, mobile too). Render and voice only run after it.
Video outputs attach to the mission as evidence (preview + contact sheet),
with the editable master (HTML motion page) named.

### 6. Monetisation

Inside Consistent CEO: per rendered video or a tier. Voice costs pass
through: the user's own ElevenLabs key, or a local VoiceStudio. If a hosted
pool is ever offered, run VoiceStudio **unmodified as a separate service** or
buy its commercial licence (AGPL-3.0: modifying and serving it to others
triggers source sharing). Nothing from VoiceStudio's default model goes in a
paid ad.

## Repos

- `Al-Baqshi/consistent.ceo` — **the main repo.** Public since 2026-10-11;
  private work from 14 Oct to 7 Nov imported in `924c6da`; 0.3.0 released.
- `Al-Baqshi/Consistent.world` — the private predecessor. Diff on 2026-11-15:
  only private-only files remain there (`docs/strategy/` GTM research,
  `mobile/store/` listing and review notes, Higgsfield Claude skills,
  `.agents/`, `.vscode/`). Code is at or behind `consistent.ceo`; two small
  leftovers worth a glance: macOS CI matrix entries (deliberately dropped in
  `.ceo`, see the comment in `ci.yml`) and tenant-name labels in
  `sveltfront/src/routes/+page.svelte`. Keep `.world` as the private archive;
  do new work here.
- `Al-Baqshi/consistent-ceo-room` — the investor data room (Astro, Vercel).
- `Al-Baqshi/ugc` — CreativeUGC: ideas, ventures, projects, motion kit, voice
  tools, the private Studio. Stays the engine; this repo is the first client.

## Checklist

- [x] 1. `ugc-video` built-in skill (2026-11-15)
- [ ] 2. Voice provider layer in `ugc` (`tools/voice.py`, `tools/vs.py`,
      `voices.json` provider field, `.mcp.json`, doctor)
- [ ] 3. ffmpeg equivalents of the Swift tools
- [ ] 4. `CREATIVEUGC_DIR` + installer step
- [ ] 5. Approval item + evidence attachment
- [ ] 6. Pricing
