# UI fidelity review, round 2 (Opus) — ui-quiet-native @ 9e490a2

Score: 8.0/10 (r1: 6.5). Six structural P0s fixed and hold at 1100px. Some "Fixed" claims did not match rendered output: always re-measure in the browser.
Comparison images: /tmp/claude-1000/-home-chark1es-Github-h4rn/85f8c258-4684-4a92-bc1a-5898c7809f0a/scratchpad/cmp/ ; live captures: .../scratchpad/live/

## Round-1 items not fully fixed
- Selected row: `.q-row.sel .q-row-time{visibility:hidden}` + always-visible `···` shows ··· instead of "2m". Prototype has no row "more" button and keeps time on the selected row.
- Provenance: at 1440 on a Done thread path cut mid-word with no ellipsis ("/home/you/code/desl") because header carries three tool icons the prototype doesn't show there.
- Terminal pitch is 24px at 390/360 (xterm lineHeight:1.5 multiplies cell height), desktop ~21px vs ~19 reference; mock results print plain "ok" instead of green ✓.
- Empty starter: native <select>s and text input; composer lacks attach/@/model and uses "+" send; copy column ~200px narrower than composer ~360px; missing "Pair a device" footer line.
- Offline banner wraps to two lines at 1440 ("Offline · threads as of / 01:59" + Reconnect icon); header says "Host unreachable" in offline (regression).
- Error state header still shows Stop and tool icons for an unreachable host.
- "Always allow" uses `disabled` → `.q-btn[disabled]{opacity:.45}` reads greyed out instead of an fg-2 trigger.
- Shortcut chips regressed (P0-1). Avatar legibility regressed (P1-2).

## P0
1. Every shortcut chip breaks onto two lines ("Ctrl" above the glyph on Deny/Approve on every approval card, desktop and 1100); palette shows tofu for Enter. Causes: Tailwind preflight `svg{display:block}` overrides the inline glyph → `.q-kbd-glyph{display:inline-block}` in quiet.css near line 212; CommandPalette.tsx:130 renders `{MOD} {ENTER}` as mono text → use `<KeyGlyphs value={`${MOD} ${ENTER}`}/>`. Palette shortcuts must be `.q-kbd` chips (one per key, sans 500 11px/16px, inset --line ring) like the prototype's `.kbd`, not 11.5px mono `.q-pal-kbd` (quiet.css:1829). Same for "Go to Working · Alt 1".

## P1
1. ThreadHeader.tsx:67: when stale && working → "Last seen running". "Host unreachable" only when the host itself is offline (error state).
2. Avatars on white tiles in dark mode: EntityAvatar.tsx maps machine→"squircle", harness/user→"circle"; Blobatar background is a light plate. Use `background={false}` like the prototype; if legibility needs help, a 1px --line ring or --fill plate, never white.
3. Selected row timestamp: drop `.q-row.sel` from the hide rule (quiet.css:701-703); show `.q-row-more` only on :hover/:focus-within.
4. Terminal pitch: terminal-controller.ts:97 lower lineHeight until measured row pitch is 18px on phone (~1.1–1.15 at 12px) and ~18–19px desktop at 12.5px. Measure `.xterm-rows > div` spacing. Restore green ✓ in mock (render ✓ via an available font or color "ok" with --ok).
5. Empty state (AppShell.tsx:82,101): replace native selects/folder input with prototype chip pickers (24px, 6px radius, avatar + label + 12px chevron, mono folder). Reuse the real Composer (attach, @, model, ↑ send). Text block and composer one centered column (290px→full). Restore "No host yet? Pair a device or run `h4rn host` on the machine."
6. OfflineBanner.tsx: one line ~32px; plain-text "Reconnect" no icon; shorten "threads as of" to fit the 212px list width.
7. Locked pairing switch: PairingPage.tsx:76 sets disabled but phone switch renders full strength → `.q-switch[disabled]{opacity:.5}`.
8. Palette focus restore: after Esc focus lands on <body>; return it to the element focused before Ctrl K.

