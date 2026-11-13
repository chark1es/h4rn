# UI fidelity review, round 1 (Opus, designer of Quiet native) — branch ui-quiet-native @ b51f14b

Score: 6.5/10. Tokens match DESIGN.md; Library/Devices/Providers/PR near pixel-faithful. Most deviations come from ~6 structural causes.
Side-by-side composites: /tmp/claude-1000/-home-chark1es-Github-h4rn/ea2688f0-8945-4573-8836-edd97ab1ab48/scratchpad/cmp/ ; 360 px renders: same scratchpad, p360-*.png

## P0
1. Command palette broken: renders at x=154,y=-183, 512px wide, under its own scrim (z 21 vs overlay 50), clicks hit overlay. Prototype: top:96px, 620px wide, centered, above scrim. Cause: Tailwind v4 uses `translate` property not cancelled by `.q-palette{transform:none}`; `sm:max-w-lg` caps width; `.q-palette{z-index:21}`. Fix: `.q-palette{translate:none;max-width:none;z-index:51}` in quiet.css or drop the utilities in ui/command.tsx:38. Group "This thread" must come first (currently third after Threads and Recent threads).
2. Transcript column ≠ approval card/composer column. Stream: 720px content from x=630; dock: 656px from x=662. At 1100px stream text flush against the pane divider/right edge (zero gutter). Prototype: `.col{max-width:720px;padding:0 32px}` for stream AND dock → 656px content, one left edge. Cause: ThreadStream.tsx:114-128 virtualized rows are `position:absolute;left:0;width:100%`, ignoring parent padding (loses 32px sides and 28px top). Fix: `left:32px;right:32px` (0 under 639px) or a padded position:relative wrapper inside .q-col; use virtualizer paddingStart:28, paddingEnd:20.
3. Sidebar nav labels, Settings and host names centered (every desktop screen). Prototype resets `button{text-align:inherit}` (line 115). Tailwind preflight doesn't. Fix: add `button{text-align:inherit}` (or start) to the base layer in globals.css.
4. Phone buttons/switches use desktop sizes (approval card, approval sheet, pairing, usage): Details/Review, Deny/Approve, Pair this phone, Cancel/Resume now render 28px tall, 13px left-aligned; switches 30×18. Prototype: 44px buttons, 12px radius, 16–17px centered labels, 51×31 switches. Cause: rules scoped to `.q-ph …` (quiet.css:2898-2925) and no element has class q-ph. Fix: add q-ph to the phone shell root or move rules into the `@media (max-width:639px)` block. Also a WCAG target-size failure.
5. Open thread not highlighted in inbox (inbox, inbox-done, subagents, offline, error): on deep link/reload nothing has .sel; only after a click. Prototype: open row on --sel background, 8px radius. Fix: set ui.selectedThreadId from the route on load (router or InboxPane).
6. Inbox rows have no hairline separators; hover/selection fills run edge to edge (all inbox screens incl. phone). `.q-row + .q-row::before` can't match because each row is in its own virtualizer wrapper div; same left:0;width:100% ignores .q-rows 6px side padding. Prototype: inset hairlines (left:34px;right:10px, --line) hidden around hovered/selected rows; rows inset 6px, 8px radius. Fix: draw separator from wrapper (e.g. `[data-index]:not(:first-child) > .q-row::before`), inset wrappers left:6px;right:6px in InboxPane.tsx:516-524.

