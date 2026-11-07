# Plan C — Control a logged-in browser

Goal: Consistent CEO can act in browsers you are already signed in to (social
accounts, dashboards, anything with a login), under the same rules as the rest
of the product: you see what it does, outward-facing actions wait for your
approval, and it can be stopped instantly.

Status: **M1 built** (2026-10-17): the Consistent browser, per-entity sites, and the
safety changes it needed. M2 onward is still a plan. See "What M1 shipped" below.

## Recommendation

Do it in two steps, and do step one first.

1. **A dedicated "Consistent browser" profile** (a real Chrome window with its
   own profile that the app manages). You sign in to your accounts in it once;
   the agent drives that same window over the Chrome DevTools Protocol while you
   can watch. No extension, no store review, works today with the code we have.
2. **A Chrome extension** (Firefox after) only if you need it to act in your
   *everyday* browser, in the tab you are looking at. It is more work and more
   risk, so it earns its place only after step one proves the idea.

Chrome first, Firefox second. Chrome is what is installed here, and its
extension API (`chrome.debugger`) lets the extension reuse the same action
vocabulary the CDP tool already has. Firefox has no equivalent, so a Firefox
port is content-script based and a little less capable.

## Already here

| Piece | Where |
|---|---|
| CDP browser tool: navigate, click, type, text, screenshot | `crates/tools/src/browser.rs` |
| Persistent profile, headless or visible, attach to a running browser | `CONSISTENT_WORLD_BROWSER_PROFILE_DIR`, `CONSISTENT_WORLD_BROWSER_HEADLESS`, `CONSISTENT_WORLD_BROWSER_CDP_URL` |
| Domain allow/deny enforced on every request the page makes (not just the first URL) | same file; known gaps (WebSocket, popups) are documented and pinned by tests |
| Money and identity sites always denied for unattended runs | `ALWAYS_DENIED` in `crates/gateway/src/mcp_browser.rs` |
| Bridge so the `claude` CLI can use it for `actionKind: browser` tasks | `consistent-world mcp-browser`, `task_executor.rs` |
| Approvals queue for outward-facing work | `requires_human_approval`, `/api/approvals` |

## What is missing for step one

- Nothing in the UI sets any of it up; it is all environment variables.
- No default profile folder, so a login is lost unless one is configured.
- Headless is the default, which cannot be signed in to by a person.
- Allowed domains are an environment variable, not a per-entity setting.
- Chrome 136+ refuses `--remote-debugging-port` on its *default* profile, so
  attaching to your normal Chrome is not possible anyway. A dedicated profile
  is the only supported route, which is a point in its favour, not a limitation.

## What M1 shipped

- **Settings → Consistent browser** opens a visible Chrome window on its own profile
  (`~/.consistent-world/browser-profile`), shows which sites are open in it, and closes it
  politely so Chrome saves the session. The window outlives the gateway; a window left open
  is found again through the `DevToolsActivePort` file Chrome writes
  (`browser_manager.rs`, `tools/src/chrome.rs`).
- **Browser tasks drive that same window.** The executor opens it if it is closed, and the
  run's MCP server attaches to it, in its own tab. If the window cannot be opened the run
  falls back to launching its own Chrome, as before.
- **Sites are opt-in per entity** (Permissions tab; `PUT /api/entities/{id}/browser-sites`).
  Money and sign-in domains are refused when a site is added. **A browser task on an entity
  with no sites is blocked with an instruction instead of running.** That tightens what
  existed before (browser tasks ran against any non-denied site), on purpose.
- **Attaching was made safe first.** The tool used to send `Browser.close` to whatever it was
  attached to, which would have closed the signed-in window after every task; it now closes
  only its own tab. `evaluate` (arbitrary JavaScript, which could read tokens) is off whenever
  the tool is attached. Popups no longer escape the policy (a guard script plus a sweep); the
  recorded `known_hole_popup_windows_escape_the_policy` test became
  `popup_windows_cannot_escape_the_policy`.
