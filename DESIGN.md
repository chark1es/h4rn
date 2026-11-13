---
name: h4rn
description: A cross-harness, cross-machine control surface for coding agents — the prompt book above the work.
colors:
  paper: "oklch(0.975 0.004 250)"
  panel: "oklch(0.955 0.006 250)"
  raised: "oklch(0.992 0.002 250)"
  ink: "oklch(0.22 0.018 255)"
  ink-muted: "oklch(0.435 0.014 255)"
  ink-faint: "oklch(0.545 0.012 255)"
  line: "oklch(0.88 0.008 250)"
  control-border: "oklch(0.545 0.012 255)"
  stage: "oklch(0.20 0.022 255)"
  stage-ink: "oklch(0.94 0.004 250)"
  stage-ink-muted: "oklch(0.72 0.010 250)"
  cue: "oklch(0.64 0.15 68)"
  cue-ink: "oklch(0.45 0.12 55)"
  cue-tint: "oklch(0.945 0.045 75)"
  standby: "oklch(0.50 0.10 155)"
  standby-ink: "oklch(0.41 0.09 155)"
  standby-tint: "oklch(0.93 0.035 155)"
  strike: "oklch(0.52 0.19 27)"
  strike-ink: "oklch(0.46 0.18 27)"
  strike-tint: "oklch(0.94 0.035 25)"
  prompt: "oklch(0.48 0.12 250)"
  prompt-tint: "oklch(0.94 0.025 250)"
  dark-ground: "oklch(0.16 0.014 255)"
  dark-panel: "oklch(0.19 0.016 255)"
  dark-raised: "oklch(0.235 0.016 255)"
  dark-ink: "oklch(0.93 0.005 250)"
  dark-ink-muted: "oklch(0.72 0.012 250)"
  dark-ink-faint: "oklch(0.58 0.012 250)"
  dark-line: "oklch(0.30 0.014 255)"
  dark-control-border: "oklch(0.58 0.012 250)"
  dark-stage: "oklch(0.115 0.012 255)"
  dark-cue: "oklch(0.78 0.14 70)"
  dark-cue-fill: "oklch(0.72 0.15 70)"
  dark-cue-tint: "oklch(0.26 0.05 70)"
  dark-standby: "oklch(0.74 0.12 155)"
  dark-standby-tint: "oklch(0.25 0.045 155)"
  dark-strike: "oklch(0.70 0.16 27)"
  dark-strike-tint: "oklch(0.25 0.045 25)"
  dark-prompt: "oklch(0.72 0.10 250)"
typography:
  display:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.75rem"
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.375rem"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "-0.015em"
  title:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "1.125rem"
    fontWeight: 600
    lineHeight: 1.3
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: "0em"
  compact:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "0em"
  label:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.71875rem"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "0.08em"
  micro:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "0.6875rem"
    fontWeight: 500
    lineHeight: 1.2
    letterSpacing: "0.02em"
  code:
    fontFamily: "Spline Sans Mono, ui-monospace, SFMono-Regular, Cascadia Mono, monospace"
    fontSize: "0.78125rem"
    fontWeight: 400
    lineHeight: 1.6
    letterSpacing: "0em"
rounded:
  xs: "2px"
  sm: "4px"
  md: "6px"
  lg: "10px"
  xl: "14px"
  full: "999px"
spacing:
  1: "4px"
  2: "8px"
  3: "12px"
  4: "16px"
  5: "20px"
  6: "24px"
  8: "32px"
  10: "40px"
  12: "48px"
  16: "64px"
components:
  button-primary:
    backgroundColor: "{colors.cue}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 12px"
    height: "30px"
  button-secondary:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 12px"
    height: "30px"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.sm}"
    padding: "0 8px"
    height: "30px"
  input:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 10px"
    height: "30px"
  nav-rail:
    backgroundColor: "{colors.stage}"
    textColor: "{colors.stage-ink}"
    height: "100%"
    width: "56px"
  thread-row:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    height: "48px"
  badge-cue:
    backgroundColor: "{colors.cue-tint}"
    textColor: "{colors.cue-ink}"
    rounded: "{rounded.sm}"
    padding: "0 6px"
    height: "20px"
  standby-bar:
    backgroundColor: "{colors.cue-tint}"
    textColor: "{colors.ink}"
    height: "40px"
  card:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "12px"
  dialog:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    width: "560px"
---

# Design System: h4rn

## Overview

**Creative North Star: "The Prompt Book"**

