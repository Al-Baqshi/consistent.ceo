---
name: ugc-video
description: Turn an idea into a short UGC-style video for the active venture — idea test, five hooks, an agreed concept, a spoken script, a timed voice-over, and the rendered cuts — using what the entity already knows about its audience and voice. Use when the user says "make a video", "UGC", "reel", "TikTok", "short ad", "voice this", drops a video idea, or asks for content for a product, service or launch.
---

# UGC video

The idea is the product; tools only render it. A good UGC video is one true,
specific moment the audience recognises in two seconds, a twist that makes it
worth sharing, and the product as the payoff, not a sticker at the end. Spend
the effort on the idea and the hook. Never render or spend voice credits before
the user has said yes to a concept.

The procedure works in two modes. **Everywhere:** the thinking steps (0–4)
need only chat and notes. **With the CreativeUGC toolkit installed** on this
machine (`CREATIVEUGC_DIR`, default `~/CreativeUGC`), steps 5–6 render the
video and lay the voice on it through `shell`. Without it, step 5 hands back a
complete production bundle a video maker or another agent can finish.

## 0. Load the venture

1. Read the active entity: profile, goals, audience, tone, and
   `recall_recent` for recent notes, posts, learnings and past videos.
2. Build the **venture card** from it and show it in four lines: *audience*
   (who exactly, on which platform), *pain* (the one thing they feel),
   *voice and humour limits* (what we joke about, what we never touch),
   *CTA* (the single action we want). If a line is missing, **ask**; never
   invent an audience. Save confirmed facts with `update_entity`.
3. Standing rules for every venture: humour punches at the situation or at
   ourselves, never at the customer. Real UGC looks unpolished on purpose
   (phone framing, natural light, real speech rhythm). AI people never
   imitate real, identifiable people or pose as real customers. Every claim
   about the product must be true today; when unsure, ask or leave it out.

## 1. Sharpen the idea

From the user's idea (or from "brainstorm", 8–10 one-liners), write:

