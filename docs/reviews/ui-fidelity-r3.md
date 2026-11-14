# UI fidelity review, round 3 — h4rn Quiet native at `dev` 77b8989 (wired to backend, mock mode)

**Score: 6.8 / 10** (r2: 8.0).

The desktop thread, sidebar, IDE and right panel still match the prototype closely. The layout v2 behaviours work: there is exactly one left column, Ctrl B and Ctrl Alt B toggle, and the IDE follows the project. But wiring the backend added three P0s:

- The terminal drawer hides the approval card and the composer, and once opened it can never be closed.
- The pairing QR code spills out of its white frame.
- Every phone Settings page renders the desktop form, so content is clipped off-screen.

r2 also said the sidebar's ⌘K chip was fixed, but it still renders as `CtrlK` in one chip.

Method: `bun run dev -- --port 1541 --strictPort`, `?mock=1`. `scripts/screenshots.mjs` was copied with OUT redirected (98/98 captured, 0 failures). Three headless Playwright probes ran at 1440×900, 1100×800, 390×844 and 360×780, in light and dark, reading computed styles, bounding boxes, overflow, focus order and keyboard behaviour. The impeccable skill was used as the critique lens. Its detector (`impeccable detect --json` on `src/components` and `src/features`) returned 1 finding, a false positive: `ide/theme.ts:72` "8px outside the radius scale", but 8 px is the `md` radius. No tracked file was edited and `git status` is clean. The dev server is stopped.

Captures:
- Script set: `/tmp/claude-1000/-home-chark1es-Github-h4rn/f0db1c64-7e42-4af7-b542-36c74ef7cb81/scratchpad/r3/script/` (98 files)
- Probe set: `/tmp/claude-1000/-home-chark1es-Github-h4rn/f0db1c64-7e42-4af7-b542-36c74ef7cb81/scratchpad/r3/probe/`. This includes `terminal-approval-1440-light.png`, `thread-panel-1100-light.png`, `delegate-1440-light.png`, `ide-follow-web-1440-light.png`, `set-{resume,forges,account,hosts,general,appearance}-1440-{light,dark}.png` and `p-{settings,devices,providers,forges,account,error,…}-{390,360}-light.png`.

## Layout v2 checks (measured)

| Check | Result |
|---|---|
| Exactly one left panel | ✅ landmarks: `NAV "Threads" x0 w288`, `MAIN x288 w1152`; nothing else docks left. Settings takes over the sidebar. |
| Ctrl B toggles sidebar | ✅ `0/288 → none → 0/288` |
| Ctrl Alt B toggles right panel | ✅ `absent → x1040 w400 → absent` |
| Ctrl E Thread ↔ IDE | ✅ the heading becomes the project (`api`) in place |
| IDE follows another project's thread | ✅ selecting *Migrate session store* switches the heading to `h4rn · orion · /home/you/code/h4rn`, chip "Migrate session store to Redis", Files tree re-rooted at `h4rn`, Agent changes footer updated; the ide-switch shot shows "Restored 3 tabs" |
| Main never below 560 px | ❌ at 1100 with the panel open, main is **412 px** (see P1-1) |
| Approval never pushed off by terminal | ❌ see P0-1 |

## P0

### P0-1. The terminal drawer removes the approval card and composer, and can't be closed
- **Evidence:**
  - On `/t/thr_a`, the composer box before the terminal opens is `336,786 656×72`. After "Toggle terminal" it is `0×0`, and `.q-ask` (the approval card) is gone. Capture: `probe/terminal-approval-1440-light.png`, compared with reference `shots/terminal-1440-dark.png`, where the approval and composer stay above the drawer.
  - Toggling again does not hide the drawer. The probe measured `term "286/340 hidden=false display:flex"` with the dock back at 274 px, so the drawer and composer now share the main column and squeeze the stream. `probe/delegate-1440-light.png` shows that state.
  - The drawer is 340 px tall; the spec is 300.