- **Deliberate flag choices:** no `--enable-automation` (sites refuse a sign-in when they see
  it), site isolation left on (this browser holds real sessions, so a renderer boundary is
  worth more than complete iframe interception), a random loopback debugging port that exists
  only while the window is open. Chrome itself refuses DevTools connections that carry a web
  page's `Origin`, so a page in the browser cannot reach the port.

Verified against a real Chrome: attaching never closes the browser; a **server-set HttpOnly
login cookie survives closing and reopening the window and the agent works inside it**;
`evaluate`, off-list and payments sites are refused; the person's own tab is untouched when a
task ends; the cold first navigation takes under a second; the settings panel and the site
list work in the dashboard; an entity with no sites blocks its browser task.

Not exercised: a visible window on screen through the shipped app (headed Chrome was only
tested in a scratch run), signing in to a real site, and a real `claude` CLI run driving the
MCP server. Known limits: popups from a frame in its own process are outside the guard (see
`browser.rs`), and WebSocket handshakes are still not intercepted.

## Milestones

**M1 — the Consistent browser. Built, see above.**
Settings → Browser: "Open the Consistent browser" starts visible Chrome with a
profile under `~/.consistent-world/browser-profile`, with a debugging port the
gateway attaches to. Status line (open / signed-in sites seen). A per-entity
list of sites the agent may use; everything else is refused. Done when: you sign
in to one site, close the window, reopen it, and a browser task for an entity
that allows that site reads a logged-in page.

**M2 — watch, pause, take over.**
Live screenshot strip in the dashboard task view, a Pause button the agent
respects between actions, and a visible banner in the browser while it is being
driven. Every action lands in `audit_trail`. Done when: a task can be paused
mid-run and resumed, and the audit trail shows each click and typed field.

**M3 — extension, read only.**
Chrome MV3 extension plus a native-messaging host (a small mode of the
`consistent-world` binary, so no token ever lives in the extension and the
connection is locked to the extension's id). Tools: list tabs, read page text,
screenshot. Per-site grants use `optional_host_permissions`, so Chrome itself
asks you the first time a site is touched. Done when: it can read a tab you
granted and is refused one you did not.

**M4 — extension, actions, Firefox.**
Click and type, behind the same approvals and audit trail as M2. Firefox port.
Done when: a draft post is typed into a logged-in account and stops at an
approval before anything is submitted.

## Rules that do not move

- **Never touch credentials.** No reading cookies, saved passwords or tokens.
  The browser holds the login; the agent only sees pages.
- **Outward actions need approval.** Posting, sending, buying, deleting,
  changing settings: all create an approval first, like the rest of the product.
- **Sites are opt-in per entity.** Nothing is allowed by default. The money and
  identity list stays denied.
- **One kill switch.** Pause in the app, and the toolbar button in the extension,
  stop the agent between actions.
- **Pages are untrusted input.** A page can contain text that tries to give the
  agent orders. Page text is passed as data, never as instructions, and an
  action the page suggested still needs the same approval as any other.

## Risks worth saying out loud

- **Account bans.** Many platforms forbid automated activity on personal
  accounts. Prefer the platform's official API where one exists, and use the
  browser to read and to draft, with a person pressing the final button.
- **Blast radius.** A signed-in session can do everything you can. That is why
  the allow-list is per entity and why posting always waits for you.
- **Known holes in the policy layer** (WebSocket handshakes, popups) are listed
  in `browser.rs`. They matter more once real logins are involved, so M1 should
  close the popup one before it ships.

## Decisions needed from you

1. Which sites first? (Name two or three; it decides what to test on.)
2. Comfortable signing in once inside a separate Consistent browser window?
3. Chrome only for now, Firefox later?
4. Extension distribution: load-unpacked for developers (simple, open source),
   or the Chrome Web Store (review, but one click for everyone)?