## P2
- Palette list overflows (scrollHeight 949 vs 470), last row cut in half: trim default list to prototype groups (This thread / Threads / Go to) or snap max-height to whole 34px rows. "Go to Working/Done" rows lack icons and start at a different x; use status glyphs for thread rows, not blobatars; drop dividers between groups.
- Done and error headers: hide terminal/subagents/changes icons (and Stop when host unreachable). Show "Done · 3h ago".
- Inbox segmented control: one Tab stop with ←/→ between tabs.
- Arrow-key focus moves --sel off the open thread: keep --sel on the open row; focused row shows the ring only.
- "Always allow": aria-disabled + title at fg-2 instead of [disabled] .45 opacity.
- Approval in-flight: label "Approving…" instead of both buttons fading.
- Terminal scrollbar ~6px with --fill; phone key bar ←/→ as SVG like ↑/↓.
- Editor: "Hand to agent" to the right of the Edit / vs HEAD / vs agent control.
- Phone pairing: avatar 48px (not 56), 8px between avatar and name, remove "›" from "Pair this phone". Phone approval sheet: Folder value 13px mono. Phone usage stat tiles +4px between value and caption.

## To reach 10
Fix P0-1 and P1-1..8 (~9.3), then all P2. Re-capture screenshots and re-measure terminal pitch, palette list and chip widths in the browser instead of trusting status notes.

## Round 2 fixes

All values below were measured in headless Chromium against `bun run dev -- --port 1532` with `?mock=1` after the fixes, not taken from status notes. Fresh captures live in `apps/ui/screenshots/` (56 files: 1440×900, 390×844 and 360×780, light + dark). `design/a-quiet-native/shots` stayed the reference.

### Round-1 items not fully fixed

- **Selected row time / `···`.** `.q-row.sel` removed from the hide rule; `.q-row-more` is hover/focus-within only. Measured on `thr_a`: time text `2m`, `visibility: visible`, 18 px wide; more button `opacity: 0`.
- **Done provenance cut mid-word.** Done/error headers no longer carry tool icons; `.q-prov .br .q-mono` is `min-width:0; overflow:hidden; text-overflow:ellipsis`. `inbox-done-1440` renders the path in full at 1440 with the header state/actions still on one row.
- **Terminal pitch / mock result glyph.** `lineHeight` is `1.18` below 640 px and `1.12` above it. `.xterm-rows > div` offsetTop deltas: **19 px at 1440**, **18 px at 390**, **18 px at 360**. The mock prints a green `ok` instead of `✓`: Geist Mono's loaded subsets and the installed fallback fonts have no U+2713 (font checks report it as tofu), and the brief allowed the coloured `ok` fallback.
- **Empty starter.** Rebuilt in `AppShell.tsx`: 420 px single column, chip pickers, real composer. Measured: box **420 px**; chips **79 / 125 / 133 × 24 px**, radius 6 px, `white-space: nowrap`; composer **420 × 93 px** with attach, @, model dropdown and ↑ send. The "No host yet? Pair a device or run `h4rn host` on the machine." footer is restored and always shown, matching `scrEmpty`.
- **Offline banner wraps + header regression.** Inbox banner measured **32 px high, `scrollHeight = clientHeight = 32`, one line** ("Offline · cached 02:47 Reconnect"). Header reads "Last seen running" while stale (`offlineHeaderState` contains that label, not "Host unreachable"); the thread-level explanation banner is explicitly allowed to wrap (`.q-banner.wrap`, 2 lines, full copy) like the prototype.
- **Error header Stop + icons.** `showToolIcons` is false for Done and unreachable; Stop now also requires `!unreachable`. `state-error-1440` shows only state and the `···` menu.
- **"Always allow" greyed out.** Uses `aria-disabled` (no DOM `disabled`). Measured: `aria-disabled=true`, `disabled=false`, `opacity: 1`, colour `--fg-2`.
- **Shortcut chips / avatars.** See P0-1 and P1-2.

### P0

1. **Shortcut chips** — one `.q-kbd` chip per key, `.q-kbd-glyph { display:inline-block }`. Approval card at 1440: four chips, each **16 px high** with `scrollHeight = clientHeight = 16` (`Ctrl` 28.9 × 16, glyph 19 × 16). Palette: every `.q-pal-kbd` group is an inline flex of per-key chips, **16 px high** (e.g. `Ctrl ⇧ N` = three chips, 91.1 px total). The phone approval card intentionally has no key hints (touch sheet, no hardware keyboard); the desktop/1100 cards are the measured target.