A stage manager's prompt book is the working document that makes a live performance repeatable: every cue numbered, every standby called, every entrance and light change annotated in pencil, and a spine that survives the run. It is not a script and not a control panel. It is a high-density, low-drama instrument for the one person responsible for the whole show.

h4rn borrows that world's **type, palette, density and one signature move** — never its page layout, navigation or controls. The shell is standard and learnable: a rail, a list, a detail pane, a context panel, shadcn controls. The world shows up in a stage-black rail that stays black in both themes, a cool paper ground, cue amber reserved for the one thing that needs a decision, and a **cue gutter** that numbers every turn and a **standby bar** that names the next required action. Nothing glows, nothing bounces, nothing is decorated as a theatre prop.

The result should feel like an instrument left running on a desk: quiet at rest, exact under load, and legible at a glance from across the room. Density is a feature. Colour does jobs: cue amber = attention and primary action; standby green = ready, scheduled, healthy; strike red = stopped, failed, destructive; prompt blue = information and links. The default surface is neutral and cool, not warm paper and not a black terminal.

**Key Characteristics:**
- Stage-black chrome in both themes, cool paper content surfaces, hairline seams.
- One working accent (cue amber) used at ≤5% of pixels; state colours are semantic, never decorative.
- Archivo for UI, Spline Sans Mono for identifiers, times and cue numbers; tabular figures everywhere.
- High information density with a strict 4 px rhythm; no card-on-card nesting.
- Cue gutter + standby bar as the only signature interaction; everything else is a familiar control.
- Flat by default; depth is a seam and one tonal step, not a shadow.
- Terminal states are static glyphs, live states are thinking orbs; identity is a blobatar, never a robot icon.

## Colors

The palette is a working wardrobe: a constant black rail, a cool neutral stage, and four state inks that only appear when they mean something. It is **Restrained** overall and **Committed** at the shell — the rail is stage black on every screen in both themes, which gives the product a recognisable silhouette with no ornament.

### Primary
- **Cue Amber** (`oklch(0.64 0.15 68)`, dark `oklch(0.78 0.14 70)`): the primary action, the current cue in the gutter, an approval that is waiting, and focus rings. Filled amber always carries ink text in light theme (`4.98:1`) and ground text in dark (`7.60:1`).
- **Cue Ink** (`oklch(0.45 0.12 55)`): amber used as *text* on paper (`7.21:1`) and inside cue-tint panels (`6.57:1`). Never use raw cue amber for small text on paper.

### Secondary
- **Standby Green** (`oklch(0.50 0.10 155)`, dark `oklch(0.74 0.12 155)`): scheduled auto-resume, healthy host, passing check, successful completion. Used as a dot, a 3 px bar or tinted badge; text uses `standby-ink` (`7.86:1`).
- **Strike Red** (`oklch(0.52 0.19 27)`, dark `oklch(0.70 0.16 27)`): failed turn, unreachable host, destructive action, denied approval. Solid red is reserved for confirmation; ordinary destructive affordances are outline + red text.
- **Prompt Blue** (`oklch(0.48 0.12 250)`, dark `oklch(0.72 0.10 250)`): informational callouts, links inside prose, "where it runs" provenance links.

### Neutral
- **Paper** (`oklch(0.975 0.004 250)`): content ground in light theme. Cool, not cream.
- **Panel** (`oklch(0.955 0.006 250)`): list pane, sidebars, toolbars — one step cooler/darker than content.
- **Raised** (`oklch(0.992 0.002 250)`): inputs, popovers, cards, the one step above paper.
- **Ink** (`oklch(0.22 0.018 255)`): primary text; `16.11:1` on paper.
- **Ink Muted** (`oklch(0.435 0.014 255)`): metadata, secondary labels; `7.38:1`.
- **Ink Faint** (`oklch(0.545 0.012 255)`): placeholders and disabled-adjacent labels; `4.61:1` — the floor, never lighter for text.
- **Line** (`oklch(0.88 0.008 250)`): decorative seams and dividers. **Control Border** (`oklch(0.545 0.012 255)`) is the ≥3:1 border for inputs and toggles.
- **Stage** (`oklch(0.20 0.022 255)`, dark `oklch(0.115 0.012 255)`): the rail, status bar and scrim ground. Stage stays black in both themes; it is the constant shell.

### Dark theme mapping

The same roles invert onto a near-black blue-green ground: `dark-ground`, `dark-panel`, `dark-raised`, `dark-ink`, `dark-ink-muted`, `dark-line`, `dark-control-border`. Cue amber brightens rather than darkening; green and red lift to remain legible on the dark ground. Never derive dark colours by inverting light ones — the state hues are separately tuned.

