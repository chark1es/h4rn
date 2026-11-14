# UI fidelity review, round 4: h4rn Quiet native at `dev` 89bf407 (mock mode)

**Score: 8.1 / 10** (r3: 6.8, r2: 8.0).

I re-measured every r3 P0 and P1 fix in the browser, and they hold, except one P1 (the 560 px floor) that now hides the panel without telling the user. The new backends render correctly on desktop: pairing code and scopes, Fail over…, the PR Commits tab, required checks, View log and Re-run failed, the MCP Resolve… popover, diffstat and match counts on tool rows, and the host footer copy. The desktop thread, sidebar, IDE, right panel, PR detail and MCP page now read close to the prototype.

What keeps the score down:
- **One new P0.** The phone auto-resume card has no padding, so **Resume now is cut off at 360 px**.
- **Copy from the new backends that nobody designed.** The Fail over… menu shows backticks, a credential filename and the same reason four times. Sign-in shows "Failed to fetch". The code-entry screen says "Pair with atlas" before it knows which host the code is for.
- **14 native `<select>`s.** Providers (4) and New thread (3) are the most visible.
- **The palette** can't deny an approval, open the terminal or the right panel, or pair a device.

## Method

- **Server:** `bun run dev -- --port 1549 --strictPort` with `?mock=1`.
- **Script captures:** `scripts/screenshots.mjs`, copied with OUT redirected, so no tracked file was written. 98/98 shots, 0 failures.
- **Probes:** my own headless Playwright scripts at 1440×900, 1100×800, 390×844 and 360×780, in light and dark. They read computed styles, bounding boxes, overflow (any element with `right > innerWidth`), focus order and keyboard toggles. A WCAG contrast scan converts each computed colour through a canvas and composites opacity and ancestor backgrounds.
- **Lens:** the impeccable skill, run as a single-context review because the brief forbids subagents. `impeccable detect --json` on `src/components` and `src/features` returned 1 finding, the same false positive as r3: `ide/theme.ts:72`, where 8 px is the `md` radius.
- **States mock can't reach:** I rendered two states by calling the store from the page. The phone code-entry screen came from `pairMachine("not-a-link")` and the signed-out Account page from `signOut()`.
- **Cleanup:** `git status` is clean and the dev server is stopped (`curl` → 000).

Captures:
- Script set: `/tmp/claude-1000/-home-chark1es-Github-h4rn/f0db1c64-7e42-4af7-b542-36c74ef7cb81/scratchpad/r4/script/` (98 files).
- Probe set: `/tmp/claude-1000/-home-chark1es-Github-h4rn/f0db1c64-7e42-4af7-b542-36c74ef7cb81/scratchpad/r4/probe/` (87 files). The key ones:
  - `p-usage-360-light.png`
  - `p-pair-code-390-light.png`
  - `signin-1440-light.png`, `signin-390-light.png`
  - `usage-failover-1440-light.png`
  - `thread-panel-1100-light.png`
  - `terminal-open-1440-{light,dark}.png`
  - `set-providers-1440-light{,-2}.png`
  - `mcp-resolve-1440-{light,dark}.png`
  - `pr-{commits,checks,files}-1440-{light,dark}.png`
  - `p-set-*-{390,360}-light.png`

## Round 3 fixes, re-measured