## P1
Thread/stream:
- Composer double focus ring: textarea shows own 2px outline inside composer :focus-within ring → `.q-composer textarea:focus-visible{outline:none}`.
- Row "more" button overdraws the timestamp on hovered/selected rows → hide .q-row-time while button visible or reserve space.
- Phone prose 14px not 16/24: markdown renders into .stream-prose (globals.css:48) but phone override targets .q-prose (quiet.css:2472) → add .stream-prose to that rule.
- Provenance line never drops items: prototype `@container (max-width:640px){.prov .opt,.prov .br{display:none}}`; quiet.css has no @container rule. Line clips mid-word with no ellipsis; at 360px phone "Claude Code" wraps to two lines.
- Done thread header missing actions: prototype shows `Snooze` (secondary) and `Acknowledge [A]` (primary) + "Done · 3h ago"; app shows only icons and "Done · unread". Missing receipt line "Finished 09:19 · 7m 02s · 22.1k tokens · $0.51 est.".
- Terminal panel layout: prototype = 340px bottom panel and dock hidden; app keeps approval card + composer below terminal, stream squeezed to ~150px.
- Terminal rendering: phone 13.5px/1.5 (27px pitch) wraps at 45 cols; prototype 12px/18px pitch. Desktop 12.5/1.25. Change in terminal-controller.ts:95-96. "✓" renders as tofu (font fallback). "Opening shell…" overlay uses a spinner (spec bans spinners). Error/offline overlay uses border-white/20 and hover:bg-white/10, invisible in light theme (TerminalView.tsx:140-207).
Inbox/lists/states:
- "Limited" row shows a queued-style orb; should be amber pause glyph (limited, needs you) or amber clock + "Resumes 14:32" (scheduled). Where resume_at exists anywhere, the row must show it.
- Empty state lacks the starter: prototype detail pane is "Start a thread" with host, harness, folder pickers + composer; app shows "No thread is open" with two buttons.
- Offline banner is red; spec wants neutral. Duplicate red "Your reply is saved as a draft…" under composer (offline and error) → remove; placeholder already says it.
- Error state: list banner should read "Couldn't reach buildbox. Showing 3 of 4 hosts. Retry" (app: "Reconnecting · attempt 1"); header "Host unreachable" (app: "Last seen running"); prototype shows no composer when a thread failed to load.
- Phone inbox "Snoozed" row is 13px and ~40px tall; prototype 16px label, 66px row.
Approval card:
- "Always allow" is a disabled ghost button with "›"; prototype: active fg-2 menu trigger with "▾". If wire can't support it keep disabled but show ▾ and reason in tooltip.
- Shortcut chips "Ctrl Backspace"/"Ctrl Enter" are wide; prototype compact SVG glyphs ⌘⌫/⌘↵; on Linux/Windows use "Ctrl ⌫"/"Ctrl ↵". Sidebar still says "⌘K" (make platform-consistent).
- Phone sheet copy wrong: "Claude Code · subagent of Fix flaky auth test" for the thread's own request. Drop "subagent of" when the requester is the thread's own agent.
Usage/settings:
- Usage chart stack inverted: [3,2,1,0] + column-reverse (UsagePage.tsx:292-298) reverses twice; Claude Code blue on top and rounded cap on bottom segment. Use one or the other.
- Usage limit meter at 100% is graphite not red: class q-meter danger but CSS only defines .q-meter.full (UsagePage.tsx:701 vs quiet.css:2046).
- Settings → Hosts: WSL distros (Ubuntu-24.04, Debian) not indented under atlas; prototype indents avatar+name 16px.
- Subagents inspector: Credential value wraps (key icon on one line, text on next); 3px budget bar under selected running node missing; Open thread / Cancel subagent / Delegate look disabled (must not look disabled when they would work).
Editor: "Hand to agent" and "since 12:03" missing; tabs show × where prototype shows a dirty dot; selected file in Agent changes not highlighted.
Keyboard: thread listbox has no Tab stop (Tab goes from segmented control to hidden "more" buttons). Roving tabindex=0 on the selected row, ↑/↓ between rows.

## P2
- Icons differ from prototype set: filter (sliders vs lines), MCP (blocks vs plug), Editor (</> vs <>), terminal (square-terminal vs >_), failed check (triangle vs ✕-circle).
- Providers probe glyphs plain grey check/dot; prototype ok-colored check-circles.
- Phone back button "← Inbox"; prototype iOS "‹ Inbox". Phone usage legend clips "Antigravity" at 360px instead of wrapping.
- Loading: list skeleton lines all same width (prototype varies); tool-row skeletons missing; header "Connecting…" should be "Connecting to atlas…"; Hosts group shows "0 of 0" instead of cached hosts.
- Leftover Prompt Book naming/styling in non-screenshotted surfaces: StandbyBar.tsx, cueNumber() in format.ts:82, --stage/--cue/--strike aliases; text-micro, rounded-sm, bg-panel and font-mono latency in HostsPane.tsx, MessageAttachments.tsx, Markdown code-block header. Rename/restyle.
- Real build must pad env(safe-area-inset-top) and the home indicator on phones.
- Harness avatars at 14px hard to read in dark mode; verify blobatar seeds use machine:/harness:/subagent: prefixes.