- **Cause:**
  - `styles/quiet.css:1769` sets `.q-main.q-term-open > .q-dock { display:none }`.
  - `ThreadPane.tsx:350` renders `<TerminalPanel>` *before* `.q-dock` (`:361`).
  - `TerminalPanel.tsx:89` hides itself with the Tailwind `hidden` class. Tailwind's utilities layer loses to the unlayered `.q-term { display:flex }` at `quiet.css:1761`, so `hidden` never applies.
  - The height comes from `quiet.css:1763` (`height:340px`).
- **Fix:**
  - Delete the `.q-term-open > .q-dock` rule.
  - Render `TerminalPanel` after `.q-dock` so the order is stream → approval → composer → drawer.
  - Add `.q-term.hidden { display:none }`, or render `null` / `display:none` via a `q-term[hidden]` attribute.
  - Set `height:300px`.
  - Re-measure: with the terminal open, `.q-ask` and `.q-composer` must be fully inside the viewport. With it closed, the `Terminal panel` element must have `display:none`.

### P0-2. The pairing QR code overflows its white tile (Settings → Devices)
- **Evidence:**
  - `.q-qr` tile: `476,253 168×168`, padding 10.
  - QR `<svg>`: `486,263 188×188`. It spills 30 px right and down, over the "atlas" text column. In dark mode the two white plates are visibly offset (`script/set-devices-1440-dark.png`).
  - At 390 the same overflow covers the right column (`probe/p-devices-390-light.png`).
  - A QR without its quiet zone inside the frame can fail to scan.
- **Cause:** `features/remote/QrCode.tsx:13` defaults to `size = 188`. `SettingsRemote.tsx:636` doesn't pass a size, while `quiet.css:2890` fixes `.q-qr` at `168px` with 10 px padding.
- **Fix:** pass `size={148}`, or drop the fixed width and height on `.q-qr` and let the 188 px code plus padding set the size. In both cases keep a ≥4-module quiet zone inside the white plate.
- **Missing from the spec on the same screen:**
  - No 8-character code: the probe found no `XXXX-XXXX` pattern.
  - No scope checkboxes (0 found).
  - "New code" appears twice.
  - The machine picker is a native `<select>`.
  - The raw URL `http://127.0.0.1:8787` sits under the buttons.
- **Fix to match `scrDevices`:** QR, then the mono code (`7K4Q-2M9P`), then "Works once, for one device…", then the 2-column scope checkboxes, then one Copy link / New code row. Use the chip picker (as in the empty state) instead of `<select>`.

### P0-3. Phone Settings is the desktop form squeezed into 390/360 px, so content is clipped
- **Evidence at 390 and 360:** elements extend past the viewport while `scrollWidth == vw`, so they are cut off, not scrollable.
  - Hosts: `Pair a device →399`, `Refresh →537`.
  - Providers: `Replace →527`, `Remove →634`; the Base URL input collapses to **~20 px wide**.
  - Account: `http://127.0.0.1:8… →569`, `Manage hosts →447`.
  - Forges: `Add account →429`.
  - Devices: `New code →409`; the Machine `<select>` collapses to a chevron.
  - MCP: the command string → 394.
- **Layout:**
  - The page header keeps the title and its actions side by side, so the description wraps one word per line ("Machines / that run / agents…").
  - Tables keep 4–5 columns and are truncated ("Onlin", "Presen").
  - Captures: `probe/p-settings-390-light.png`, `p-providers-360-light.png`, `p-devices-390-light.png`, `p-account-390-light.png`.
- **The tab bar highlights both Hosts and Settings** (`aria-current="page"` on both). Cause: `PhoneNav.tsx:43-49`: the `hosts` and `settings` keys both match `/settings/hosts`.
- **Cause of the layout:** the Settings forms in `SettingsPage.tsx`, `ProxySettings.tsx`, `SettingsRemote.tsx` and `ForgeSettings.tsx` have no phone branch. The `.q-page-head` and table styles have no `@media (max-width:640px)` rules.
- **Fix:**
  - Below 640 px, stack the page head: title, then description, then a full-width actions row.
  - Turn label/control rows into a grouped inset list: label on top, control full width.
  - Turn tables into one 2-line row per item (name + status on line 1, detail on line 2), as DESIGN §Phone does for the inbox.
  - Give the Base URL input `min-width:0; width:100%`.
  - Make only one tab active: `/settings/hosts` → Hosts, every other `/settings/*` → Settings.