- **Truth** — the specific moment the audience lives ("you open the fridge
  four times hoping food appeared"), not a category ("mornings are hard").
- **Twist** — where the laugh or surprise comes from.
- **Target** — who, which platform, what they do after watching.
- **Job** — organic (awareness, saves, shares) or paid (sign-ups, sales).
  Paid videos follow the ad anatomy: hook, product intro, problem to
  solution, value, proof only if real, explicit CTA.

Run the **idea test**, 1–5 each, out of 20: *Recognisable* (would the target
say "that's literally me" in two seconds?), *Specific* (a concrete detail?),
*Ownable* (could only this venture post it?), *Shareable* (would they send it
to a friend, and why?). Under 14 → rework or kill, and say which line failed.
Save the idea, score and verdict as a note with `create_note` titled
`Idea: <slug>` so killed ideas are kept too.

## 2. Five hooks

Write five hooks, each on a **different mechanism**: call-out ("if you …,
this is for you"), pattern break (something wrong in frame one), mid-action
start, contrarian ("stop doing X"), confession, question with stakes, visual
promise (end result first), dialogue cold-open. For each, spell the layers:
on-screen text, spoken line, what we see, sound. Mark your pick and why.
**Stop and ask the user to choose.**

## 3. Concept and agreement

One paragraph: format (talking head, POV skit, two-character skit, before and
after, green-screen reaction, listicle, day-in-the-life, text-on-screen meme),
character, beat-by-beat arc, the laugh, the product moment, the CTA. Length:
organic 15–30 s, paid 30–40 s, at about 2.5 spoken words per second (30 s ≈
70 words). Vertical 9:16 first; 4:5 and 16:9 as extra cuts if asked.
**Stop and confirm.** No script, no render, no voice credits before a yes.
On yes: `create_mission` "<Venture>: video <slug>" with tasks for script,
render, voice and review, so the work is tracked like any other mission.

## 4. Script (audio-first)

The voice carries the video and the picture follows the words:

1. **Continuous speech.** A real hook in the first two seconds (bold claim,
   stinging number, unanswerable question, or the funniest line first), then
   natural talk for the whole video. No silence over one second. Captions
   are phrases lifted from the narration, never a separate caption script.
2. **Written for the mouth:** contractions, short lines, the venture's accent
   and idiom. Read it at speaking pace in your head and cut until it fits.
3. **Pronunciation map:** list every brand name, URL and acronym with how it
   must be spoken (e.g. a URL as "dot co dot N Z"). Keep it with the script.
4. **Beat marks** `{h1}`, `{h2}`, … right before the words each picture beat
   belongs to. Beats are what the motion page re-times itself to.
5. Tag each beat with its role: hook, intro, problem, solution, value, proof,
   CTA. Then run the humour checks: is the joke still funny on the third
   read, and does the product moment land without an ad-read?

Save the script as a note `Script: <slug>` and, when the toolkit is
installed, as `script.txt` in the project folder (step 5).

## 5. Produce

**With the toolkit** (`test -d "$CREATIVEUGC_DIR"`), via `shell`, in order:

1. Create `projects/YYYY-MM-DD-<venture>-<slug>/` from `projects/_template/`;
   write `post.md` (idea, hooks, concept, captions, status), `script.txt`
   with the beat marks, and `vo-script.json` per `tools/VO_SCRIPT.md`.
2. Build the motion page from `tools/motion/template.html` (kinetic type,
   product screenshots, the venture's colours and logo from the entity's
   brand files). Render a silent master with `node tools/motion/make.mjs`.
3. Voice: `python3 tools/voice.py doctor` shows which provider is live
   (ElevenLabs with a paid key, or a local VoiceStudio backend). Generate
   **one unbroken take** with `python3 tools/voice.py take <project> --voice
   <name from tools/voices.json>`; it writes `take.mp3` and `words.json`
   (word timings) whichever provider ran. Check the transcript it prints
   against the pronunciation map; fix only the wrong line, never the whole
   take.
4. `python3 tools/cue_take.py times <out_dir>` turns the words into beat
   times; re-render the motion page to them; lay the voice on with
   `tools/mux` (voice ≈ 1.15 gain, any music bed ≈ 0.09 ducked to 0.03).
5. Export the cuts, a preview and a contact sheet into `outputs/`, then show
   the user the preview and name the editable master (the HTML page), not
   just the MP4.

**Without the toolkit**, deliver a production bundle as one note
`Bundle: <slug>`: venture card, concept, the marked script, pronunciation
map, `vo-script.json` contents, a shot list (duration, framing, action,
spoken line, which tool renders it) and ready-to-paste prompts per tool
(image, video, voice, edit) with negative guidance ("no studio lighting, no
cinematic grade") so it still looks like real UGC. Offer to `create_task`
for a video maker.

## 6. Review and learn

Before calling it done: hook lands in under three seconds with the sound
off? Product clear without an ad read? Every claim true? CTA explicit? Paid:
all anatomy parts present? Then `set_reminder` to ask how it performed, and
when the user reports, `create_note` `Learning: <slug>` with what worked,
what flopped and why, so the next idea starts smarter.

## Voice providers

Voice is pluggable; the script and the pipeline do not change:

- **ElevenLabs** (paid plan): library voices per venture, expressive tags,
  word timestamps built in. The usual choice for a brand's standing voice.
- **VoiceStudio** (local, open source, on this machine): no character
  budget, the founder's own cloned voice from a 30–60 s phone recording,
  word timings from its local transcription. Its default model is
  non-commercial; a paid ad needs one of its permissively licensed engines.
  Clone only the user or someone who has agreed on record.

Pick one voice per venture and keep it; swap only when the user says so.

## Rules

- Agree before spending: concept yes before script, script yes before render
  or voice credits.
- Never write "free", "no sign-up" or other generous pricing lines unless the
  entity's CTA rule allows them; push the venture's CTA.
- One unbroken take; never generate line by line or slice a take into lines.
  Rejected for good: deadpan reads, caption-fragment scripts, letter or
  alphabet gimmicks on screen.
- Never print an API key. Never send anything outward (post, DM, upload to a
  channel) yourself; hand the files back and let the user publish.
- Save as you go: idea, hooks chosen, concept, script, learnings are notes
  on the entity, so a new session can continue from them alone.