## Passed
Focus ring 2px --focus offset 2px; transitions 120ms hover/color, 180ms approval/chevron; reduced motion freezes orbs; truncation OK; no horizontal overflow at 360/390; dark contrast matches DESIGN.md pairs.

## Accepted deviations (data limits)
Branch→workspace path, no TTFT/context bar, change-set card and delegated row omitted (put on wire roadmap), header "Needs approval", attention-first sort, one terminal tab, @blobatar/react avatars, static mocks for subagents/library/PRs/MCP/usage/settings/pairing (but fix their bugs now).

## Round 1 fixes

Branch `ui-quiet-native`, based on `b51f14b`. Item-by-item.

### P0

1. **Command palette (position, width, stacking, order)** — Fixed. `.q-palette` now cancels the Tailwind `translate` property, is 620px wide, centred at `top: 96px`, and sits at `z-index: 51` above the scrim. "This thread" is the first group. Recent threads now sit inside the "Threads" group (3 items, current thread excluded), matching the prototype's group shape instead of a separate empty-looking group.
2. **Transcript / dock column** — Fixed. Stream rows are padded (`left/right: 32px` at ≥640px, 0 below) and the virtualizer uses `paddingStart: 28`, `paddingEnd: 20`. Stream rows, approval card and composer share the same 656px column at 1440px and align at 1100px (measured x=572..1068 inside a 540..1100 pane, i.e. 32px gutters on both sides).
3. **Centred sidebar labels** — Fixed. Added `button { text-align: inherit }` to the base layer; nav labels, Settings and host names are left-aligned.
4. **Phone control sizes** — Fixed. The phone shell root now carries `q-app q-ph`, so Details/Review, Deny/Approve, Cancel/Resume now, the pairing switch rows and the pairing CTA render at phone sizes (44px controls, 51×31 switches, centred 16–17px labels).
5. **Open thread not highlighted** — Fixed. `selectedThreadId` is initialised from the route, so deep links and reloads paint the open row before any click.
6. **Inbox separators / inset rows** — Fixed. Row wrappers are inset 6px and separators are drawn from the wrapper (`[data-index]:not(:first-child) > .q-row::before`) at `left: 34px; right: 10px`, hidden around the hovered/selected row.

### P1