| r3 item | Measured now | Holds? |
|---|---|---|
| P0-1 Terminal drawer | Closed: `display:none 0×0`. Open: ask `536,326 656×150`, composer `536,486 656×72`, drawer `288,600 1152×300`, all inside 900. DOM order `q-th-head → q-stream → q-dock → q-term`. xterm scrollbar 6 px. Same in light and dark. | ✅ |
| P0-2 QR | Tile `476,253 168×168`, svg `486,263 148×148`, inside = true; at 390/360 the tile is `16,269` and the svg `26,279 148`. White plate in dark. Code `74E2-XNGP`, 5 scope checkboxes, `New code` ×1, 0 selects. | ✅ |
| P0-3 Phone Settings | 10 settings routes plus `/library` and `/library/skills` at 390 and 360: `scrollWidth == vw`, 0 unclipped elements. One `aria-current`: Hosts on `/settings/hosts`, Settings elsewhere. | ✅ (but see P2-11: `/library` highlights no tab) |
| P1-1 560 floor | 1100, sidebar open, panel requested: main `812`, panel absent. Sidebar hidden: main `700`, panel `x700 w400`. | ⚠️ the floor holds, but the click does nothing (P1-1 below) |
| P1-2 Key chips | Sidebar `["Ctrl","K"]`; panel footer `Ctrl`/`E` | ✅ |
| P1-3 Row gutter | `.q-row-time` right = 269, 269, 269 | ✅ |
| P1-5 Error banner / hosts menu | "Couldn't reach buildbox. Showing 3 of 4 hosts."; buildbox row in danger with "2h" | ✅ |
| P1-6/7 Subagents | `.q-rp-body` sw 399 = cw 399; `.sel` = `oklch(0.94 0.004 260)`; one bar, on the running child only | ✅ |
| P1-8 Native selects | 0 on the six r3 surfaces. **14 remain elsewhere** (P1-3 below) | ⚠️ |
| P1-9 MCP health glyphs | check / x / key / circle in the settings nav | ✅ |
| P1-10 Git `M` | `oklch(0.5 0.11 62)` = `--warn` | ✅ |
| P1-11 PR detail | Breadcrumb `fg-3`, one header badge, Commits 3, `build · required`, `e2e · required`, View log, Re-run failed, quoted hunks | ✅ |
| P1-13 Resizer focus | the r3 notes stand; not re-broken | ✅ |
| r3 P2: tool rows | `+4 −1`, `3 matches`, no `0s` | ✅. The glyph is still a circled check (P2-1). |
| r3 P2: empty-state footer | footer reads `atlas` | ✅ |
| r3 P2: loading | queued orb + "Connecting…", segment counts "—" | ✅ |
| r3 P2: change-set file header above the hunk | still missing (P2-3) | ❌ |
| r3 P2: Fail over… | present, but the menu copy is raw (P1-2) | ⚠️ |

**Layout v2:**
- One left panel: `NAV "Threads" x0 w288` and `MAIN x288 w1152`.
- ⌘B: `0/288 → none → 0/288`.
- ⌘⌥B: `absent → x1040 w400 → absent`.
- ⌘E in the IDE, then picking *Migrate session store to Redis*: the heading changes from `api` to `h4rn`, the path becomes `orion / /home/you/code/h4rn`, the thread chip updates, and "Restored 3 tabs" shows in the ide-switch shot.

All four behaviours work.

## P0

### P0-1. Phone Usage: the auto-resume card has no padding, and Resume now is cut off at 360
- **Evidence:**
  - At 390 the card spans x 16–374 and its content starts at x=16 (0 px padding), so the countdown `34m 59s` ends against the border at 374.
  - The action row is `Fail over… 16–144`, `Cancel 154–242`, `Resume now 252–384`. That is 368 px of buttons in a 358 px card, and the card's `overflow:hidden` cuts off the last 10 px.
  - At 360 the card ends at 344, so Resume now loses 40 px and reads "Resume n" (`probe/p-usage-360-light.png`).
  - This is the one action on the screen that answers "does it need me". The reference (`shots/p-usage-390-light.png`) has a padded card: countdown as the largest number, "Auto-resumes at 14:32", then two 44 px buttons.
- **Cause:**
  - `features/usage/AutoResumeCard.tsx:82` gives the compact card the list-container class `q-ph-group`. That class (`quiet.css:3446`) has no padding, because grouped lists pad their rows instead.
  - The three-button row at `:124` sits in `display:flex` with `flex:1` children. Their min-content widths add up to more than the card.
