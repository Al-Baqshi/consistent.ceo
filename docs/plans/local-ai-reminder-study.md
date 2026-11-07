# Study: Consistent as a local-AI personal reminder

_October 2026._ This study answers: can Consistent's core product (capture
anything → get reminded → get followed up until it's done) run on a local
model, on a phone, on a 16 GB laptop, and on the founder's 2019 Intel MacBook?
What do we build first?

## 1. The core product, stated precisely

A user drops anything into Consistent: "take a photo and send it to ABT",
"call mum Sunday", "renew the domain before it lapses". Consistent must:

1. **Capture** it as a durable item linked to the Personal entity or a
   business entity, with a due time if one is stated or implied.
2. **Remind** at the right moment, on the device in the user's hand, even
   when the app is closed and the computer is asleep.
3. **Follow up** until it is done: snooze, re-nudge, escalate, or ask
   "still relevant?". It never fires once and forgets.
4. **Proactively surface** open loops ("you still owe ABT that photo") on a
   steady rhythm (hourly when something is due, otherwise quiet).
5. **Act on the computer** when the item is computer work (send the file,
   open the doc), using the gateway's local tools.

Key design rule: **the AI is not the alarm clock.** Firing a reminder on time
must be deterministic code plus OS notifications, so it works with no model,
no network and a dead battery-saver. The model does the judgement work:
- understand the note and pick the time;
- write the nudge;
- decide whether to follow up, and how.

## Status (19 October 2026): phases 1, 2 and 4 built, phase 3 ready to run

- **Reminder engine** (`crates/gateway/src/reminders.rs`):
  - Nothing auto-completes any more; reminders follow up an hour later, then
    the next morning, then ask "still relevant?" the morning after, then
    park.
  - Notes and tasks can carry a follow-up time (`remind_when`).
  - Weekday phrases like "on Friday at 2pm" now parse.
  - The task runner no longer picks up reminders or notes.
  - Follow-ups wait out quiet hours.
- **API:** `GET/POST /api/reminders`, `POST /api/reminders/{id}/action`,
  `POST /api/reminders/local-scheduled`, `GET /api/local-model`.
- **Hourly check-in:** a `loops` beat on every Personal Space. It stays
  silent unless the open-items list changed, and the model may answer `SKIP`.
- **Chat:**
  - Rule 7: everything you need to do gets a time; tomorrow 9am by default.
  - A guard re-asks the model when it claims an action no tool performed.
- **Phone** (`mobile/`):
  - Reminders and their follow-ups are scheduled as local notifications,
    with Done / Snooze 1h / Tomorrow buttons and an offline queue.
  - A Reminders section sits on Today.
  - "Consistent AI (offline)" has a download button (3.1 GB, Gemma 4 E2B
    GGUF) and an on-device agent via llama.rn for when the computer is
    unreachable.
- **Desktop and web:**
  - Reminders panel; the "Schedule Reminder" button now creates reminders
    that fire.
  - A "Download Consistent AI" button (Ollama `gemma4:e2b`).
  - Catch-up notifications on launch, and a "Reminders (N due)" tray item.
- **Model:** the training pipeline covers check-ins (including when to stay
  silent), note → follow-up, replies to nudges, and the phone's offline
  prompt. It needs a GPU session to run (`training/README.md`).

## Status (20 October 2026): tuned and extended

Benchmarked stock **Gemma 4 E2B**, which is about 5× faster than E4B on the
Intel Mac (around 30 s per answer instead of about 2.5 min). Then tuned the
system around it, with no GPU yet:

| Benchmark (E2B) | v1 | v2 (prompt + code) |
|---|---|---|
| Heartbeats | 82% | 91% |
| Chat | 80% | 88% |
| Phone, offline | 100% | 100% |

- **Code decides when to speak.** The check-in only calls the model when
  something needs the owner this hour, using a short persona (system prompt
  about 3k → 1.1k characters).
- **Reminder reflex.** Short replies to a nudge are applied without the
  model: instant, and they can't be faked.
- **Do it for me.** A reminder can be handed to the computer as an
  approval-gated task; it closes itself when the task finishes, or comes
  back with the reason if it fails.
- **Learned rhythm.** Follow-ups land at the hour the owner actually marks
  things done.
- **Capture from anywhere.** `consistentworld://remind?text=…` (Siri
  Shortcut recipe in `docs/REMINDERS.md`) and `POST /api/reminders {text}`.
- **Learning from outcomes.** Check-ins that led to action become training
  data (`traces_to_sft --learn-from-outcomes`).

The remaining E2B failure is chat replies to nudges ("done, sent it")
without a tool call. The reflex covers this in production; fine-tuning
covers it in the model.