- **Composer double focus ring** — Fixed. The textarea's own outline is suppressed; the composer keeps a single inset ring plus halo.
- **Row "more" overdraws the timestamp** — Fixed. The timestamp is hidden while the row action is visible.
- **Phone prose size** — Fixed. The phone override now targets `.stream-prose` as well; body copy is 16/24.
- **Provenance line** — Fixed. A container query drops optional items and the model/folder parts before clipping; at 360px the line is one line.
- **Done thread header** — Fixed. `Snooze` secondary and `Acknowledge [A]` primary actions are back, with the `Finished HH:MM · duration · tokens · cost` receipt.
- **Terminal panel layout** — Fixed. The terminal is a 340px bottom panel and the dock is hidden while it is open.
- **Terminal rendering** — Fixed. Phone pitch is 12px/18px, desktop 12.5px; the font fallback renders `ok` instead of a missing `✓`; the opening state is skeleton text, not a spinner; error/offline overlays use theme tokens and are legible in light mode.
- **Limited / scheduled rows** — Fixed for glyphs and copy. Amber clock/pause glyphs replace the queued orb, and "Limited · needs you" / "Auto-resume scheduled" are used. Resume time is shown when the loaded thread log contains a `limit` item with `reset_at`; summaries on the wire have no resume field, so the inbox cannot show 14:32 before that thread's log is loaded. This is the one item still data-limited.
- **Empty state starter** — Fixed. The empty pane is "Start a thread" with host, harness and folder pickers plus the composer; the inbox empty state is "No threads yet" with a New thread action.
- **Offline banner / duplicate draft line** — Fixed. The offline banner is neutral; the duplicate draft sentence under the composer is gone and the placeholder carries it.
- **Error state** — Fixed. The inbox shows "Couldn't reach buildbox. Showing 3 of 4 hosts. Retry", the header says "Host unreachable", and the failed thread renders without a composer. The screenshot settle was raised so the post-reconnect banner is captured.
- **Phone Snoozed row** — Fixed. 16px label, 66px row.
- **"Always allow"** — Fixed as far as the wire allows. It is a quiet `fg-2` trigger with a `▾`; it stays disabled with an explanatory title because approval scopes are not on the wire.
- **Shortcut chips** — Fixed. Compact SVG key glyphs (`⌫`, `↵`, arrows) with platform-consistent `Ctrl` labels; sidebar reads `Ctrl K`.
- **Phone sheet copy** — Fixed. The sheet says "Claude Code · Fix flaky auth test"; "subagent of" is dropped when the requester is the thread's own agent.
- **Usage chart stacking** — Fixed. Series render bottom-up once (Claude → Codex → OpenCode → Antigravity) with the rounded cap on the top segment.
- **Usage limit meter at 100%** — Fixed. The danger tone is applied to the full meter, not only `.full`.
- **WSL indentation** — Fixed. Ubuntu-24.04 and Debian are indented under atlas.
- **Subagents inspector** — Fixed. Credential is one line, the selected running node has the 3px budget bar, and Open thread / Cancel subagent / Delegate use enabled styling.
- **Editor** — Fixed. "Hand to agent" and the "since HH:MM" scope line are present, the selected file in Agent changes is highlighted, and tabs no longer show a permanent `×`: an unsaved buffer shows a dirty dot and the `×` appears only on hover or keyboard focus. The IDE screenshot leaves a real trailing-space edit so the dirty state is captured, as in the reference.
- **Keyboard** — Fixed. The selected row is the listbox Tab stop (`tabindex=0`, one row only), ↑/↓ move focus and selection, Enter opens, and per-row hidden action buttons were removed from the tab order so Tab leaves the list instead of walking hidden "more" buttons. Palette is centred (410,96, 620 wide at 1440px), above its scrim, and clickable; focus rings are visible on every control.

### P2

- **Icons** — Fixed. Filter uses lines, MCP uses the plug, Editor uses `</>`, terminal uses `>_`, failed checks use the circled ✕.
- **Providers probe glyphs** — Fixed. Ok-coloured check circles.
- **Phone back / usage legend** — Fixed. Back reads `‹ Inbox`; the 360px usage legend is a two-column grid and Antigravity is not clipped.
- **Loading state** — Fixed. Skeleton line widths vary, tool-row skeletons are present, the header reads "Connecting to atlas…", and Hosts shows cached "3 of 4".
- **Leftover Prompt Book naming** — Fixed. `StandbyBar` is `ApprovalCard`, `cueNumber()` is gone, the `--stage/--cue/--standby/--strike/--prompt` aliases are removed, `.text-micro` is `.text-small`, and `bg-panel` / `rounded-sm` / font-mono latency usages were replaced with Quiet-native tokens and surfaces. The old `h4rn:cue` event is `h4rn:turn` and the shortcut reads "Previous / next message".
- **Safe-area insets** — Fixed. `env(safe-area-inset-top)` and the home-indicator inset are applied to the phone shell, tab bar, dock and sheets.
- **Avatar legibility / seeds** — Fixed. Subagent seeds use `subagentSeed(...)` with `kind="subagent"`; machine and harness seeds are prefixed and verified.

### Verification

- Screenshots: 56 files, desktop 1440×900 and phone 390×844 plus a 360×780 set for the six phone screens, light and dark, captured with `apps/ui/scripts/screenshots.mjs`.
- 1100px gutter check: stream content 572..1068 inside 540..1100 (32px each side); composer and approval card share x=572 / width 496.
- Keyboard pass: sidebar → Inbox → segmented tabs → roving listbox (Tab stop, ↑/↓, Enter) → thread pane → composer; palette opens centred and its items are clickable; every stop shows the 2px focus ring.
- Commands: `bun run typecheck` pass; `bun run lint` pass (20 existing `noDescendingSpecificity` warnings, 0 errors); `bun run test` 73/73 pass; `bun run build` pass; `bun run e2e` (from `apps/ui`, with cargo on PATH) pass.
