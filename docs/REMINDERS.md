# Reminders

Consistent is a reminder that keeps going until something is done.

## How a reminder behaves

1. **It fires at the time you gave.** If you gave no time, it fires tomorrow
   at 9am, and Consistent says so.
2. **It follows up until you answer:**
   - an hour later;
   - the next morning, at the hour you usually get things done (learned
     from when you mark reminders done);
   - a "still relevant?" the morning after.

   Then it parks. Parked items come back in the hourly check-in.
3. **Only you end it.** Answer from the notification (Done, Snooze 1h,
   Tomorrow), the Reminders list, or chat ("done", "push it to tomorrow",
   "drop it"). A short chat reply to a reminder Consistent just mentioned is
   applied instantly, without waiting for the AI.
4. **Do it for me.** Computer work ("send ABT the invoice PDF") can be
   handed to your computer. It waits for your approval, runs on its own, and
   the reminder closes itself when the task finishes. If the task fails, the
   reminder comes back and tells you why.

The phone schedules every reminder and its follow-ups as local
notifications. Reminders still ring when the app is closed, the phone is
offline, or the computer is asleep.

## Hourly check-in

Each Personal space has an "Hourly check-in" heartbeat. It looks at what is
nudged, parked, overdue, or due within the hour. It speaks only when that
list has changed, and it stays silent during quiet hours. You can pause it
like any heartbeat. If you delete it, it comes back at the next gateway
start, so pause it instead.

## Capture from anywhere

`consistentworld://remind?text=<sentence>` saves one sentence as a
reminder. The time is read from the sentence, for example
`consistentworld://remind?text=send%20ABT%20the%20photo%20tomorrow%20at%209am`.

**Siri Shortcut** (iPhone):
1. In the Shortcuts app, create a shortcut named "Remind me".
2. Add **Dictate Text**.
3. Add **URL** with the text
   `consistentworld://remind?text=` followed by the *Dictated Text* variable.
4. Add **Open URLs**.

Say "Hey Siri, remind me", then speak the reminder. It works offline, and
the reminder reaches the computer once it's back.

**From anything else** (scripts, Raycast, Alfred, Stream Deck), call the
gateway directly:

```sh
curl -X POST http://127.0.0.1:<port>/api/reminders \
  -H "Authorization: Bearer <token>" -H "content-type: application/json" \
  -d '{"text":"call mum in 2 hours","tzOffsetMinutes":120}'
```

## API

| Endpoint | What it does |
|---|---|
| `GET /api/reminders?include=open\|all` | Open reminders, each with its next nudge and planned follow-ups |
| `POST /api/reminders` | `{what, when?, inMinutes?, fireAt?, entityId?, clientId?, tzOffsetMinutes?}`, or `{text}` |
| `POST /api/reminders/{id}/action` | `done` · `cancel` · `snooze` (`minutes`, default 60) · `reschedule` (`when`/`fireAt`) · `reopen` · `delegate` |
| `POST /api/reminders/local-scheduled` | The phone reports which fire times it scheduled itself, so the gateway skips the duplicate push |
| `GET /api/local-model` | Which offline model the download buttons fetch |
