# Consistent CEO — How it works, for the person using it

> Plain language. No setup steps, no code. If you want the engineering view,
> read [`technical-flow.md`](technical-flow.md).
>
> Everything below describes what the app does **today**. Things that are not
> built yet are in their own section at the end, marked clearly, so nobody is
> promised something that isn't there.

---

## What it is, in one line

A private assistant that lives on your own computer, remembers your businesses
and projects, and quietly pushes them forward — instead of forgetting
everything the moment you close the window.

**Your data never leaves your machine.** There is no account to create with us,
no server holding your notes, and it works on a plane.

---

## The first minute

```mermaid
flowchart LR
    A["Download the app"] --> B["Double-click"]
    B --> C["It opens.<br/>You're already signed in."]
    C --> D["Start talking"]
```

You are signed in because you are sitting at your own computer. There is no
form, no password to invent, no email to confirm.

Later, if you want the phone app or another browser to reach the same
workspace, **Settings → Your account** turns it into a proper account with a
username and password. It's the same account, so everything you've already done
comes with it.

---

## The six things you can do right now

```mermaid
flowchart TD
    you(["You, talking to it"])

    you --> e1["Create an entity<br/><i>a business, project, or goal</i>"]
    you --> e2["Just talk<br/><i>think out loud, ask, decide</i>"]
    you --> e3["Make folders on your Mac"]
    you --> e4["Keep a record of progress"]
    you --> e5["Get reminded"]
    you --> e6["Put a note on your Desktop"]

    e1 --> hub["The entity becomes a hub:<br/>its own notes, tasks, missions,<br/>and a heartbeat that keeps it moving"]
    e3 --> folder["~/Consistent/whatever-you-asked-for"]
    e4 --> record["Tasks, missions and notes<br/>you can look back on"]
    e5 --> ping["A real notification,<br/>even with the app closed"]
    e6 --> desk["A Markdown file on your Desktop<br/>you can open in anything"]
```

### What each one feels like

| You say | What happens |
|---|---|
| *"Set up an entity for my kitchen business"* | It creates the hub, seeds its profile from what you told it, and gives it a heartbeat |
| *"Remind me to chase the invoice at 4pm"* | At 4pm your Mac shows a notification — the app doesn't need to be open |
| *"Make me a folder for the Acme job"* | `~/Consistent/acme` appears |
| *"Put a note on my desktop with the three things I decided"* | A `.md` file lands on your Desktop |
| *"What was I working on?"* | It recalls recent work, notes and lessons |

---

## Where your things are kept

```mermaid
flowchart LR
    subgraph yours["On your computer, and nowhere else"]
        app["Inside the app<br/>entities, missions, tasks,<br/>notes, memories"]
        desk["~/Desktop<br/>notes you asked it to leave out"]
        proj["~/Consistent<br/>folders and files it made"]
        vault["Encrypted vault<br/>passwords and keys"]
    end
```

Two folders are the **only** places the assistant can write, and it cannot be
persuaded otherwise — not by you, and not by anything it reads on the web. If
it tries to write anywhere else, the attempt is refused rather than obeyed.

You can move those two folders in settings. You cannot widen them to your whole
computer, on purpose.

---

## How reminders reach you

```mermaid
sequenceDiagram
    participant You
    participant App as Consistent
    participant Mac as Your Mac

    You->>App: "remind me at 4pm"
    App->>App: keeps it, checks the clock
    Note over App: 4pm
    App->>Mac: notification
    Mac-->>You: banner, even if the window is closed
```

The phone app shows the same things while it's open, and can approve a sign-in
on your laptop.

---

## What it does *not* do yet

Being straight about this, because the gap between the pitch and the product is
where trust goes to die.

```mermaid
flowchart TD
    subgraph now["Works today"]
        n1["Entities, notes, tasks, missions"]
        n2["Talking, with real actions taken"]
        n3["Folders and Desktop notes"]
        n4["Reminders and notifications"]
        n5["Password vault"]
    end

    subgraph soon["Not yet"]
        s1["Checking its own claims against your workspace<br/>before it answers"]
        s2["Doing longer jobs on its own<br/>editing files, running commands"]
        s3["Windows and Linux versions"]
        s4["Installing without a security warning"]
        s5["Paying for anything in-app"]
    end
```

The honest headlines:

- **It can be confident and wrong.** The design calls for every meaningful claim
  to be checked against your actual workspace before you see it. That checking
  exists but is not switched on yet, so treat what it tells you the way you'd
  treat a sharp colleague talking from memory.
- **It talks and records; it doesn't yet go away and *do* long jobs.** It can
  create a folder or leave a note on request. It cannot yet be handed a
  multi-step job and left alone with it.
- **macOS only for now**, and until the app is signed, macOS will warn you when
  you open it.

---

## It needs a brain

The assistant does the thinking through a model. On your machine that is the
`claude` command-line tool, using your existing Claude subscription — no API
keys, no per-message billing from us.

```mermaid
flowchart LR
    app["Consistent"] --> q{"Which brain?"}
    q -->|"preferred"| cli["claude CLI<br/>your subscription"]
    q -->|"fallback"| ollama["Ollama<br/>fully offline"]
    q -->|"optional"| keys["Your own API key"]
```

If none of those is installed, the app runs but has nothing to think with — the
notes, tasks and reminders still work, but conversation won't.

---

## The shape of a week

```mermaid
flowchart LR
    mon["Tell it what you're working on"] --> tue["It keeps the record"]
    tue --> wed["It reminds you"]
    wed --> thu["You talk, it captures"]
    thu --> fri["You look back<br/>and can actually see the week"]
    fri --> mon
```

That loop — say it, keep it, be reminded, look back — is the whole product. Everything
else is in service of it.