## P1

1. **Main falls to 412 px at 1100 with the right panel open.** The thread title disappears from the header, the terminal icon is clipped at x≈695, and "Approve Ctrl ↵" is cut to "Approve C" (`probe/thread-panel-1100-light.png`). DESIGN §Layout 6 says main ≥ 560 and the right panel closes before the sidebar does.
   - **Fix:** in AppShell, when `viewport − sidebar − panel < 560`, close the panel, or show it as an overlay sheet over main. Re-measure `main.width ≥ 560` at 1100×800.

2. **The sidebar ⌘K chip renders as `CtrlK` in one chip**, so r2's chip fix doesn't hold here.
   - **Evidence:** `.q-kbd` contains `<span>C</span>…<span>K</span>`; one chip; text "Search or runCtrlK".
   - **Cause:** `Sidebar.tsx:600-602` wraps `KeyGlyphs value={`${MOD}K`}` in a single `.q-kbd`.
   - **Fix:** `<KeyCaps value={`${MOD} K`} />` (two chips, Ctrl and K). Apply the same fix to "Open in IDE CtrlE" and "Back to thread CtrlE" in the right-panel footer.
   - The Files filter shows `⌘P` on Linux while everything else says Ctrl. Route it through `MOD` too.

3. **Sidebar rows waste a 32 px right gutter**, so titles truncate where the prototype doesn't ("Migrate session store to R…", "Ship relay retry with bac…").
   - **Evidence:** `.q-row-main` right edge is 237 while the row's content edge is 269; time and status end at x=237 against the reference's 269. The `···` button sits in flow at `243–265`, 22 px wide, with opacity 0.
   - **Cause:** `quiet.css:3615` `.q-ibtn { position:relative }` comes later than, and so overrides, `.q-row-more { position:absolute }` at `:920`.
   - **Fix:** `.q-row .q-row-more { position:absolute; top:5px; right:4px }`, or move the `.q-ibtn` rule above. Expected result: time and status end at x≈269.
   - The phone inbox has the same 34–54 px gutter: status right edge 336 of 390, and 306 of 360.

4. **Phone project headers show a tofu square instead of a chevron.**
   - **Evidence:** the last child is `<span class="chev">▾</span>` (9×18); the font has no glyph for it (`script/p-inbox-390-light.png`).
   - **Cause:** `PhoneInbox.tsx:133`.
   - **Fix:** `<ChevronDownIcon className="q-icon s16" strokeWidth={1.5}/>` in `fg-3`.

5. **The partial-failure banner names the wrong host.** The error scenario's thread and hosts menu say **buildbox** is unreachable, but the sidebar says "Couldn't reach **atlas**. Showing 3 of 4 hosts."
   - **Cause:** `Sidebar.tsx:649` uses `Object.values(hosts)[0]?.name`.
   - **Fix:** name the host(s) whose presence is offline or failed. Fall back to "a host" when there are several.
   - Also, in the hosts menu, render the unreachable row in `danger` with its last-seen time, per DESIGN §States. It is currently `fg-3` with a hollow dot.

6. **Subagents tab overflows horizontally.**
   - **Evidence:** the grandchild node `.q-sub-node.kid2` ends at x=1450 against the panel's 1440; `.q-rp-body` has scrollWidth 409 vs clientWidth 399. "0:08" touches the edge and the budget bar runs past it (`script/subagents-1440-light.png`).
   - **Fix:** `min-width:0` on the node grid, and reduce the left indent per depth (each level adds about 22 px). Clamp the budget bar to the content box.

7. **Subagents selection isn't visible, and done children still show a budget bar.**
   - **Evidence:** the selected node `.q-sub-node.sel` has background `rgba(0,0,0,0)`, while the detail below shows "Audit the session store…".
   - **Fix:** `.q-sub-node.sel { background: var(--sel); border-radius: 8px }`, matching the prototype. Show the 3 px bar only while `running`, per DESIGN §Subagents tab.