### Checked pairs

| Pair | Ratio | Use |
|---|---|---|
| ink / paper | 16.11 | body text |
| ink-muted / paper | 7.38 | metadata |
| ink-faint / paper | 4.61 | placeholder floor |
| cue-ink / cue-tint | 6.57 | cue badge text |
| standby-ink / standby-tint | 6.96 | healthy badge text |
| strike-ink / strike-tint | 6.45 | failed badge text |
| paper / strike | 5.65 | text on solid red |
| stage-ink / stage | 15.18 | rail labels |
| dark-ink / dark-ground | 15.80 | body text (dark) |
| dark-ink-muted / dark-ground | 7.83 | metadata (dark) |
| dark-cue / dark-ground | 9.46 | cue text (dark) |
| dark-cue / dark-cue-tint | 7.63 | cue badge text (dark) |
| dark-standby / dark-standby-tint | 7.18 | healthy badge text (dark) |
| dark-strike / dark-strike-tint | 5.67 | failed badge text (dark) |
| dark-control-border / dark-ground | 4.54 | control border (dark, ≥3) |
| dark-ground / dark-cue-fill | 7.60 | primary button (dark) |
| stage-ink-muted / stage | 7.30 | inactive rail labels |
| cue / paper | 3.23 | focus ring (non-text, ≥3) |

### Named Rules

**The One Cue Rule.** Amber is attention, not decoration. It appears on the primary action, the current cue, a waiting approval, and the focus ring — nowhere else. If a screen has two amber fills competing, one is wrong.

**The Stage Black Rule.** The rail and status bar are stage-black in both themes. Do not tint them with the accent, do not make them translucent, do not invert them in dark mode. The silhouette is the brand.

**The State Never Alone Rule.** No state is carried by colour only. Every orb, dot, badge and diff line is paired with a word, an icon, or a `+`/`−` sign. Colour is the fast channel, not the only channel.

## Typography

**Display / UI Font:** Archivo (with `ui-sans-serif, system-ui, sans-serif`)
**Mono Font:** Spline Sans Mono (with `ui-monospace, SFMono-Regular, Cascadia Mono, monospace`)

**Character:** Archivo is a workhorse grotesque with slightly condensed proportions and a tall x-height — plain enough for a table, distinctive enough to give labels a printed-cue-sheet crispness. Spline Sans Mono has a humanist quirk that keeps IDs and timestamps from reading as a generic developer terminal. One family carries the interface; the mono carries facts.

### Hierarchy

| Role | Size / line-height | Weight | Purpose |
|---|---|---|---|
| Display | 28 / 32 px (1.75 rem) | 600 | Screen-defining titles and the rare setup hero; never inside a pane. |
| Headline | 22 / 28 px (1.375 rem) | 600 | Pane titles, PR titles, dashboard section leads. |
| Title | 18 / 23 px (1.125 rem) | 600 | Thread title in detail, dialog titles, card headings. |
| Body | 14 / 21 px (0.875 rem) | 400 | Stream prose, descriptions, form values. |
| Compact | 13 / 19 px (0.8125 rem) | 400 | List rows, table cells, tool-call summaries. |
| Label | 11.5 / 14 px (0.71875 rem) | 600, 0.08 em, uppercase | Pane headers, table headers, field groups. |
| Micro | 11 / 13 px (0.6875 rem) | 500, 0.02 em | Cue numbers, timestamps, token counts, shortcut hints. |
| Code | 12.5 / 20 px (0.78125 rem) | 400 mono | Terminal, diffs, IDs, file paths, model slugs. |

### Named Rules

**The Tabular Rule.** Every number that can change — tokens, cost, TPS, TTFT, duration, counts, cue numbers — uses `font-variant-numeric: tabular-nums`. Columns must not jitter as values update.

**The No Display Voice Rule.** Archivo at 600 is the loudest voice allowed in the product. No display serif, no condensed poster face, no italic headings, no all-caps sentences. Uppercase exists only at Label size with tracking.

**The Measure Rule.** Stream prose is capped at 68 ch and centred in the detail pane; documentation-style prose at 72 ch. Code, tables and terminals are exempt and run full width.

## Layout

The shell is a **three-pane instrument**: a 56 px stage-black rail, a 304 px list pane, and a flexible detail pane. A collapsible 420 px context panel (diff / files / terminal / subagents) is the only fourth region, and only on desktop ≥1280 px. It is a tab strip, not a second sidebar.