### P1

1. **Stale header copy** — stale + working renders "Last seen running"; "Host unreachable" is reserved for the error/unreachable state. Verified in `state-offline-1440` and `state-error-1440`.
2. **Avatars on white plates** — `EntityAvatar` uses `background={false}` for every kind. Dark-mode probe finds the Blobatar `<img>` with `background-color: rgba(0,0,0,0)` and a transparent parent; avatars read as faces, not badges (see `inbox-1440-dark`).
3. **Selected row timestamp** — fixed and measured above.
4. **Terminal pitch and green result** — fixed and measured above; scrollbar is `6px`/`--fill` in `quiet.css`, and the phone key bar ←/→ are 1 SVG each (probe: Up/Down/Left/Right `svgs: 1`).
5. **Empty state** — fixed and measured above.
6. **Offline banner** — fixed and measured above.
7. **Locked pairing switch** — `.q-switch[disabled]` is `opacity: .5`; probe reads `0.5` for "View threads".
8. **Palette focus restore** — `onCloseAutoFocus` returns to the last focused element outside the dialog. Probe: focus `thr_a` row → Ctrl K → Esc → activeElement is again `.q-row.sel` with `data-thread-id="thr_a"` (was `BODY`).

### P2

- **Palette overflow.** Resting list trimmed to the prototype's short list (2 recent threads, 10 rows, 3 headings) and `.q-pal-list` snapped to whole 34 px rows. Measured `scrollHeight 454 = clientHeight 454`; 10 rows at 34 px; last row bottom 596 vs list bottom 602 (6 px padding). The long tail (6 destinations + View group, 5 recent threads) appears once the query is non-empty and then scrolls normally.
- **Palette row art/grouping.** Thread rows use `AgentGlyph` status glyphs (not blobatars); Go-to rows have their own icons and the same 10 px inset; no separators between groups.
- **Done/error headers.** Tool icons hidden on Done and unreachable; Stop hidden for unreachable. Done renders "Done · 3h" (`inbox-done-1440`).
- **Segmented control.** One roving tab stop, ←/→ move focus and selection. Probe: one element with `tabIndex=0`; ArrowRight focuses and selects "Done 3".
- **Arrow-key focus.** Arrow moves DOM focus only. Probe: `activeElement = thr_d`, `.q-row.sel` stays `thr_a`.
- **Always allow.** `aria-disabled` + `--fg-2`, no `[disabled]` opacity (measured `opacity: 1`).
- **Approval in flight.** Approve's label becomes "Approving…" with `aria-disabled=true`; button opacity stays 1 instead of fading both actions.
- **Terminal scrollbar / key bar.** 6 px `--fill` scrollbar; ←/→ are SVG icons like ↑/↓ (probe counts one SVG each).
- **Editor "Hand to agent".** Moved to the right of the Edit / vs HEAD / vs agent control (`ide-1440`).
- **Phone pairing.** Avatar **48×48** (transparent); 8 px avatar→name gap (`.q-pair-hero .ttl { margin-top: 8px }`); no `›` in "Pair this phone" (probe: `false`). Phone approval Folder value is 13 px mono. Phone usage stat value/caption gaps are now 6 px / 4 px (`p-usage-390`).

### Verification

From `apps/ui` with `PATH=$HOME/.cargo/bin:$HOME/.bun/bin:$PATH`:

- `bun run typecheck` — pass.
- `bun run lint` — pass (0 errors; the remaining diagnostics are the pre-existing `noDescendingSpecificity` warnings in `quiet.css`/`terminal.css`).
- `bun run test` — 9 files, 73 tests passed.
- `bun run build` — pass (existing chunk-size warning only).
- `bun run e2e` — "UI e2e passed: thread streamed, settled, and restored after reload."
- Screenshots re-shot with `node scripts/screenshots.mjs http://127.0.0.1:1532`: 56 files captured, 0 failures; each screen compared against `design/a-quiet-native/shots`.