- **Fix:**
  - Give the compact card its own class, `.q-ph-resume { padding:16px; display:flex; flex-direction:column; gap:12px }`.
  - Order it: the title line, then the countdown at 30/36 600 with "Auto-resumes at {time}" under it, then a 2-up row of `Cancel | Resume now` (`flex:1; min-width:0`, 44 px).
  - Put Fail over… on its own full-width ghost row underneath, or behind a `···` menu.
  - Drop the "Failover ready…" sentence on the phone (see P1-2).
  - Re-measure: at 360, every button has `right ≤ card.right − 16`.

## P1

1. **At 1100 px the right-panel button presses but nothing opens.**
   - **Evidence:**
     - Clicking "Right panel" sets `aria-pressed="true"` and draws the pressed fill, but no panel renders and main stays 812 (`probe/thread-panel-1100-light.png`).
     - Ctrl ⌥ B does the same. There is no feedback, and the panel only appears after the user guesses to hide the sidebar.
   - **Cause:** `components/shell/AppShell.tsx:472-476` silently drops the panel when `mainRoom < 560`.
   - **Fix:**
     - When the user explicitly opens the panel and there's no room, collapse the sidebar for this window (DESIGN: "the right panel closes before the sidebar does" covers automatic resizes, not an explicit request), or show the panel as an overlay sheet over main.
     - Either way, set `aria-pressed` from what renders, not from `layout.panelOpen`.

2. **The Fail over… menu and the card show backend strings.**
   - **Evidence:** in `probe/usage-failover-1440-light.png`:
     - Each of the 4 items repeats "\`gpt-5.1-codex\` was limited; this model has no active cooldown", with literal backticks.
     - The account is a credential filename ("on anthropic-work.json").
     - A model alias ("fast") sits in proportional text.
     - The card says "Failover ready: claude-sonnet-5-5 on **atlas** · Same family, warmer credential" (the host), while the menu says "on **anthropic-work.json**" (a file).
     - The card line uses a **warning triangle** for a positive state, and wraps the card to 130 px tall (the reference is 88).
   - **Cause:**
     - `AutoResumeCard.tsx:97-104` renders `schedule.failover.reason` with `TriangleAlertIcon`.
     - `:197-208` prints `choice.reason` on every row and the raw `account`.
   - **Fix:**
     - Give the menu a label line, "Codex hit its limit. Resume now on:".
     - Each item: the model in mono, then `· {account name without .json}` in `fg-3`, plus "default" on the first.
     - Show the shared reason once in the label, not per row, and render backticks as `<code>` or strip them.
     - On the card, replace the triangle line with nothing, or with `fg-3` "Will fail over to claude-sonnet-5-5".
     - Make the card and the menu name the same thing (the credential label, not the host).

3. **14 native `<select>`s remain.**
   - **Evidence:** at all four sizes, 4 on `/settings/providers` (per-harness default model, with OS chevrons; `probe/set-providers-1440-light-2.png`).
   - **Cause:** the source also has them in:
     - `components/dialogs/NewThreadDialog.tsx:168,193,237` (⌘⇧N, a primary flow)
     - `features/forge/PullRequestsPage.tsx:426` (the repo picker)
     - `features/ide/DiffView.tsx:213`
     - `features/mcp/McpDialogs.tsx:447,538,673`
     - `features/skills/SkillDialogs.tsx:574,588`
     - `features/proxy/ProxySettings.tsx:497`
   - **Fix:**
     - Replace them all with `ChipSelect` (or the field-style variant used for "6 hours" on Auto-resume).
     - Add a lint rule (`noRestrictedElements: select`) so they don't come back.