```
┌ 56 ┬────────── 304 ──────────┬──────────── detail ────────────┬── 420 ──┐
│rail│ list + tabs + filter    │ header / stream / composer     │ context │
└────┴─────────────────────────┴────────────────────────────────┴─────────┘
```

- **Desktop ≥1280 px:** all three panes + optional context panel. The status bar (24 px) spans the bottom with connection, presence and queue depth. No global top bar: each pane owns its header so vertical space goes to content.
- **Tablet 768–1279 px:** rail + list + detail; the context panel becomes a tab *inside* detail, not a column. In landscape, list and detail can split 40/60; in portrait, detail pushes over the list.
- **Phone <768 px:** single column, bottom navigation (Inbox / Hosts / New / Limits / More). Thread detail pushes full-screen with a back affordance; the standby action becomes a sticky card above the composer; context (terminal, diff, subagents) becomes a full-height sheet. Never shrink the three-pane desktop into a phone.
- **Spacing rhythm:** 4 px base; 8/12/16 for component internals, 16/24/32 between groups, 48/64 for screen-level breathing room. One rhythm per pane; no arbitrary 10/18 px gaps.
- **Density:** desktop list rows 48 px (two-line) or 32 px (compact table); thread stream blocks separated by 16 px; tool calls 28 px collapsed. Phone list rows 64 px minimum, touch targets 44×44 px, 16 px page gutters.
- **Cue gutter:** 48 px column at the left of the stream on desktop; each turn carries a two-digit cue number, the current cue has a 2 px amber bar, struck cues are dimmed and marked with a slash. On phone the gutter collapses to a 4 px progress rail plus “Cue 03 / 12” in the header.
- **Stream width:** the readable column is `min(100%, 760px)`; the composer matches it. The context panel and terminal use the full pane width.
- **Breakpoints:** 640 (large phone), 768 (tablet portrait), 1024 (tablet landscape), 1280 (desktop 3-pane), 1600 (wide desktop, context panel default open).

### Named Rules

**The No Fourth Column Rule.** A permanent fourth column is a failure of priority. Context is a tab, a sheet, or a panel the user opens; it never squeezes the stream below 560 px.

**The Thumb Rule.** On phone, the primary action and the standby bar live in the bottom 30% of the screen. Destructive actions never sit next to the primary; they are separated by at least 24 px and require a deliberate press.

**The One Scroll Rule.** Each pane scrolls independently; the page itself never scrolls. On phone, only the content region scrolls — headers and the composer are fixed.

## Elevation & Depth

The system is **flat by default**. Depth is conveyed by tonal steps (paper → panel → raised) and 1 px hairline seams, not by shadow. A panel is a different value, not a floating card. Shadows exist only for things that genuinely leave the page plane: popovers, dialogs, sheets and the command palette.

### Shadow Vocabulary
- **Overlay low** (`0 1px 2px rgb(3 5 9 / 0.18), 0 8px 24px -12px rgb(3 5 9 / 0.35)`): popovers, dropdowns, tooltips with content.
- **Overlay high** (`0 2px 4px rgb(3 5 9 / 0.20), 0 24px 48px -16px rgb(3 5 9 / 0.45)`): dialogs, command palette, phone sheets.
- **No shadow** on rows, cards, panes, buttons or the rail. Hover on a row is a background step, not a lift.

### Named Rules

**The Flat-By-Default Rule.** If an element stays in the page plane, it has no shadow. If it leaves the plane, it gets both a scrim (stage at 40–60% opacity) and one shadow — never a border and a shadow and a tint.

**The Seam Rule.** Dividers are 1 px `line`; selected or active boundaries are 2 px (`cue` for the current cue/tab, `stage` for a pinned divider). Never use a 1 px border plus a shadow to separate two panes.

## Shapes

Corners are **small and precise**: 4 px on controls, 6 px on cards and inputs, 10 px on dialogs, 14 px on phone sheets. Pills are reserved for counts and presence, never for buttons. The dominant silhouette is the rectangle with one hairline; the only angled mark is the 2 px cue tick in the gutter.

- Buttons and inputs: 4 px; height 30 px desktop, 44 px phone.
- Cards, popovers, table containers: 6 px.
- Dialogs, command palette: 10 px.
- Sheets, phone modals: 14 px top corners.
- Blobatars: squircle for machines, circle for harnesses and humans, transparent for subagents.
- Terminal and diff panels: 0 px inside the pane; the pane itself owns the corners.