8. **Native `<select>` elements are still used on the wired surfaces:**
   - Delegate dialog (3: Host, Harness, Workspace)
   - Devices (1)
   - MCP conflict block (2, `McpPage.tsx:225,237`)
   - Forges (1)
   - Auto-resume (1)
   - Skills (1)

   They render OS chrome that clashes with the Quiet controls, and the dark-mode popups are unthemed. The MCP conflict block also differs from the prototype: it should offer one "Resolve…" action that opens a popover with the choice, not two dropdowns plus a small button (`script/library-1440-light.png`). The Delegate task `<textarea>` shows the native resize grip.
   - **Fix:** reuse the empty-state chip picker (24 px, radius 6, avatar + label + 12 px chevron) or the Radix dropdown. Use `resize:none` with auto-grow.

9. **MCP server health is shown by colour alone.** The settings nav shows orange, green and grey dots (`Sidebar.tsx:1012`). DESIGN rule 3 says colour is "never shown as colour alone", and the prototype uses a check, a red x and an amber key.
   - **Fix:** use the 14 px Lucide `CircleCheck` / `CircleX` / `KeyRound` in `fg-3`, `danger` and `warn`.

10. **IDE file tree git marker `M` is accent blue.** Reference and DESIGN: "`M` in `warn`, `A` in `ok`". Blue is reserved for "you".
    - **Fix:** `color: var(--warn)` for M and `var(--ok)` for A.

11. **PR detail is badge soup and slop.** Compare `script/pr-1440-light.png` with `shots/pr-1440-light.png`.
    - The breadcrumb is entirely accent blue (`Pull requests / h4rn/api / #482`).
    - There are four badges: Open, No conflicts, and the `tests` and `auth` labels.
    - Every review comment has an always-open textarea with a visible native resize grip, plus a faded Reply button.
    - Review comments don't quote their diff context, which DESIGN requires.
    - **Fix:**
      - Breadcrumb in `fg-3` with `/` separators; only the first segment is a link, and it's neutral.
      - Keep only the Open badge, and move labels to the right column.
      - Show the textarea only after Reply is clicked.
      - Quote the hunk under each comment (the `+9/+10` lines in the reference).
      - Add View log / Re-run failed under Checks, mark `required` checks, and add the Commits tab.

12. **The phone error state has no gutter and widens the layout viewport.**
    - **Evidence:** at 390 the document is 461 px wide; text and buttons start at x=0 (`probe/p-error-390-light.png`). The header reads "Last seen running", but the desktop error header says "Host unreachable".
    - **Fix:** 16 px side padding, `max-width:100%` on the error block, and the same header label on every platform.

13. **The resize handle has no visible keyboard focus.** The sidebar resizer (`<hr aria-label="Resize sidebar">`) is in the Tab order, but tabbing to it shows `outline:none` and transparent `::before`/`::after`. DESIGN §Layout 6 says the hairline becomes a 2 px `focus` line on keyboard focus.
    - **Fix:** `.q-resizer:focus-visible::after { background: var(--focus); width: 2px }`, plus `role="separator" aria-orientation="vertical" aria-valuenow`.

## P2

- **Tool rows show `0s` for every sub-second call** (`Read 0s`, `Search 0s`, `Edit 0s`), which is noise where the reference shows measured results (`+12 −4`, `+6`).
  - Hide durations under 1 s.
  - Show the diffstat for Edit/Write, the match count for Search, and `48 passed` for Bash.
  - The status glyph is a circled check; the reference uses a plain 14 px check.
- **Change-set card in the stream:** file rows carry file icons, which the reference doesn't. The inline hunk is clipped at the card's bottom edge with no file header (`script/change-set-1440-light.png`: line 3 touches the border). Drop the icons, add the sticky file header above the hunk, and give it 4 px bottom padding.
- **Approval card subtitle** reads "Command · in /home/you/code/api". Drop "Command ·". Use the `~/code/api` form, and add the worktree when one exists.
- **Header provenance** uses `·` separators and appends the full path. The prototype uses `/` separators, a branch icon before the branch, and no path. Drop the path, which is already in the IDE header.
- **Usage:**
  - Stat captions wrap to 2 lines ("721.8k in · 157.1k out", "estimate, not billing") and squeeze the sparklines. Shorten them to one line ("+12% vs prior", "estimate").
  - The Limits row shows "100%" in red without the word "Limited".
  - The auto-resume card has filler ("The provider reset time is known.") and says "Cancel" where DESIGN says "Cancel auto-resume". It is also missing "Fail over…".
  - `.q-stat` overflows by 2 px at 1100, and chart labels overflow to x=380 at 360.