4. **Providers has drifted from the spec into a debug dump.**
   - **Evidence** (compared with `shots/set-providers-1440-light.png`):
     - The probe is a 13-row capability list (Usage queue, Legacy usage, v8 config, Fork enrichment…), not the 4 lines the user needs.
     - Keys say "Stored on the daemon", not "Stored on atlas" (DESIGN: name the host).
     - The models table has a "Provenance" column with `claude enriched` badges, and family reads "open ai".
     - The per-harness overrides lack the **config path and redacted preview diff**, and there is no **"Apply and restart" that names the turn it interrupts**. Both are DESIGN §Settings/Providers requirements.
     - The Cooldown column reads "429 rate limit · 480s".
   - **Cause:** `features/proxy/ProxySettings.tsx`. Host wording at `:293,310,416,450`; probe list around `:600-700`; `enriched` badge at `:207`.
   - **Fix:**
     - Fold the probe into Health / Management + version / Model shape / Usage history, and put the rest behind a "Details" disclosure.
     - Name the host in every "Stored on" label.
     - Drop the Provenance column. Show a "Custom" badge only on aliases.
     - Add the config path and the redacted diff under each enabled override.
     - Add "Apply and restart" with "Interrupts *Fix flaky auth test* on atlas" when a turn is live.
     - Show cooldowns as "Cooling down · 8 min".

5. **Phone pairing by code: it names a host it doesn't know, and two lines overlap.**
   - **Evidence:**
     - With no pairing link, the screen is titled "Pair with atlas", shows atlas's blobatar and says "Credentials stay on atlas" (`probe/p-pair-code-390-light.png`). The code could be for any host, so this makes a consent screen claim something it doesn't know.
     - The two notes overlap: the "Credentials stay…" note covers y 415–451 and the error note starts at y 443, an 8 px overlap at 390 and 360.
     - The code block is left-aligned at 260 px inside a centred screen.
     - Its help text is 12 px, below the phone scale (13–16).
   - **Cause:**
     - `components/pages/PairingPage.tsx:58-60` falls back to `active?.machineName ?? "atlas"`.
     - `quiet.css:3797` gives `.q-pair-note` `margin:-8px 0 0`, so a second note rides up into the first.
     - `.q-sub-empty` (`quiet.css:4140`) is the 12 px desktop panel style reused on the phone.
   - **Fix:**
     - With no link, title the screen "Pair a host", show no host avatar or name, and say "Credentials stay on the host".
     - `.q-pair-note + .q-pair-note { margin-top: 6px }`, or drop the negative margin.
     - Centre the code block, make the input full width (`max-width:none`), and set phone body to 15/22.
     - The hero blobatar is 48 px (`:94`); DESIGN says 56.

6. **Signed-out Account & relay is a dead end that shows a raw error.**
   - **Evidence:**
     - The subtitle reads "Relay unreachable / **Failed to fetch**".
     - The fix offered is "then reload this page".
     - The "Sign in" row has no sign-in action, only "No provider advertised" and a secondary "Probe relay".
     - Everything is `fg-2`/`fg-3`, so nothing on the page is primary (`probe/signin-1440-light.png`, `signin-390-light.png`).
     - DESIGN §States: errors are designed, name what failed, and offer Retry.
   - **Cause:** `features/remote/ConnectSignIn.tsx:137-142`.
   - **Fix:**
     - Use one danger-soft banner, "Can't reach h4rn-connect at 127.0.0.1:8787. It may not be running." with a **Retry** button (re-probe) and a Details disclosure that holds the browser error.
     - Disable the Sign in row with its reason until a provider is advertised. Once the probe succeeds, make "Sign in with {provider}" the primary button.
   - **Signed-in view:**
     - The account row prints "Dev" twice: "Dev / dev@example.com" plus "Dev / h4rn web" in the control column.
     - "Transport · No remote machine selected" sits beside a live "Direct · 12 ms" and a raw URL.
     - "2 online of 3" machines disagrees with the footer's "3 of 4 hosts".
     - **Fix:** one identity line (avatar, name, email, Sign out); say "Direct to atlas · 12 ms · encrypted"; move the URL into a Details row.