**The Small Radius Rule.** If a corner radius exceeds 14 px on a working surface, it is decoration. Status pills and avatars are the only full-round shapes.

## Components

Every component is a shadcn/ui primitive with a thin h4rn skin. The vocabulary is deliberately standard; the world lives in type, palette, density and the cue gutter, not in reinvented controls.

### Buttons
- **Shape:** 4 px radius, 30 px high desktop, 44 px phone, 12 px horizontal padding.
- **Primary:** cue amber fill with ink text in light theme, cue-fill with ground text in dark. One per visible region; if two compete, the secondary becomes `secondary`.
- **Hover / Focus:** 90 ms background/border change; hover adds a 1 px ink inset border rather than changing hue. Focus is a 2 px cue ring with 1 px paper offset; `:focus-visible` only.
- **Secondary:** raised fill, 1 px control-border, ink text. **Ghost:** no fill, muted text, panel hover. **Destructive:** strike text + strike-tint hover; solid strike fill only inside a confirmation dialog.
- **Loading:** the label stays, a 14 px orb or a 3-dot inline pulse replaces the icon, width does not change. Never a spinner in a button.

### Inputs / Fields
- **Style:** raised ground, 1 px control-border, 4 px radius, 10 px padding, 30/44 px height. Labels sit above at Label size; no floating labels.
- **Focus:** 1 px cue border + 2 px cue ring at 2 px offset. The caret is ink, the selection is cue-tint.
- **Error:** 1 px strike border, a strike-ink message below with a specific cause, and `aria-invalid`. Disabled: panel ground, ink-faint text, no hover.
- **Secret fields:** show “Stored on atlas” with a `KeyRound` icon and a Replace action; never reveal, never prefill, never echo in errors.

### Tabs
- **Style:** text tabs at Compact size with counts, 2 px cue underline on the active tab, 1 px line under the strip. Working / Done / Settled are the only top-level tabs in Inbox.
- **State:** active = ink + cue underline; inactive = ink-muted; hover = panel ground; focus = cue ring. Counts use micro tabular figures and never animate as a rolling number.

### List rows
- **Thread row:** 48 px desktop / 64 px phone, two lines: title (Compact 500) and meta (Micro: host blobatar, harness blobatar, state orb + word, time). A left 2 px cue bar marks selection; panel background marks hover; a “Needs approval” cue badge sits at the right.
- **Table row:** 32 px, 1 px line separators, sticky Label header, numeric columns right-aligned and tabular.

### Chips & badges
- **Entity chip:** 20 px high, tinted ground, 14 px blobatar + label, 4 px radius. Host, harness, model and branch each get a chip; model and branch use mono.
- **State badge:** cue/standby/strike tint + matching ink + icon. Never a saturated fill except the primary button.

### Cards / Containers
- Used only for limits, usage summaries, pairing and empty states — not as a generic page wrapper. 6 px radius, raised ground, 1 px line, 12–16 px padding. No card inside a card.

### Navigation
- **Rail:** stage black, 56 px, 20 px Lucide icons, 10 px labels only on hover/focus tooltip. Active item: 2 px cue left bar + stage-ink icon. Inactive: stage-ink-muted. Bottom cluster: search, settings, user blobatar.
- **Phone:** bottom navigation, 64 px + safe area, five items (Inbox, Hosts, New, Limits, More). The center New action is a 44 px cue-tint square with a plus; it is not a floating action button.
- **Breadcrumb:** compact, slash-separated, truncates from the middle; host and harness chips follow the title.

### Tables
- Sticky header, 32 px rows, zebra never; hover row = panel, selected = cue-tint at 40% plus a 2 px cue left edge. Sort indicators are 14 px chevrons, always paired with the active sort label. Column visibility lives in a popover, not a settings page.

### Diff
- 12.5 px Spline Sans Mono, 20 px line height, unified by default with a split toggle. Added lines use standby-tint + `+`; removed use strike-tint + `−`; hunk headers use panel + mono. File headers stick. Never rely on red/green alone; the sign column is mandatory. Inline comments are 1 px prompt-blue left borders.

### Terminal
- xterm.js with a token-mapped theme: ground = dark-ground in both themes (a terminal is dark by default; light terminal is an explicit setting), ink text, cue cursor, cue-tint selection, standby/strike for ANSI green/red. 13 px mono, 1.4 line height. The terminal pane has a 28 px header with shell, cwd, host and a font-size stepper. Phone adds a 44 px key bar (Esc, Tab, Ctrl, arrows, paste).

