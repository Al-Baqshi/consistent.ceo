---
name: meeting-to-actions
description: Turn raw meeting notes, a call transcript, or a brain-dump into decisions, owners, and tasks filed under the right venture. Use when the user pastes notes or a transcript, says "here's what we discussed", "action items from…", "notes from my call with…", or asks what came out of a meeting.
---

# Meeting to actions

Meetings produce two durable things: **decisions** and **commitments**. Everything
else is context. Extract the two, file them where they will be seen again, and
keep the context short.

## Extract

From the notes, pull out:

1. **Decisions** — things that were settled. Phrase each as a statement:
   "We will launch the beta on the 14th."
2. **Commitments** — who will do what by when. If the owner or date is missing,
   mark it `owner?` / `date?` rather than guessing.
3. **Open questions** — things raised but not settled.
4. **Context worth keeping** — 1-3 lines of facts learned (a customer's budget, a
   partner's constraint). Skip pleasantries and repetition.

## Route to the right venture

The user runs several ventures, and one meeting can touch more than one. Assign
each item to a venture using names, products, people, and domains in the notes;
call `list_entities` if unsure. Ask only when an item is genuinely ambiguous.

## Show, then file

Present the extraction grouped by venture. Then ask whether to file it. On a yes:

- `create_task` for each commitment the user owns or wants tracked (with the
  due date when one was stated).
- `create_note` for decisions and kept context, under the right venture.
- `set_reminder` for any follow-up the user owes someone else by a date.

## Rules

- Don't invent owners or dates.
- Don't create tasks for other people's commitments unless the user wants to
  track them — ask.
- Quote numbers and names exactly as they appear in the notes.