7. **The command palette is missing DESIGN's "This thread" and "View" actions.**
   - **Evidence:**
     - With an approval pending, "This thread" offers Approve, Keep in Working, Settle now and Interrupt, but **no Deny, Changes or Subagents**.
     - "View" only appears while searching and holds only "toggle theme".
     - Searching "terminal", "right panel" or "changes" returns 0 results.
     - "pair" returns only threads, with no "Pair a device…".
     - Fuzzy matching is loose: "deny" returns *Audit dependency tree*, and "ide" returns 4 threads before "IDE".
   - **Cause:** `components/dialogs/CommandPalette.tsx:148-345`.
   - **Fix:**
     - Add Deny ⌘⌫, Open Changes (D), Open Subagents (⌘J), Open *project* in IDE (⌘E), Right panel (⌘⌥B), Terminal (T), Sidebar (⌘B), and Pair a device… to Go to.
     - Show View without a query.
     - Rank prefix and word-start matches above subsequence matches.

8. **The PR's linked thread shows a done check while that thread needs approval.**
   - **Evidence:** the PR right column shows a grey circle-check beside *Fix flaky auth test*, which is "Needs approval" in the sidebar. The reference shows the blue needs-you dot. "Blue means you" breaks here.
   - **Cause:** `features/forge/PullRequestsPage.tsx:1620` hard-codes `{ kind: "check" }`.
   - **Fix:** `glyphForThread(linkedThread, hostOnline)`, and add "api · atlas" in `fg-3` as in the reference.

## P2