### Agent glyph (thinking orb)
- Wrap `thinking-orbs` in an h4rn `<AgentGlyph>` that owns the mapping table below. Sizes 16 (inline), 20 (row), 28 (thread header), 64 (empty/hero only). Never size 32. Always pair with a text state; `aria-label` names the state and host.
- One orb per visible turn, not per message. Offscreen turns pause via the library; do not mount two orbs in the same row.
- Terminal states are **static glyphs**, not frozen orbs. An orb that never moves again is a glyph with extra steps and a larger performance cost.

| h4rn state | Tab | Orb `state` | Props | Text label | Notes |
|---|---|---|---|---|---|
| Running | Working | `working` | `speed={1}` | `Running` | Canonical live state; particles on orbits. |
| Thinking | Working | `working` | `speed={0.9}` | `Thinking` | Model reasoning; no separate mode. |
| Searching | Working | `searching` | — | `Searching` | Web, grep, glob; scan meridian. |
| Reading | Working | `listening` | `speed={0.9}` | `Reading` | Files/URLs; data flowing in. |
| Editing | Working | `weaving` | — | `Editing` | File writes and diffs being merged. |
| Running tool | Working | `solving` | — | `Running <tool>` | Discrete steps resolving into a fixed state. |
| Queued | Working | `breathing` | `speed={0.5}` | `Queued` | Behind another run; alive, not working. |
| Awaiting approval / answer | Working | `shaping` | `speed={0.8}`, `opts={{shape:0}}` | `Waiting for approval` / `Waiting for answer` | Held circle, not a progress cycle; paired with Go/Hold. |
| Scheduled auto-resume | Working | `breathing` | `speed={0.3}` | `Resumes 14:32` | Clock badge + countdown text; slow stalled breath. |
| Limited, auto-resume off | Done | static limit glyph | — | `Limited · needs you` | No orb; the animation would imply work. |
| Failed | Working | static strike glyph | — | `Failed` | Raised hand; the turn did not complete. |
| Done | Done | static check glyph | — | `Done · unread` | Terminal; acknowledge moves to Settled. |
| Idle | Working (pinned) | `breathing` | `paused` | `Idle` | Only for an explicit “Keep in Working” pin. |
| Host unreachable | Working | static disconnected glyph | — | `Host unreachable` | Animation would lie about liveness. |
| Settled | Settled | static check, dimmed | — | `Settled` | No orb, no motion. |

- **Only `working`, `searching` and `solving` are unambiguous**; the other mappings are design decisions, not library facts. `connecting` is intentionally unused — a transport handshake is too short to read as an animation and is better shown by a disabled control.
- **Reduced motion** collapses every animated row to the library’s static representative frame. Static glyphs are unchanged, so the mapping survives with no alternate table.
- **Never** use an orb as the only state signal, as a page loader, or as a button spinner. Loaders are skeletons or inline text.

### Entity avatar (blobatar)
- Wrap `blobatar` in `<EntityAvatar>`. Static `<img>` by default; `animate="hover"` only on a single large host header; never in lists or tables. Always pass `title` = the human label. Pin `blobatar@2.7.0` exactly, with a golden test for a handful of IDs.
- The face answers **who** is doing it; the orb answers **what** is happening. Never run both at full amplitude in one row: in a list, the avatar is static and the orb is the only motion.

| Entity | Seed string | Background | Sizes | Animation |
|---|---|---|---|---|
| Machine | `machine:<machine_id>` | `squircle` | 20 row, 28 header, 64 profile | static; `hover` only at 64 |
| Harness instance | `harness:<machine_id>:<harness_kind>[:<instance_id>]` | `circle` | 14 inline, 20 row | static |
| Subagent | `subagent:<machine_id>:<parent_thread_id>:<subagent_id>` | transparent | 14 tree row | static |
| Thread (optional) | `thread:<machine_id>:<thread_id>` | transparent | 20 | static |
| Human | `user:<user_id>` | `circle` | 20 rail, 64 profile | static |

**Seed rules**