- **Phone Changes sheet:**
  - "All 1 turns" should read "1 turn" (`PhonePanelSheet.tsx:299`).
  - The +/− stats and chevron sit mid-row (x≈186–316) instead of right-aligned.
  - Two explanatory paragraphs say "Press Ctrl E" on a phone. Keep one sentence: "Files and the IDE are on desktop."
- **Phone inbox rows** still print the host on line 2 ("atlas · Claude Code"), but DESIGN drops it because the group header carries it (`ThreadRow.tsx:130-133`; `compact` isn't set on phone).
- **Hosts menu:**
  - The "Dev / http://127.0.0.1:8787 / Sign out" block is indented differently: Sign out at x=32, other items at x=22.
  - It shows a raw URL. Move account and sign-out to Settings → Account, and keep the menu to presence, Pair, and Manage.
- **Terminal:**
  - The vertical scrollbar is **14 px** (`.scrollbar.vertical`), not the 6 px r2 claimed.
  - The prompt branch is amber in light mode (mock ANSI 180); DESIGN says use `accent-tx`. This only comes from the mock, so fix it in `mock.ts:1969` so captures match.
- **Empty state:**
  - "Add a project…" uses the pencil (new-thread) icon; use `FolderPlus`.
  - The footer says "3 of 4 hosts" and the PR badge shows 3 in a no-projects world; DESIGN says "footer shows the one host".
  - The "Start a thread" title renders in `fg-2`; it should be `fg`.
- **Loading:** the footer shows a grey static dot; DESIGN says a queued orb with "Connecting…".
- **Error state:** the Details disclosure uses the native `▶` marker. Use a 12 px chevron.
- **Providers:** the "Test / probed 12m" label is vertically centred against a 14-row list. Top-align it.
- **PR footer badge** renders as a superscript `³` on the icon; the reference shows inline "⑂ 3".

## What reads as generic "AI slop"

- Always-open reply textareas with native resize grips on every PR comment.
- Native `<select>` controls in six places.
- Badge clusters in the PR header (Open + No conflicts + labels).
- Explanatory filler lines: "The provider reset time is known."; the two paragraphs in the phone Changes sheet; "Blank limits use the daemon defaults…", which is acceptable but long.
- The fully blue breadcrumb.

There are no gradients, no emoji and no decorative colour. Type is consistent: header 15/600/−0.15 px, prose 14/22 in a 720 px column, usage line 11.5 px. Shortcut chips on the approval card are 16 px single-line (four chips, as r2 left them).

## To reach 10

1. **P0s:**
   - Terminal: dock order, a real `hidden`, 300 px height, and approval + composer visible with the drawer open.
   - QR: size/frame fix, then add the code and scopes.
   - Phone Settings: stacked header, grouped lists, no clipping at 360, one active tab.

   These alone get back to about 8.3.
2. **P1-1…13** bring it to about 9.3. Do the 560 px main floor, the Ctrl K chip, the row gutter and the phone chevron first; they show on every screen.
3. **All P2s**, then re-capture every shot.
4. **Re-measure in the browser rather than trusting status notes:**
   - `.q-ask` and `.q-composer` inside the viewport with the terminal open.
   - `Terminal panel` has `display:none` when closed.
   - QR `svg` box fits inside `.q-qr`.
   - At 390 and 360, no element has `right > innerWidth` on any `/settings/*` route.
   - `main.width ≥ 560` at 1100 with the panel requested.
   - `.q-row-time` right edge ≈ 269.
   - Sidebar search shows 2 chips.
   - `.q-rp-body` `scrollWidth == clientWidth` on Subagents.