1. **Tool-row glyph is still a circled check.** `lib/agent.ts:131` maps `completed` to `kind:"check"`, which is the thread "done" glyph. Use a plain 14 px Lucide `Check` in `fg-3` (r3 P2, not done). The Bash row puts "· 48 passed" inside the mono target instead of the right-aligned result column; send it as `stats` (mock: `thr_h`).
2. **Contrast.**
   - The Approve button's shortcut chip text is `oklch(1 0 0 / .78)` on accent: **3.79:1 light, 3.53:1 dark** at 11 px (`quiet.css:183`). Use `accent-fg` at full opacity, or 0.9.
   - In dark, times on the selected sidebar row and the selected segment count are `fg-3` on `fill-2`: **4.27:1** (`quiet.css:712` uses `fill-2`; DESIGN's checked pair is `fg-3/sel` = 4.73). Use `--sel` for `.q-side .q-row.sel`, or lift `fg-3` on selected rows to `fg-2`.
3. **The change-set card** still has no file header above the inline hunk. The hunk (`tests/setup.ts` content) sits under the `src/auth/session.test.ts` row, so it reads as that file's diff (`script/change-set-1440-light.png`). Add the 28 px sticky file header (r3 P2, not done).
4. **Dark approval card:** the command well (`--fill` 0.240) on the card (`--bg-raised` 0.232) differs by ΔL 0.008, so it's invisible (`probe/terminal-open-1440-dark.png`). The prototype has the same token pair. Use `.dark .q-ask .q-cmd { background: var(--bg-sunk) }`.
5. **Provenance doesn't drop the model below 820.** At 1100 (main 812) it keeps `claude-sonnet-5-5` and truncates the branch to "h4…". `quiet.css:1122` only has the 640 rule. Add `@container (max-width:820px){ .q-prov .opt{display:none} }`.
6. **Usage:**
   - Y ticks are `62.2k / 31.1k / 0` (max-derived). Use round ticks, as the reference does (0/50k/100k…).
   - The header stacks a 4-option scope segment beside the range segment and a refresh button. On the phone the scope segment floats above the large title (`segTop 8`, `h1 44`). Move scope into a "Global ▾" menu.
   - Limit labels read "harness / proxy / provider", where the reference names the window ("5-hour", "weekly").
7. **MCP:**
   - "1 headers" (`McpPage.tsx:326`, `labels.ts:43`): pluralise.
   - The Resolve… popover asks the user to pick one of 4 harnesses, although the conflict names its harness. Preselect and lock it, and show where the other config lives ("from `~/.codex/config.toml`"), as in the reference.
   - Skills uses the same plug icon as MCP servers in the settings nav. Use the blocks icon.
8. **Devices:** "atlas · Daemon" and "h4rn web · this device" both offer Revoke. Hide Revoke on this device (offer Sign out instead), and confirm before revoking the daemon (`SettingsRemote.tsx:507,526`).
9. **Phone pairing consent:** every scope is a disabled switch at 50% opacity (scopes are now chosen on the host). Switches imply you can change them, so show read-only scopes as a check list with "Set on orion" (`PairingPage.tsx:111-119`).
10. **Hosts menu:** atlas and Ubuntu-24.04 both carry the `on` background (`Sidebar.tsx:293`), so two rows look selected and the keyboard highlight is ambiguous. Mark the active route with a trailing check or the footer line, not a row fill.
11. **Phone `/library` and `/library/skills`** highlight no tab (0 `aria-current`). Map them to Settings in `PhoneNav.tsx`. The phone MCP list uses a wrench icon and a 20 px title with 32 px gutters; use the 30 px large title and 16 px gutters.
12. **Auto-resume settings:** the Resume prompt textarea still shows a native resize grip at 28 px tall. Use `resize:none` with auto-grow.
13. **Relative time:** `format.ts:11` returns "just now", which truncates sidebar titles ("Ship relay retry with backo… just now"). Use "now".
14. **PR:**
   - The review-thread status "Open" uses an amber chip, while the PR state "Open" uses a green one. Same word, two colours. Use `fg-3` text "Unresolved" for threads.
   - In the Checks tab the right column repeats the same checks list. Hide the right-column Checks block while the Checks tab is active.
   - The PR list runs title and status 1100 px apart. Cap it at the 880 px form column.
15. **Mock coherence:** times disagree, which makes captures look broken.
   - The error view says "stopped answering at 09:14" while the host shows last seen 2h ago and the clock reads 02:13.
   - `thr_h` says "Finished 23:12" under a 02:12 message.
   - Derive all of these from one `now` in `lib/client/mock.ts`.

## What still reads as AI slop

- Explanatory lines that state the obvious. They are shorter than r3's but still there:
  - "Failover ready: … Same family, warmer credential"
  - "Chosen on orion when it made this code."
  - "Start h4rn-connect at …, then reload this page."
  - "Each target receives the server its own way."
- Raw strings from the backend:
  - "Failed to fetch"
  - backticked model ids
  - `anthropic-work.json`
  - "429 rate limit · 480s"
  - "Fork enrichment: Present"
- The 13-row capability dump on Providers.
- Native selects with OS chevrons beside the custom chips.

There are no gradients, no emoji, no eyebrows and no decorative colour. Type and spacing hold everywhere I measured. Desktop contrast passes everywhere except the chip and selected-row cases in P2-2.

## To reach 10

1. **P0-1** (the phone resume card). It's one component and gets back about 0.4.
2. **P1-1, P1-2, P1-5, P1-6, P1-8.** These are all small and copy-heavy, and they are what a first-time user hits: the 1100 dead click, the Fail over menu, pair-by-code honesty and overlap, sign-in, and the linked-thread glyph. About 8.9.
3. **P1-3** (selects, plus a lint rule), **P1-4** (Providers back to spec) and **P1-7** (palette coverage). About 9.5.
4. **All P2s**, then re-capture. Unify the mock clock first so the captures stop contradicting themselves.
5. **Measure these in the browser, not from notes:**
   - At 360/390 on `/usage`, every button in the resume card has `right ≤ card.right − 16`.
   - At 1100 after clicking Right panel, the panel renders, or `aria-pressed` stays false.
   - On `/settings/providers` and in the ⌘⇧N dialog, `document.querySelectorAll('select').length === 0`.
   - On `/pairing` with no link, no host name appears, and no two `.q-pair-note` boxes intersect.
   - The palette offers Deny, Terminal, Right panel and Pair a device with an empty query.
   - The approve-chip contrast is ≥ 4.5, and dark `fg-3` on the selected row is ≥ 4.5.

## Round 4 fixes

Measured in headless Chromium against `bun run dev -- --port 1551 --strictPort` with `?mock=1`, at 1440×900, 1100×800, 390×844 and 360×780, light and dark (both themes gave the same result unless a value is listed per theme). `node scripts/screenshots.mjs` captured 98/98 with 0 failures. The dev server is stopped.

### P0

1. **Phone auto-resume card.**
   - It has its own class, `.q-ph-resume`: 16 px padding, title, 30/36 countdown, "Auto-resumes at 07:20", then a 2-up `Cancel | Resume now` row (44 px) and a full-width `Fail over…` row.
   - At 390 the card spans 16–374 and the buttons end at 358. At 360 the card spans 16–344 and the buttons end at 328. Every button has `right ≤ card.right − 16`, and `scrollWidth` equals the viewport.
   - The "Failover ready…" sentence is gone on the phone.

### P1

1. **Right panel at 1100.**
   - Clicking Right panel (or Ctrl Alt B, or opening the IDE or Files) closes the sidebar when the panel would not fit. Automatic resizes still hide the panel first.
   - Measured: the panel renders, main is 700 px (floor 560), `aria-pressed="true"`. `aria-pressed` now follows what is on screen (`panelFits`).
2. **Fail over… copy.**
   - Rust now sends `account_label` ("anthropic-work", derived in `h4rn-protocol::credential_label`), and the reasons are plain text with no backticks.
   - The menu reads "Codex hit its limit. Resume now on:", then `claude-sonnet-5-5 · anthropic-work · default`. A reason shows per row only when the reasons differ.
   - The card no longer shows the warning triangle (0 triangles). The "Failover ready…" line is gone, and the card is 88 px tall (was 130).
3. **Native selects.**
   - 0 `<select>` on the 9 routes probed (`/settings/providers`, devices, forges, resume, `/pulls`, `/library`, `/library/skills`, `/usage`, `/ide`) at 1440 and 390 in both themes, and in the ⌘⇧N dialog. That is 38 probes, all 0.
   - All 12 source sites use `ChipSelect` (added a `block` variant).
   - `biome.json` enables `correctness/noRestrictedElements` for `select`.
4. **Providers.**
   - Capability summary: Health, Management, Model shape, Usage history. The rest is behind "Details".
   - The host name appears ("CLIProxyAPI on atlas", "Stored on atlas"). "the daemon" appears 0 times and "Provenance" 0 times. The models table shows Model, Maps to, Context, plus a Custom badge on aliases. Cooldowns read "Cooling down · 8 min".
   - Each enabled override shows its config path (`~/.claude/settings.json`) and a redacted preview. The preview is derived in Rust (`ProxyHarnessOverride.config_path` and `.preview`, secrets are `null`).
   - **Apply and restart** is new in Rust: `ApplyProxyOverrides` interrupts the live turn of every routed harness and drops the session. The footer reads "Interrupts *Fix flaky auth test* on atlas." and the button enabled after a toggle. Covered by `applying_proxy_overrides_drops_the_sessions_of_routed_harnesses`.
   - No horizontal overflow at 1440, 390 or 360.
5. **Phone pair by code.**
   - With no link the title is "Pair a host" and no host name appears.
   - Notes: 2 `.q-pair-note` elements, 0 overlap in the normal and error states at 390 and 360.
   - The code block is centred, the input is full width (342 px at 390), and the body is 15 px.
   - The hero blobatar is 56 px. Scopes are a read-only check list ("Set on orion.").
6. **Signed-out Account.**
   - One danger-soft banner: "Can't reach h4rn-connect at 127.0.0.1:8787. It may not be running." with a Retry button (enabled) and a Details disclosure that holds the browser error.
   - "Failed to fetch" is not visible outside Details. `Sign in` is a disabled primary button with the reason "Available once the relay is reachable" and becomes "Sign in with {provider}" once the probe succeeds.
   - The signed-in view has one identity line, "Direct to atlas · 12 ms · encrypted", and the relay URL on its own row.
7. **Command palette.**
   - With an empty query and an approval pending it lists Approve, Deny (⌘⌫), Open changes (D), Open subagents (⌘J), Open api in the IDE (⌘E), Show right panel (⌘⌥B), Show terminal (T), Hide sidebar (⌘B) and Pair a device….
   - Searching "deny", "terminal", "right panel", "changes" and "pair" each puts the matching command first. "deny" lists Deny pending action before *Audit dependency tree*. Ranking is prefix, then word start, then substring, then a last-resort subsequence (unit-tested).
8. **PR linked thread.** The glyph comes from `glyphForThread(thread, hostOnline)` (the needs-you dot for *Fix flaky auth test*), and the line reads "api · atlas".

### P2

1. **Tool rows.** The completed glyph is a plain 14 px `lucide-check` in `fg-3` (3 of 3 rows). The Bash row shows `48 passed` in the result column from `ToolStats::Tests`, parsed in Rust from the shell output (unit tested), with no `· 48 passed` in the target.
2. **Contrast.**
   - Approve chip: 5.09:1 light, 4.66:1 dark (was 3.79 / 3.53).
   - Selected-row time: 4.82:1 light, 4.73:1 dark (was 4.27 in dark). The selected segment count in dark uses `fg-2`.
3. **Change-set card.** A 28 px file header ("tests/setup.ts") sits above the inline hunk.
4. **Dark approval card.** The command well is `oklch(0.172)` on a card of `oklch(0.232)` (was ΔL 0.008).
5. **Provenance.** At 1100 the `.opt` model is `display:none` (container rule at 820 px).
6. **Usage.** The axis uses round steps (`niceCeil`: 100k / 50k / 0). Scope is a "Global ▾" chip, so there is 0 scope segment, and on the phone it sits right-aligned above the title. Limit labels read "weekly" or "5-hour" when the message names the window, and otherwise say who reported them.
7. **MCP.**
   - "1 header" is singular.
   - The Resolve… popover locks the harness when the conflict names one.
   - Skills uses the blocks icon in the settings nav, and the phone MCP list uses the plug icon, a 30 px title and 16 px gutters.
8. **Devices.** "h4rn web · this device" offers Sign out (the same revoke, with a confirm) instead of Revoke. The daemon row asks "Disconnect this host?" before revoking. The remote e2e signs out through it.
9. **Phone pairing consent.** Scopes are a check list, not disabled switches (see P1-5).
10. **Hosts menu.** No row has a fill (all `rgba(0,0,0,0)`). A trailing check marks only the active machine (`[true,false,false,false]`).
11. **Phone `/library`.** The Settings tab is `aria-current` (was 0).
12. **Auto-resume prompt.** `resize:none` with auto-grow.
13. **Relative time.** "now" replaces "just now" (0 occurrences on the inbox).
14. **PR.**
    - Review-thread status reads "Unresolved" in `fg-3`.
    - The right-column Checks block is `hidden` on the Checks tab.
    - The list is capped at 880 px.
15. **Mock clock.**
    - Turn and tool times derive from each thread's last activity. thr_h now reads "Finished 03:46" at a clock of 06:46, which is 3 h ago and consistent.
    - The error view reads "stopped answering at 04:46", 2 h before the 06:46 clock, matching the host's "2h".

### Lint

`bun run lint` reports **0 warnings** (was 67: 64 `noDescendingSpecificity`, 2 `useOptionalChain`, 1 unused suppression). The CSS rules were reordered, with no global suppression and no ignore comments added. The only suppression is a justified `useExhaustiveDependencies` on the Retry re-probe effect. The reorder was checked with before/after captures; the only differences were timing noise (clocks, orbs, a loading skeleton).

### Verification

See the commit message of the final commit.