## 2. What worked before this round (code audit)

| Step | Status | Notes |
|---|---|---|
| Chat "remind me in 2 hours to…" → reminder | ✅ Works | `set_reminder` (`chat_tools.rs`). Plain Gemma 4 sometimes *says* it set one without calling the tool (3/8 in our benchmark). |
| Reminder fires on time | ✅ Works | `ReminderTicker` polls every 20s (`reminders.rs`). |
| You see it on the Mac | ✅ Works (app open) | Desktop app polls every 5s and shows an OS notification. Nothing that fired before launch is shown. |
| You see it on the iPhone | ⚠️ Only while the app is open | Expo push exists but is **off** (`[notifications] mobile_push` is unset in your config). The phone doesn't schedule local notifications itself. |
| Done / Snooze from the notification | ❌ Missing | No action buttons on any platform. |
| Follow-up if ignored | ❌ Missing | A fired reminder is auto-marked **Completed**, so nothing ever re-nudges. |
| Note with no time → reminder | ❌ Missing | `create_note` / `create_task` have no due time. "Take a photo, send it to ABT" becomes a silent note. |
| Web "Schedule Reminder" button | ❌ Bug | Saves with no due time, so it **never fires**. |
| REST snooze of a reminder | ❌ Bug | 409 after firing; before firing it doesn't move the real fire time (`metadata.due_at`). |
| Recurring reminders | ❌ Missing | Only heartbeats recur (cron, run an LLM). |
| Proactive check-ins | ⚠️ Daily/weekly only | Pulse daily, review and ideas weekly. No hourly "open loops" beat. |
| Quiet hours | ⚠️ Lossy | A push muted by quiet hours is never sent later. |
| Local model as the agent | ✅ Fixed this round | The Ollama adapter now sends and receives tool calls. |

**Verdict:** the plumbing is there, but the core loop is not dependable yet.
The model choice is secondary. Missed and forgotten reminders would sink the
product with any model.

## 3. Where the model can run

### 3.1 Measured / published numbers

| Device | Model | Runtime | Speed | Memory | Source |
|---|---|---|---|---|---|
| MacBook Pro 2019, Intel i9, 64 GB | Gemma 4 E4B Q4 | Ollama (CPU only; AMD GPU unused) | ~2.5 min per heartbeat answer (long prompt) | ~7 GB | our benchmark |
| iPhone 17 Pro | Gemma 4 E2B (2.6 GB) | LiteRT-LM, GPU | 57 tok/s, 0.3 s to first token | ~1.5 GB peak | Google AI Edge |
| iPhone 17 Pro | Gemma 4 E4B (3.7 GB) | LiteRT-LM, GPU | 25 tok/s | — | Google AI Edge |
| Galaxy S26 Ultra | Gemma 4 E2B | LiteRT-LM, GPU | 52 tok/s | ~0.7 GB | Google AI Edge |
| MacBook Pro M4 Max | Gemma 4 E2B | LiteRT-LM, GPU | 160 tok/s | ~1.6 GB | Google AI Edge |
| any phone | FunctionGemma 270M | LiteRT-LM | 154 tok/s (S25U CPU) | 0.3 GB | Google AI Edge |

### 3.2 What that means

- **Modern phones (iPhone 15 Pro and later, 2024+ Android flagships, 8 GB RAM):**
  Gemma 4 E2B runs comfortably. A 150-word nudge takes about 3 seconds.
  **On-phone works.**
- **Older or 6 GB phones:** E2B is marginal (memory pressure, the OS kills it in
  the background). Use FunctionGemma 270M for "note → structured reminder" on
  the phone, and leave nudge writing to the computer.
- **M1/M2 Macs with 16 GB:** E2B or E4B through Ollama/Metal, tens of tokens per
  second. Fast enough for chat too.
- **The 2019 Intel Mac:** works for background beats (minutes per answer), slow
  for chat. E2B roughly halves that. Fine as an always-on "brain" for hourly
  nudges; not where live chat should run.

### 3.3 Running on the phone only

Possible, with three hard limits:

1. **iOS won't run a model in the background on a schedule.** Background tasks
   get seconds, not minutes, and aren't guaranteed. So on iOS the phone can't
   *wake up hourly and think*. What does work offline and closed:
   - local notifications scheduled ahead of time;
   - model work done when the app is opened, or when the user taps a
     notification action.
2. **Computer actions need the computer.** "Send the photo to ABT from my
   files" runs on the gateway.
3. **Model download.** 2.6 GB for E2B, fetched on first launch over Wi-Fi.

So the right architecture is **phone-first for reminders, computer for
thinking**, not phone-only:

- **The phone** stores reminders and fires them as **OS local notifications**,
  scheduled the moment a reminder is created or synced. They work offline,
  with the app closed and the computer off. The phone also hosts a small
  on-device model for capture ("take a photo → send to ABT, tomorrow 10am")
  when the computer isn't reachable.
- **The computer** (gateway, always on, tray app) runs the hourly proactive
  beats and follow-up decisions with `consistent-gemma`, and performs
  computer actions. Its results are pushed to the phone (Expo push, or synced
  and scheduled locally on next contact).
- **Offline fallback:** if the computer is unreachable, the phone keeps every
  scheduled reminder and uses fixed follow-up rules (re-nudge after 1h,
  then the next morning) until the computer is back.

### 3.4 Runtime choice for the mobile app (Expo / React Native)

| Option | Pros | Cons |
|---|---|---|
| **llama.rn** (llama.cpp in React Native) | Uses the **same GGUF** we already export; mature; tool-call grammars; Expo dev-client friendly | No NPU; a bit slower than LiteRT on Android GPUs |
| LiteRT-LM (Kotlin stable, Swift early preview) | Fastest; Google's official Gemma 4 path; has a "convert a fine-tuned model" tutorial | Needs our own native module for RN; iOS SDK is preview |
| Apple Foundation Models (iOS 26+) | Free, already on the device, no download | Apple's model, not ours; Apple Intelligence devices only; iOS only |

**Recommendation:** start with **llama.rn + our GGUF**, so one model file
serves Mac, Windows and phone. Move Android to LiteRT-LM later if speed
matters.

## 4. The model: `consistent-gemma` (E2B)

- **Base:** Gemma 4 E2B (Apache-2.0); it fits phones and Intel Macs.
- **Training jobs** (pipeline in `training/`): the heartbeat nudge, chat with
  tools, and, new for this product:
  - (a) **note → reminder extraction**: who, what, when, entity, and a
    follow-up policy;
  - (b) **follow-up decisions**: "user ignored it twice; re-nudge, reschedule,
    or ask if it's still relevant?";
  - (c) **staying quiet** when nothing is due.
- **Export:** GGUF Q4 for Ollama and llama.rn; `.litertlm` later.
- **Cost:** one cloud GPU session (~$10–15) or free Colab plus a teacher API.

## 5. Plan

### Phase 1: a reminder you can trust (Mac + iPhone, no new model needed)
1. A fired reminder becomes **"due, awaiting you"**, not Completed. Re-nudge
   after 1h, then next morning, then ask "still relevant?" (configurable).
2. **Done / Snooze 1h / Tomorrow** actions on phone and desktop
   notifications, backed by one `POST /api/reminders/{id}/{action}`.
3. The **phone schedules local notifications** for every synced reminder, so
   reminders fire even when the app is closed and the Mac is asleep. Turn on
   Expo push for the rest.
4. **Notes become reminders:**
   - give `create_task` and `create_note` an optional due time;
   - a note with an implied deadline gets one;
   - a note with no time gets a gentle default follow-up ("tomorrow 9am:
     still need to send ABT the photo?").
5. Fix the bugs: web Schedule Reminder, REST snooze, muted pushes lost in
   quiet hours, fire event missing `task_id`, desktop missing events from
   before launch.
6. Add a chat guard that catches "I've set a reminder" when no tool ran.

### Phase 2: proactive rhythm (Mac brain)
1. An hourly **"open loops" beat** for the Personal entity. It speaks only
   when something is due, overdue or newly stuck, and is silent otherwise.
   Quiet hours are respected.
2. Point the gateway at local `gemma4:e2b` now, and at `consistent-gemma`
   once trained.

### Phase 3: our own model
Generate data (now including extraction, follow-up and silence), fine-tune
E2B, then benchmark against plain E2B. Ship to Ollama with
`ollama pull consistent/gemma`.

### Phase 4: on-phone brain
Add llama.rn to the Expo app (dev build) and download the GGUF on Wi-Fi. Use
it for capture and offline nudges when the gateway is unreachable. Gate it by
device RAM (8 GB or more → E2B; otherwise rules only, or FunctionGemma).

## 6. Risks

- **iOS background limits:** solved by scheduling local notifications ahead
  of time, not by thinking in the background.
- **Notification fatigue:** hourly beats must be silent by default, with
  quiet hours and a per-entity mute.
- **Privacy:** Expo push sends titles through exp.host. Use generic titles
  ("You have a reminder"), or local notifications only.
- **The Intel Mac is slow:** fine for background work. Live chat should
  prefer an M-series Mac, the phone, or a cloud model when online.