- **Type-prefix every seed.** A machine, a thread and a user with the same underlying id must not alias to the same face.
- **Stable ids only.** Never put a display name, model, branch or title in the seed; renaming must not change the face.
- **Lowercase from the daemon.** The library normalizes to NFC + trim + lowercase; emitting lowercase keeps server and client in agreement.
- **Keep the parent in a subagent’s seed**, so a child’s face is stable and unique per parent without a registry.
- **Narrow silhouettes by role, never pin a hue.** Machines draw from `round / cloud / sun / capsule`; subagents from `nub / capsule / droplet / triangle`; humans from `round / organic / cloud`. The id still chooses within the set. Do not pin `hue` or `tone` per role, and never pass `palette` (it bypasses the library’s 4.5:1 eye/body contrast guarantee).
- **`title` is mandatory** and is the human label (“atlas”, “Claude Code”, “Reproduce CI failure”). Mark the avatar `aria-hidden` when an adjacent text label already names it.
- **Windows and WSL are different entities.** A WSL distro host gets `machine:<wsl_id>`, not the Windows host’s id.

### Cue gutter
- 48 px column; one cell per turn; two-digit mono number; a 2 px amber bar marks the current cue; struck cues are ink-faint with a diagonal slash; clicking or `[`/`]` jumps to the turn. Hover reveals a 1-line cue summary. It is a navigation index, not a progress bar.

### Standby bar
- 40 px strip above the composer, cue-tint ground, 3 px cue left bar. One line: what is waiting + the consequence + a countdown if any. Actions on the right: primary `Go` (approve/resume), secondary `Hold` (deny/cancel), overflow for details. On phone it becomes a 2-line card with 44 px buttons. Only one standby bar exists per thread; queued items collapse into “+2 more”.

### Toasts
- Bottom-right desktop, above the phone nav. 4 s, one action maximum. **Never** use a toast for an error that requires a decision — that is a standby bar or an inline alert. Success toasts are silent by default for auto-save; explicit actions may confirm.

### Motion tokens

| Token | Value | Use |
|---|---|---|
| `dur-instant` | 90 ms | hover, press, colour feedback |
| `dur-fast` | 150 ms | focus ring, row select, tooltip, tab underline |
| `dur-base` | 200 ms | pane reveal, diff expand, approved cue resolution |
| `dur-slow` | 250 ms | sheet/drawer, phone push, context panel |
| `ease-out` | `cubic-bezier(0.22, 1, 0.36, 1)` | entrances, reveals |
| `ease-in` | `cubic-bezier(0.4, 0, 1, 1)` | exits, dismissals |
| `ease-standard` | `cubic-bezier(0.65, 0, 0.35, 1)` | resize, reorder, layout |
| `spring-sheet` | stiffness 420, damping 40, mass 0.9 | phone sheets only |

- Transitions animate `transform` and `opacity` only; never `height` on stream content, never `filter: blur` on text.
- Every animation is interruptible: Motion's `layout`/`AnimatePresence` always starts from the current value; no queued sequences.
- Approval resolution: the standby bar collapses in 200 ms while the cue tick moves to the executed state; the row does not slide. One moving element.
- Skeletons are static blocks with a 1.5 s opacity pulse; under reduced motion they are static at 60% opacity.
- **Reduced motion policy:** `prefers-reduced-motion: reduce` sets all durations to 1 ms, disables transforms and smooth scrolling, freezes orbs on their representative static frame, renders blobatars static, and swaps the sheet spring for an instant cut. Information is never carried by motion alone, so nothing is lost.

### Iconography
- **Lucide**, 1.5 px stroke, sizes 14 (inline), 16 (buttons/rows), 20 (rail/nav), 28 (empty states). No filled icons except the 6 px presence dot, which is a shape, not an icon.
- Key mappings: `Inbox` inbox, `Server` host, `Boxes` WSL distro, `Terminal` terminal, `GitPullRequest` PR/MR, `Plug` MCP server, `Blocks` skill, `ChartNoAxesColumn` usage, `Code2` IDE, `Settings` settings, `Command` palette, `Search`, `Plus`, `Check`, `X`, `CircleAlert` error, `TriangleAlert` warning, `Clock` snooze/countdown, `Pause`, `Play`, `RotateCcw` resume/retry, `ShieldCheck` approval, `KeyRound` secret, `Smartphone` device, `QrCode` pairing, `WifiOff` offline, `FileDiff` diff, `ListTree` subagents, `Paperclip` attachments, `GitBranch` branch, `Activity` live.
- **No `Bot` icon.** Agent identity is a blobatar; agent state is an orb or a static status glyph. A robot head is the category costume.

### Component inventory

| h4rn surface | shadcn/ui base | Notes |
|---|---|---|
| App rail | `Sidebar` (collapsed) | Custom 56 px stage skin; tooltips. |
| Thread list | `ScrollArea` + custom `ThreadRow` | Tabs above via `ToggleGroup`. |
| Inbox tabs | `ToggleGroup` | Working/Done/Settled with counts. |
| Filter / sort | `Popover` + `Command` | Multi-select chips. |
| New thread | `Dialog` (desktop) / `Drawer` (phone) | Host/harness/model selects. |
| Composer | `Textarea` + `Popover` | Mentions, slash commands, attachments. |
| Attachments | `Attachment` pattern + `HoverCard` | Image thumbs, file rows, remove. |
| Tool call | `Collapsible` + `Badge` | Collapsed one-liner, expanded args/result. |
| Approval | `Alert` + custom `StandbyBar` | Go/Hold; never a modal by default. |
| Thread stream | `ScrollArea` + custom `Turn` | Cue numbers, orb, timestamps. |
| Diffs | custom `DiffViewer` | Split/unified, file tree, comments. |
| File tree | `Collapsible` + `ScrollArea` | IDE and diff share it. |
| Terminal | custom `TerminalPane` | xterm.js; `Tabs` for sessions. |
| Subagents | custom `SubagentTree` | `Collapsible` rows, orb + blobatar. |
| Hosts | `Table` (desktop) / `Item` list (phone) | Presence, harnesses, WSL. |
| Host detail | `Tabs` + `Card` + `Switch` | Harnesses, resources, providers. |
| Pair device | `Dialog` + `QRCode` | Scopes, expiry, copy link. |
| Devices | `Table` + `AlertDialog` | Revoke, last seen, presence. |
| MCP servers | `Table` + `Switch` matrix | Transport, targets, conflicts. |
| Skills | list + `Sheet` detail | Per-harness toggles, source path. |
| Provider setup | `Stepper` (custom) + `Form` | CLIProxyAPI probe and overrides. |
| Usage dashboard | `Chart` + `Card` + `Table` | KPI strip, series, per-harness. |
| Limits | `Card` + `Progress` + `Countdown` | Reset time, auto-resume, failover. |
| IDE | custom shell + CodeMirror 6 | Tabs, search, diff toggle. |
| PR/MR | `Tabs` + `Table` + `Textarea` | Checks, comments, hand to agent. |
| Settings | `Tabs` + `Form` + `Switch` | Account, devices, notifications, appearance. |
| Command palette | `Command` | 560 px, grouped, keyboard-first. |
| Toasts | `Sonner` | Success/info only. |
| Empty states | custom `EmptyState` | Static orb/glyph + one action. |
| Skeletons | `Skeleton` | Layout-shaped, not a spinner. |
| Tooltips | `Tooltip` | Shortcuts appended in micro. |
| Context menus | `DropdownMenu` / `ContextMenu` | Row actions, never hover-only on phone. |

## Do's and Don'ts

### Do:
- **Do** keep the rail and status bar stage-black in both themes, and let content surfaces step paper → panel → raised.
- **Do** reserve cue amber for the primary action, the current cue, a waiting approval and focus; keep it at ≤5% of pixels.
- **Do** pair every orb, dot and diff colour with a word, icon or sign.
- **Do** use tabular numerals for every changing value, and mono for IDs, times, paths and cue numbers.
- **Do** keep rows dense: 48 px thread rows, 32 px table rows, 28 px collapsed tool calls; 44 px touch targets on phone.
- **Do** use the standby bar for anything that needs a decision, and toasts only for confirmation.
- **Do** animate 150–250 ms, transform/opacity only, interruptibly; cut everything under reduced motion.
- **Do** show provenance at the point of decision: host blobatar, harness chip, model, branch, and “where it runs” for every subagent and MCP target.
- **Do** write error copy as “what happened + what to do”, naming the host and the still-running work.

### Don't:
- **Don't** use a card as a page wrapper, nest cards, or put a shadow on a row, pane or button.
- **Don't** use gradients, glows, glassmorphism, neon-on-black, or a purple “AI” accent.
- **Don't** use pill buttons, oversized radii, display fonts in labels, or uppercase sentences.
- **Don't** use a robot icon, a mascot, or an illustration system for agent identity.
- **Don't** show a spinner in content or a button; use a layout skeleton or an orb.
- **Don't** use saturated red/green as large fills or rely on red/green alone in diffs.
- **Don't** put the primary action and a destructive action within 24 px on phone.
- **Don't** make the phone a scaled-down desktop: no 3-pane miniature, no hover-only actions, no 12 px touch targets.
- **Don't** expose secrets in the client, in errors, in logs or in a “copy config” blob; show “stored on <host>”.
- **Don't** animate layout on scroll, page-load choreography, or more than one moving element per row.
