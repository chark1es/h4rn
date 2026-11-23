---
name: h4rn — Quiet native
description: Direction A for the h4rn redesign. A calm, system-native control surface for coding agents across machines.
status: approved visual language · Layout v2 (one sidebar, one right panel) · prototype at design/a-quiet-native/index.html
colors:
  light:
    bg: "oklch(0.995 0.0015 260)"
    bg-side: "oklch(0.970 0.0025 260)"
    bg-raised: "oklch(1 0 0)"
    bg-sunk: "oklch(0.978 0.002 260)"
    fill: "oklch(0.955 0.003 260)"
    fill-2: "oklch(0.925 0.004 260)"
    hover: "oklch(0.962 0.003 260)"
    sel: "oklch(0.940 0.004 260)"
    line: "oklch(0.915 0.003 260)"
    line-2: "oklch(0.860 0.004 260)"
    fg: "oklch(0.210 0.006 260)"
    fg-2: "oklch(0.440 0.007 260)"
    fg-3: "oklch(0.510 0.007 260)"
    accent: "oklch(0.540 0.180 258)"
    accent-hi: "oklch(0.490 0.180 258)"
    accent-fg: "oklch(0.995 0 0)"
    accent-tx: "oklch(0.500 0.170 258)"
    accent-soft: "oklch(0.955 0.024 258)"
    focus: "oklch(0.600 0.170 258)"
    warn: "oklch(0.500 0.110 62)"
    warn-dot: "oklch(0.760 0.150 72)"
    warn-soft: "oklch(0.968 0.030 80)"
    ok: "oklch(0.490 0.120 150)"
    ok-dot: "oklch(0.640 0.150 150)"
    danger: "oklch(0.520 0.190 27)"
    danger-soft: "oklch(0.966 0.020 25)"
    add-bg: "oklch(0.966 0.028 150)"
    add-tx: "oklch(0.450 0.120 150)"
    del-bg: "oklch(0.966 0.020 25)"
    del-tx: "oklch(0.500 0.180 27)"
    syn-kw: "oklch(0.470 0.150 300)"
    syn-str: "oklch(0.480 0.110 150)"
    syn-fn: "oklch(0.470 0.130 250)"
    syn-cm: "oklch(0.520 0.010 260)"
  dark:
    bg: "oklch(0.190 0.004 260)"
    bg-side: "oklch(0.163 0.004 260)"
    bg-raised: "oklch(0.232 0.005 260)"
    bg-sunk: "oklch(0.172 0.004 260)"
    fill: "oklch(0.240 0.005 260)"
    fill-2: "oklch(0.285 0.006 260)"
    hover: "oklch(0.222 0.005 260)"
    sel: "oklch(0.252 0.006 260)"
    line: "oklch(0.268 0.005 260)"
    line-2: "oklch(0.345 0.006 260)"
    fg: "oklch(0.945 0.003 260)"
    fg-2: "oklch(0.760 0.005 260)"
    fg-3: "oklch(0.640 0.006 260)"
    accent: "oklch(0.560 0.170 258)"
    accent-hi: "oklch(0.610 0.170 258)"
    accent-fg: "oklch(0.995 0 0)"
    accent-tx: "oklch(0.760 0.120 258)"
    accent-soft: "oklch(0.275 0.050 258)"
    focus: "oklch(0.700 0.140 258)"
    warn: "oklch(0.830 0.120 80)"
    warn-dot: "oklch(0.790 0.140 76)"
    warn-soft: "oklch(0.265 0.035 75)"
    ok: "oklch(0.780 0.130 150)"
    ok-dot: "oklch(0.700 0.140 150)"
    danger: "oklch(0.730 0.150 25)"
    danger-soft: "oklch(0.262 0.040 25)"
    add-bg: "oklch(0.245 0.032 150)"
    add-tx: "oklch(0.790 0.130 150)"
    del-bg: "oklch(0.245 0.032 25)"
    del-tx: "oklch(0.750 0.140 25)"
    syn-kw: "oklch(0.780 0.110 300)"
    syn-str: "oklch(0.790 0.110 150)"
    syn-fn: "oklch(0.790 0.100 250)"
    syn-cm: "oklch(0.620 0.010 260)"
  chart:
    light: ["#2a78d6", "#eb6834", "#1baf7a", "#6250d6"]
    dark: ["#3987e5", "#d95926", "#199e70", "#9085e9"]
typography:
  ui: "Geist, ui-sans-serif, system-ui, -apple-system, 'Segoe UI', sans-serif"
  mono: "'Geist Mono', ui-monospace, 'SF Mono', Menlo, Consolas, monospace"
  scale:
    caption: "11px / 14px, 500"
    small: "12px / 16px, 400–500"
    body: "13px / 18px, 400–500"
    prose: "14px / 22px, 400"
    title: "15px / 20px, 600, -0.01em"
    heading: "17px / 22px, 600, -0.01em"
    page: "20px / 26px, 600, -0.015em"
    figure: "22px / 28px, 600, -0.015em"
    code: "12–12.5px / 19–20px mono"
  phone:
    body: "16px / 22px"
    prose: "16px / 24px"
    meta: "13–14px / 18–20px"
    nav-title: "17px / 22px, 600"
    large-title: "30px / 36px, 600, -0.02em"
    tab-label: "11px / 13px, 500"
spacing: [2, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48]
rounded:
  xs: "4px   kbd, badges, inline code"
  sm: "6px   buttons, inputs, nav items, tabs"
  md: "8px   list rows, code blocks, banners, segmented track"
  lg: "12px  cards, composer, approval, popovers, phone groups"
  xl: "14px  command palette, phone sheet (top corners)"
  full: "switches, dots, avatars"
elevation:
  flat: "panes, rows, buttons, cards in the page plane"
  pop: "0 1px 2px oklch(0.2 0.01 260/.06), 0 10px 28px -10px oklch(0.2 0.01 260/.22)"
  dialog: "0 2px 8px oklch(0.2 0.01 260/.08), 0 28px 64px -16px oklch(0.2 0.01 260/.32)"
motion:
  dur-1: "120ms  hover, press, colour"
  dur-2: "180ms  focus, segmented change, switch, approval resolve, palette in"
  dur-3: "240ms  sheet, panel reveal"
  ease-out: "cubic-bezier(0.22, 1, 0.36, 1)"
  ease-in: "cubic-bezier(0.4, 0, 1, 1)"
---

# h4rn: Quiet native

This is a redesign. The current h4rn look, with its prompt-book metaphor, amber cue gutter, stage-black rail, orange logo block, numbered rows and pill-tag headers, is an anti-reference here and nothing from it is carried over. What carries over is product truth: the Working / Done / Settled lifecycle, "where it runs" provenance, host-local secrets, reversible actions, honest harness differences, and the state vocabulary in `docs/ux.md`.

**Mode:** Operate. The person using h4rn is supervising work, so ambition shows up as precision and restraint. The world contributes type, palette, density and one signature move. It never changes navigation or controls.

**Signature move:** *Blue means you.* The single accent appears only where the user is the subject: the needs-you dot in the list, the primary action that answers the agent, focus, and selection. Everything the agents do on their own stays graphite. If you look at any screen and ask "what needs me?", the answer is whatever is blue.

## Why it looks like this (10 points)

1. **The old look sold a metaphor. This one sells the work.** The stage-manager costume (cues, standby, a black rail) made you learn theatre to read an inbox. Here, structure comes from the platform's own patterns: one sidebar, the detail, one inspector, sheet, tab bar. A fluent Mac, Windows or iOS user can tell where everything is before they notice the styling.
2. **One accent, and it only ever means "you".** Agents running, queued or done stay neutral. Approvals, needs-you dots, focus and the primary button are blue. That makes colour a sorting tool and not decoration, and a screen with no blue on it is a screen that needs nothing from you.
3. **Status colours are semantic and rare.** Amber marks a limit or a pause (time is involved). Red marks failure or an unreachable host. Green appears only as a pass or a healthy dot. Each one is always paired with a word or glyph, never shown as colour alone.
4. **Hairlines, not boxes.** Panes are divided by 1 px lines and a single tonal step (sidebar vs content). The only bordered objects are the things that really are objects: a change set, a composer, an approval request, a resume card. There is no card-in-card anywhere.
5. **Typography does the hierarchy.** There is one sans (Geist) at 13 px body, 14 px for agent prose and 15/17/20 px for titles, with weight steps of 400, 500 and 600 only. There are no uppercase eyebrows, kickers or numbered rows. Mono is limited to code, paths, commands, model slugs, terminal output and diffs, where character alignment carries meaning.
6. **Provenance is plain text, not tag soup.** "atlas / Claude Code / claude-sonnet-5-5 / h4rn/fix-flaky-auth" sits as one quiet line under the title, and narrow widths drop items from the right. Identity glyphs (blobatars) appear only where *who* matters: hosts, harnesses, subagents and reviewers.
7. **Liveness is small and truthful.** A 14 px orb moves only while a turn is actually doing work. Queued breathes, waiting is a still ring, and finished, limited or offline states are static glyphs. Motion never claims work that isn't happening, and under reduced motion every orb freezes into a still frame without losing any information.
8. **The approval is a request, not an alarm.** It is a single raised card above the composer, with no amber band and no modal. It names who is asking, where the command runs and in which folder, shows the exact command, and offers Deny and Approve with their shortcuts. A subagent's request surfaces on its parent, so the inbox stays one row per job.
9. **Density where you're competent, calm where you decide.** Lists, tables, tool calls and trees are dense: 28 px tool rows, 40 px table rows, two-line thread rows. Decision points get whitespace: the approval card, the resume countdown and the pairing consent.
10. **The phone is an iPhone/Android app, not a small desktop.** It uses a large title, a segmented control, 66 px rows, a five-item tab bar with a badge, a pushed thread with a back button, a sheet for approval, grouped inset lists for consent, a key bar for the terminal, and primary actions in the bottom third.

## Colour

Strategy: **Restrained.** Graphite neutrals tinted toward 260° (cool, not blue), one accent at 258°, and four semantic states. Light and dark are tuned separately and are never inverted.

### Roles

| Token | Job |
|---|---|
| `bg` | Content surface (list, detail, pages). |
| `bg-side` | The second neutral layer: app sidebar and phone tab bar. One step darker than content, never coloured. |
| `bg-raised` | Things above the plane: popovers, palette, approval card, composer, secondary buttons. |
| `bg-sunk` | Code, diffs, terminal, tool output. Recessed reading surfaces. |
| `fill` / `fill-2` | Segmented track, user message bubble, inline code, skeletons, inactive switch. |
| `hover` / `sel` | Row hover and selected row. Selection is neutral; the needs-you dot carries the colour. |
| `line` / `line-2` | Hairline dividers / control outlines and tree connectors. |
| `fg` / `fg-2` / `fg-3` | Primary text / metadata / tertiary (times, units, placeholders). `fg-3` is the floor. |
| `accent` (+`-hi`, `-fg`) | Primary button fill, switch on, checkbox on, needs-you dot, tab badge. |
| `accent-tx` | Accent as text: "Needs approval", links, phone nav buttons. |
| `accent-soft` | Selection highlight, active key on the phone key bar. |
| `focus` | 2 px focus ring at 2 px offset. |
| `warn` / `warn-dot` / `warn-soft` | Usage limit, scheduled resume, conflicts that block an apply. |
| `ok` / `ok-dot` | Passing check, healthy host dot, `+` in diffs. |
| `danger` / `danger-soft` | Failed turn or check, unreachable host, revoke. |
| `add-*` / `del-*` | Diff bands and signs. The `+`/`−` sign column is mandatory. |

### Checked pairs (WCAG 2.x, computed from the OKLCH values)

| Pair | Light | Dark | Use |
|---|---|---|---|
| fg / bg | 17.47 | 15.72 | body |
| fg-2 / bg | 7.66 | 8.60 | metadata |
| fg-3 / bg | 5.67 | 5.49 | tertiary floor |
| fg-3 / bg-side | 5.27 | 5.75 | sidebar counts |
| fg-3 / sel | 4.82 | 4.73 | meta on a selected row |
| fg-3 / bg-sunk | 5.39 | 5.67 | line numbers, terminal dim |
| accent-fg / accent | 5.10 | 4.67 | primary button label |
| accent-tx / bg | 6.05 | 8.58 | "Needs approval", links |
| accent-tx / sel | 5.15 | 7.39 | needs-you on selected row |
| accent-tx / accent-soft | 5.38 | 6.90 | active key, selection |
| warn / bg | 6.11 | 10.83 | "Resumes 14:32" |
| warn / warn-soft | 5.64 | 9.01 | conflict text |
| ok / bg | 5.84 | 9.70 | passing check |
| danger / bg | 5.99 | 7.24 | failure text |
| danger / danger-soft | 5.44 | 6.14 | error banner |
| add-tx / add-bg | 6.44 | 8.73 | diff `+` |
| del-tx / del-bg | 5.90 | 6.95 | diff `−` |
| syn-cm / bg-sunk | 5.17 | 5.24 | code comments (lowest syntax pair) |
| focus / bg | 3.95 | 6.87 | focus ring (non-text, ≥ 3) |

### Charts

Charts use a categorical order validated with the dataviz validator (CVD ΔE ≥ 9.2 on adjacent pairs, normal-vision ≥ 24.6, both modes). The colours stay fixed per harness: Claude Code blue, Codex orange, OpenCode aqua, Antigravity violet. The default slot 4 (yellow) was skipped on purpose because it collides with the amber status colour. Light-mode aqua sits below 3:1 against the surface, so every chart ships a legend with values and a "View as table" toggle. Bars are 2 px apart with a 3 px rounded top, the grid is recessive, there's a hover tooltip per day, and there is only ever one axis.

## Typography

**Geist** carries the whole UI, and **Geist Mono** carries code. They share metrics, so a path inside a sentence doesn't jump. Production may substitute the platform face (SF Pro / Segoe UI Variable) on desktop and must use the platform face on native phone builds. The scale is built to survive that swap.

- Tabular figures everywhere (`font-feature-settings: "tnum"`), so tokens, costs, timers and counts never jitter.
- Agent prose is 14/22 with a 720 px column (about 70 characters). UI text is 13/18. Phone prose is 16/24.
- Tracking is negative only at 15 px and up (−0.01 to −0.02 em). No uppercase.
- Section headings are 14/600 in sentence case. **No eyebrow above a heading, ever.**
- Mono is never decorative. It's used only for code, diffs, terminal output, file paths, shell commands, model slugs, branch names and URLs.

## Space, radius, elevation

- 4 px base. Component internals use 6, 8, 10 or 12. Groups are separated by 16, 24 or 32. Page sections use 36–48. Headings have more space above than below.
- Desktop shell (Layout v2): sidebar 288 (resizable 240–420), main flexible (min 560), right panel 400 (resizable 320–640, closed by default), terminal drawer 300 high. The prose column is `max-width: 720px`. See **Layout**.
- Radii follow a fixed scale: 4, 6, 8, 12, 14, plus full rounding for switches, dots and avatars. A radius above 14 px on a working surface is decoration.
- Elevation is declared once, either as a border or a shadow, never both, with one exception: the approval card and popovers combine a hairline with a soft `pop` shadow, because they really do sit above the page. The palette and phone sheets use `dialog` plus a scrim. Rows, panes and buttons never cast shadows.

## Layout

Three columns at most, and only one of them is on the left.

```
┌────────────────────┬──────────────────────────────────────┬────────────────────────┐
│ sidebar            │ main                                 │ right panel            │
├────────────────────┼──────────────────────────────────────┼────────────────────────┤
│ h4rn            ▢ ✎│ Title      Thread|IDE  >_  ▢         │ Changes Files Preview │
│ Search or run    ⌘K│ provenance                           │                        │
│Working Done Settled │                                      │ vs agent|vs HEAD  turns│
│ ▣ api  atlas      ⌄│ stream  ─or─  IDE editor             │ file list              │
│   ● Fix flaky…   2m│                                      │ stacked diffs          │
│   ◌ Add jitter…  9m│ approval card                        │   ─or─ Files tree      │
│ ▣ relay  orion    ⌄│ composer                             │   ─or─ Subagents       │
│   …                │ ─ terminal drawer (optional) ─       │                        │
│ ● 3 of 4 hosts ⑂3 ⚙│                                      │ Revert all… Open in IDE│
└────────────────────┴──────────────────────────────────────┴────────────────────────┘
```

**Panel rules**

1. **One left column, always the sidebar.** Nothing else docks on the left: not a second list, not a settings nav, not a file tree. Destinations that used to have their own list column either take over the sidebar (Settings, including MCP servers and Skills, swap the thread list for a settings nav with a back button) or render their list in the main area (Pull requests).
2. **The main area shows one thing:** the selected thread, the selected thread's project in the IDE, or a page (Usage, a pull request, a settings page). The Thread | IDE toggle sits in the same place in both views.
3. **One right panel, scoped to the selected thread.** Tabs: **Changes** (agent diff, by turn, or vs HEAD), **Files** (the thread's workspace tree), **Preview** (one sandboxed HTML file the agent wrote), **Subagents** (tree + details). It is closed by default; its open/closed state, width and last tab are remembered per window, and its content follows whatever thread is selected. It never holds a second thread, a terminal, or app navigation.
4. **The terminal is a drawer in the main area**, under the composer, so an approval card is never pushed off screen by it. It belongs to the thread's worktree and appears in both views.
5. **Only blue can survive a collapse.** Hiding the sidebar leaves a show-sidebar button in the main header; if any thread needs you, that button carries the accent pip. A collapsed project group shows its thread count and the accent dot. The Subagents tab shows the accent dot when a child is waiting on an approval.
6. **Resizing** uses an 8 px hit area on the shared hairline. On hover, drag or keyboard focus the hairline turns into a 2 px `focus` line. Arrow keys move it 16 px. Widths persist per window. The main area never goes below 560 px; past that the right panel closes before the sidebar does.

**Sidebar anatomy (top to bottom).** Wordmark, hide-sidebar and new-thread buttons · "Search or run ⌘K" · the Working / Done / Settled segmented control · project groups · the Snoozed row (Working only) · the footer.

**Project groups.** A project is a workspace: one folder on one host. The same repository on two hosts is two projects. Each group header has a 16 px neutral monogram (no colour, because colour belongs to blobatars and to "you"), the name at 13/600 in `fg-2` (`fg` when the selected thread is in it), and the host in `fg-3` whenever your projects span more than one host, which includes WSL distros. A chevron on the right collapses the group, and a `+` that appears on hover starts a thread in that project. Groups appear in the order of their most urgent thread, then by recent activity. Rows inside a group keep the approved row anatomy, and line 2 drops the host because the header above it already shows it.

**Why a segmented control and not T3's stacked sections.** h4rn's lifecycle is a filter on attention, not a shelf. With Working / Done / Settled as a segment, the project grouping stays one level deep, and each project appears once on screen instead of once per section. Done stays a visible debt with its own count. Snoozed is the last row of Working (a sheet or popover on click), and Settled pages older threads behind "Show older".

**Projects.** A "Projects" heading sits above the groups with two quiet icons: a sort menu (projects by Recent activity, Date added, Name or Manual; threads by Recent activity or Name, stored per device) and Add project. Add project is a dialog with three choices, as in T3 Code: *Open folder* (browse the host's folders), *New folder* (name, location, optional `git init`) and *Clone repository* (HTTPS/SSH URL or `owner/repo`, location, folder name). A one-line mono summary always says what will happen ("Creates ~/code/demo and runs git init"). The daemon keeps registered projects in `projects.json`, so a project shows before its first thread ("No threads yet. Start one"). Each group header has a ⋯ menu (New thread here, Rename…, Copy path, Remove from sidebar; folders that only have threads offer Keep in the sidebar) and can be dragged to reorder, which switches the sort to Manual. Removing never touches the folder.

**Footer.** `● 3 of 4 hosts ▾` opens the hosts menu (presence, latency, route, Pair a device…, Manage hosts), followed by icon buttons for Pull requests, Usage and Settings (MCP servers and skills live under Settings → Tools). Each has a tooltip with its shortcut and a palette entry. This replaces the old nav list and the Hosts group.

**IDE view.** ⌘E switches the main area to the IDE for the **selected thread's project**: editor tabs, path bar with `Edit | Diff` and one source picker (Uncommitted, Turn, Branch, Commits), the scope strip ("Changes by Claude Code in *Fix flaky auth test* since 12:03 · Revert file · Accept"), the code view, and the status bar. The file tree lives in the right panel's **Files** tab, not in a second left column. Selecting a thread in another project switches the IDE to that project's workspace and restores its open tabs ("Restored 3 tabs" appears quietly at the end of the tab strip). Tabs are kept per project. The header names the workspace (monogram, host, path) and carries a **thread chip** with the state glyph and title ("Needs approval" in accent when it applies). Clicking the chip, or pressing ⌘E, returns to the thread. The thread's context stays reachable in three places: the chip, the scope strip, and the Changes tab, which in IDE view collapses to a file list because the editor shows the diff. While the agent is still writing a file, the strip says so and Accept waits until the turn ends. A thread without a workspace shows "No files open in *project*" with ⌘P and Show files.

### Shortcuts

Every command is ⌘ (Ctrl on Windows and Linux) plus a key, with optional ⇧ or ⌥. There are no single-key shortcuts and no `G`-sequences, so typing can never run a command. The registry is `apps/ui/src/lib/keybindings.ts`; Settings → Shortcuts rebinds any of them per device, and tooltips, the palette and the ⌘/ sheet all read the current binding.

| Keys | Action |
|---|---|
| `⌘K` | Search or run |
| `⌘/` | Keyboard shortcuts |
| `⌘⇧N` | New thread |
| `⌘,` | Settings |
| `⌘⇧L` | Toggle light and dark |
| `⌘⇧H` | Go to threads |
| `⌘⇧P` | Go to pull requests |
| `⌘⇧U` | Go to usage |
| `⌘⇧M` | Go to MCP servers and skills |
| `⌘1` | Show Working |
| `⌘2` | Show Done |
| `⌘3` | Show Settled |
| `⌘⌥↓` | Next thread |
| `⌘⌥↑` | Previous thread |
| `⌘L` | Focus the composer |
| `⌘↵` | Approve pending action |
| `⌘⌫` | Deny pending action |
| `⌘.` | Interrupt turn |
| `⌘[` | Previous message |
| `⌘]` | Next message |
| `⌘⇧S` | Settle thread |
| `⌘⇧A` | Acknowledge (Done) |
| `⌘⌥S` | Snooze for an hour |
| `⌘⌥P` | Keep in Working |
| `⌘⌥G` | Share with guest |
| `⌘⌥U` | Reopen a settled thread |
| `⌘U` | Attach files |
| `⌘B` | Toggle sidebar |
| `⌘⌥B` | Toggle right panel |
| `⌘E` | Switch thread and IDE |
| <code>⌘`</code> | Toggle terminal |
| `⌘⇧D` | Show changes |
| `⌘J` | Show subagents |
| `⌘P` | Find a file |
| Esc | Close palette or dialog / leave Settings |

## Motion

| Token | Value | Use |
|---|---|---|
| dur-1 | 120 ms | hover, press, colour |
| dur-2 | 180 ms | focus, segmented change, switch thumb, approval card resolving, palette pop |
| dur-3 | 240 ms | phone sheet rise, panel reveal |
| ease-out | `cubic-bezier(0.22, 1, 0.36, 1)` | entrances |
| ease-in | `cubic-bezier(0.4, 0, 1, 1)` | exits |

- One authored moment: **resolving an approval**. The card fades and drops 6 px over 180 ms, then collapses to a one-line receipt with Undo. Nothing else on the screen moves.
- Live motion is limited to the orb (1.4 s orbit), the streaming caret (1 s blink), and the skeleton pulse (1.6 s, opacity only).
- `prefers-reduced-motion: reduce` sets every duration to 1 ms and stops all animation. Orbs show a static frame with the particle at 50°, the caret stays solid, and skeletons sit at 70% opacity. Information never depends on motion.

## Components

**Sidebar.** The only left column (see **Layout**). The wordmark "h4rn" is set in 14/600 text, with no logo block. Hide-sidebar and new-thread buttons sit beside it, then "Search or run ⌘K", the Working / Done / Settled segment, the thread list grouped by project, and a 44 px footer with the hosts menu and the secondary destinations. On `bg-side`, the selected row uses `fill-2`. Rows have no hairlines in the sidebar, because the group headers carry the rhythm.

**Thread row.** The row has two lines. Line 1 has a state slot (14 px glyph in a 16 px column, aligned under the project monogram), the title (13/500), and the relative time. Line 2 has the harness and the status word on the right. The host comes from the group header; anywhere a row appears outside a group (palette, PR linked thread) it reads "project · host". States: blue dot for needs you, orb for running, breathing orb for queued, amber clock for scheduled resume, amber pause for limited needing you, red wifi-off for host unreachable, check-circle for done, faint check for settled (with a dimmed title). In the sidebar, Done rows add an inline `Acknowledge (A)` action when selected, and a limited-needs-you row always shows `Resume now`. In standalone lists, separators are inset hairlines that disappear around hovered or selected rows.

**Segmented control.** Used for Working / Done / Settled, MCP / Skills, ranges, and diff mode. It has a 2 px inset track and a raised thumb. Counts are shown in `fg-3`.

**Thread header.** Title (15/600), one provenance line, the state (orb or glyph plus a word), then the view-specific actions (Stop and overflow; Snooze and Acknowledge on Done; Un-settle on Settled), a hairline divider, and the fixed view cluster: `Thread | IDE` segment, Terminal, and Right panel. The cluster is always at the right edge, so it doesn't move between views. When the sidebar is hidden, show-sidebar and new-thread buttons lead the header. Below 820 px of main width the model drops from the provenance line, and below 640 px the branch drops too.

**Stream.** User messages are a right-aligned `fill` bubble with attachments and time beneath. Agent turns start with a small header (harness blobatar, "Claude Code on atlas · 12:03") and no bubble. Tool calls are 28 px rows (status glyph, verb, mono target, measured result) that expand into a bounded mono output well. A change set is the one bordered block in the stream: a header with totals and Review, file rows, and the first hunk inline. A delegated subagent appears as one bordered row that links into the tree. The live tail ends with a caret and a quiet "Writing · 61 tok/s" line.

**Approval card.** It sits above the composer and contains a shield icon, "Codex on orion wants to run a command", provenance (subagent, folder, worktree), the exact command in a mono well, "Always allow ▾" on the left, and Deny and Approve on the right with shortcuts. It is never a modal. Offline, it stays visible with disabled actions and the sentence "Reconnect to approve or deny. Nothing runs until you do."

**Composer + usage line.** A 12 px-radius field that names its target ("Reply to Claude Code on atlas"), with attach, mention, a model menu, and a send button that turns blue only when there is something to send. Underneath is one 11.5 px line: tokens · cost est. · tok/s · TTFT · active time, plus a context bar on the right. Cost is always labelled as an estimate.

**Right panel.** 60 px header aligned with the thread header's hairline, holding text tabs with a 2 px `fg` underline (not accent) and counts in `fg-3`, plus a close button. Under it is a 44 px tool bar per tab, a scrolling body, and an optional footer.
- *Changes:* `vs agent | vs HEAD` segment, a turn scope menu ("All 3 turns ▾": all turns, latest turn, turn n, branch vs base), and totals. Then a "Claude Code on atlas · 2 files since 12:03" line, the file list (mono path with a dimmed directory, `New` badge, +/−), and stacked diffs with sticky file headers whose long lines wrap. The footer has Revert all… and Open in IDE ⌘E. In IDE view the diffs fold away and the file list opens files in the editor.
- *Files:* filter field (⌘P), the workspace tree with git markers (`M` in `warn`, `A` in `ok`), and an "Agent changes" row pinned to the bottom that jumps to Changes. An `.html`/`.htm` row carries a quiet "Open preview" action.
- *Preview:* the file name, a reload button and a close button, then the page in a sandboxed frame on the page's own ground. A quiet notice sits above it when workspace assets were skipped, and a link clicked in the page raises a small inline bar ("Open example.com in your browser?" Open / Cancel) instead of opening by itself. There is no browser chrome: the page runs scripts but cannot reach h4rn, its cookies or its storage.
- *Subagents:* the tree and details below. Empty states name what will appear there.

**Subagents tab.** A real tree. The parent sits at the top, and children hang off 1 px elbow connectors. Each node shows state, title, elapsed time, a "host · harness" line with blobatars, a state/turn/token line, and a 3 px budget bar while running. Failed or cancelled children stay in the tree with their reason. The selected node expands into a key/value block (runs on, model, context handoff, budget, approvals, credential location).

**Terminal drawer.** A 300 px bottom drawer in the main area, under the composer on a `bg-sunk` ground, following the theme rather than being a forced black terminal. It has session tabs, the size and viewer count, split and close controls. Prompts and branch names use `accent-tx`, passes and failures use `ok` and `danger`, and dim output uses `fg-3`.

**IDE view.** Described under **Layout**. The file tree is the right panel's Files tab. Document tabs have a dirty dot, a path bar with an `Edit | Diff` segmented control, a source picker (Uncommitted, Turn, Branch, Commits), the base or commit for the wider scopes, and "Hand to agent", a scope strip that says whose changes these are and since when (with Revert file / Accept), and a code view with full-width diff bands and a sign column. The status bar is 26 px and neutral.

**MCP & skills.** Lives in Settings under *Tools*. The sidebar's settings nav lists the servers under "MCP servers" with a health glyph each (check, red x, amber key), and the main area shows the selected server. The detail starts with a credential row showing "Stored on atlas" / "Stored on orion" badges (key icon, never a value). The core is a **Where it runs** table: host, harness, delivery mechanism, status, switch. Unsupported targets stay in the table, disabled, with the reason. Conflicts are an amber-soft inline block with a single Resolve action. A "Shared with you" section names the host it comes from and the transport path.

**Usage.** The auto-resume card comes first because it's the only thing that might need you. It shows a live countdown, Fail over…, Cancel auto-resume, and Resume now. Next is a six-column stat strip separated by hairlines, where each changing number carries a 56 px sparkline of real history. This is not the hero-metric template: there are no big tiles and no accent. Then the stacked bar chart with legend totals and a table toggle, active vs idle as one proportion bar with both numbers in text, limit meters (red at 100% plus "Limited", amber at 78%, neutral otherwise) with reset times, and a per-model table with right-aligned tabular numbers where an unknown price shows "—" instead of $0.00.

**Pull request.** No list column: the main area shows the PR with a breadcrumb ("Pull requests / 3 open / dev/api") that opens the list page. The sidebar keeps the thread list. Header (title, number, state badge, `head → base`, commits, author, Open on GitHub), text tabs with a 2 px underline in `fg` (not accent), a conversation with review comments that quote their diff context, and per-comment Reply / Hand to agent buttons. Hand to agent opens a popover: this thread or a new thread, a note that the agent won't push without approval, and send. The right column shows checks (glyph, name, required, duration, View log, Re-run failed), reviewers, and the linked thread (a link that selects it in the sidebar).

**Settings.** Takes over the sidebar instead of adding a column. It has a back button with "Settings" as the title, "Search settings", and nav groups (General, Appearance, Notifications, Shortcuts / *Machines*: Hosts & devices, Harnesses, Providers / *Tools*: MCP servers, Skills / *Account*: Account & relay). Esc or ‹ returns to the threads. The footer stays the same. The main area is a 760–880 px form column made of label/control rows separated by hairlines.
- *Hosts:* a table with WSL distros indented under their Windows host as separate rows. Host detail lists harnesses with version, state, credential source, and Restart/Start.
- *Hosts & devices:* one page for the machines that run agents and the devices that supervise them: Machines, Hosts, Devices and Connection. Pair a device (or `/settings/devices`) opens a pair panel at the top with the QR code (on white in both themes so it scans), an 8-character code, a live expiry countdown, scope checkboxes, Copy link and New code. A paired-devices table offers per-device Revoke.
- *Providers (CLIProxyAPI):* base URL and Probe, the probe results as glyph + key + value, two separate secret rows (management key, access key), a models table with aliases and a custom alias badge, per-harness override switches (off means keep the harness's own credential) with the exact config path and a redacted preview diff, a verified test request, and "Apply and restart" that names the turn it will interrupt.

**Command palette.** 620 px, 14 px radius, `dialog` shadow over a scrim. It has a context chip for the focused thread. The "This thread" group comes first (approve, deny, changes, subagents, interrupt). Next is View (open *project* in the IDE, right panel, terminal, sidebar), then Threads and New thread in *project*, then Go to (Pull requests, MCP & skills, Usage, Hosts, Pair a device, Settings). Secondary destinations are always reachable here. Every row shows its shortcut, and the footer explains the keys and which hosts are being searched.

**States.**
- *Empty (no projects):* the sidebar hides the lifecycle segment and says "No projects yet" with what a project is and "Add a project…". The footer shows the one host. The main area becomes "Start a thread" with host, harness and folder pickers and a composer; the folder becomes the first project.
- *Loading:* layout-shaped skeletons (project headers with monogram blocks, two-line rows), segment counts shown as "—", the footer shows a queued orb with "Connecting…", and the header says "Connecting to atlas…". No spinners.
- *Offline:* a neutral banner at the top of the sidebar list ("Offline · as of 12:04 · Reconnect"), the footer reads "Offline", the thread stays readable with "Stream paused at 12:04", the approval is visible but disabled with a reason, and the composer saves a draft.
- *Error:* partial failure is stated as such in the sidebar ("Couldn't reach buildbox. Showing 3 of 4 hosts. Retry"), and the hosts menu shows the unreachable host in `danger` with its last-seen time. A thread that can't load gets a centred message that names the host, says whether the run may still be active, offers Retry and Open host, and has a Details disclosure (route, last event, status, correlation ID, Copy diagnostics).

### Phone

- **Shell.** Status-bar safe area, a nav bar with 44 px targets, a large title on top-level screens, and a 5-item tab bar (Inbox with badge, PRs, Usage, Hosts, Settings) with 24 px icons and 11 px labels. Compose lives in the nav bar, not in a floating button.
- **Inbox.** A segmented control under the large title, then the same project groups as desktop: a 40 px header (20 px monogram, name 15/600, host in `fg-3`, chevron), 66 px rows (title 16/500, harness and status word on line 2), and Snoozed as a row that opens a sheet.
- **Thread.** A pushed screen with a "‹ Inbox" back button, Terminal, Changes and More in the nav bar, a 20/600 title, and a provenance + state line. The "2 files changed" card and the delegated-subagent row open the panel sheet. The stream sticks to the live tail. A compact approval card sits above the composer with Details / Review (44 px). The usage line sits under the composer.
- **Approval.** A bottom sheet with a grabber: who and where, the full command wrapping at word boundaries, a grouped list (folder, worktree, asked, "Always allow in this thread" switch), and Deny | Approve as two 44 px buttons in the bottom third.
- **Changes / Subagents (the right panel on phone).** One large-detent sheet over the thread, with a `Changes | Subagents` segment (the accent dot shows when a child waits on you) and Done. Changes shows the scope menu and totals, the file list as a grouped inset list, and the first file's diff with wrapped lines; tapping a file pushes its full diff. Subagents shows the tree as a grouped list with children indented, and the waiting child highlighted in `accent-soft` with Details / Review (44 px). Files and the IDE are desktop-only on phone, and the sheet says so.
- **Terminal.** A full screen with the host, cwd and grid size, a "Sized to this phone" note, and a scrollable 44 px key bar (Esc, Tab, Ctrl latched, arrows, Paste).
- **Pairing.** A consent screen: host identity (56 px blobatar), how the connection is made, signed-in identity, a grouped list of scope switches ("View threads" is locked on), included WSL hosts, a line saying credentials stay on the host, the expiry countdown, and a full-width "Pair this phone".
- **Usage.** The paused-thread card first, with its countdown as the largest number on screen. Then a 2×2 stat group, one chart at a time behind a segmented control, and limit meters.

## Identity and state

- **Blobatars** (placeholder drawn in the prototype; production uses `blobatar@2.7.0`). Machine is a squircle, harness a circle, subagent a free shape. They're 14–18 px in rows, 32 px in host headers and 56 px on the phone pairing screen. They're always static, and they always sit next to the name, which carries the accessible label. Seeds follow the type-prefixed scheme in the old DESIGN.md (`machine:`, `harness:`, `subagent:`), so WSL distros get their own faces.
- **Orbs** (placeholder; production uses `thinking-orbs`). Inline size is 14 px, with 20 px reserved for a large thread header. Running = orbit, queued = breathe, waiting for approval = still ring. Every terminal state is a static Lucide glyph instead. There's one orb per row and they're never used as a loader.
- **Icons:** Lucide at 1.5 px stroke, 14/16 px in UI, 22–24 px on phone. Keyboard glyphs (⌘ ↵ ⌫ ⇧) are drawn as SVG so they render regardless of the font.

## Removed from the previous system, and why

| Removed | Reason |
|---|---|
| Prompt Book / stage-manager metaphor, cue/standby/strike vocabulary | It added a layer of translation between the user and the state words already defined in `docs/ux.md`. |
| Amber cue gutter and numbered turns (01, 02…) | Row numbers carried no information, and amber attention bars made every thread look urgent. |
| Stage-black rail and status bar in both themes | It was branding through chrome. The new silhouette comes from typography and the single accent. |
| Orange "h4" logo block | The wordmark is undecided (`PRODUCT.md`), so lowercase text is honest until a mark exists. |
| Pill/chip rows for host, harness, model, branch | Replaced by one provenance line, which reads faster and can truncate cleanly. |
| Uppercase tracked labels | Replaced by sentence-case section headings. |
| Mono for times, counts and cue numbers | Tabular figures in the sans do this job better. Mono is reserved for code. |

## Open questions for the owner

1. Accent hue: 258° blue is the most "native" choice. A greener or more indigo accent would be more ownable, but would weaken the "Blue means you" rule's familiarity with the OS.
2. Should the terminal follow the theme (as prototyped) or stay dark in both, as before?
3. Resolved by Layout v2: hosts moved from the sidebar into the footer's hosts menu. Still open: should a pinned-projects section sit above the groups once someone has more than about 8 projects?
4. Should "Done" auto-collapse project groups that only hold acknowledged-soon threads, or is the segment count enough?

## Layout v2 (Nov 13, 2026)

The owner asked for T3 Code's single left panel: one left panel, one collapsible right panel, and an IDE view that follows the selected thread's project. The visual language (tokens, type, hairlines, blue means you, orb and blobatar rules) is unchanged. Only the frame moved.

- **Merged nav and inbox into one sidebar.** Two left columns (212 + 328) became one 288 px column. Threads are grouped by project, and the project is the unit the IDE opens, so grouping and IDE switching use the same key. A project is a folder on a host, so a repo checked out on atlas and orion is two groups with the host shown subtly. That keeps "where it runs" honest at the group level instead of repeating the host on every row.
- **Kept h4rn's lifecycle as a segment instead of T3's stacked Active / Working / Snoozed / Settled sections.** h4rn has a Done state that T3 lacks, and stacking three sections under project folders would show each project up to three times. The segment keeps one level of grouping.
- **Moved secondary destinations to the footer, the palette and Settings.** Pull requests, Usage, MCP & skills and Settings are a click or two keys away, and none of them claims a column. Settings (with MCP servers and Skills inside it) temporarily takes over the sidebar, as T3's settings route does. Hosts and devices became a footer menu that still shows presence at a glance.
- **Put the file tree in the right panel.** In IDE view the tree is the Files tab. A narrow tree pane inside the main area would have recreated the two-left-columns look the owner rejected, and the same Files tab also serves thread view, so the tree has one home. ⌘P makes the tree optional.
- **Scoped the right panel to the thread.** Changes, Files, Preview and Subagents all answer "what has this thread touched or spawned?". The terminal stays a bottom drawer (as in T3) because it is interactive, wide, and belongs under the conversation, not beside it.
- **Mapped the right panel to one sheet on phone** (Changes | Preview | Subagents) rather than two pushed screens. The thread stays one swipe away and the pattern matches the approval sheet.
- **Kept attention from hiding.** The accent survives every collapse: the pip on the show-sidebar button, the dot on a collapsed project group, the dot on the Subagents tab, and the "Needs approval" thread chip in IDE view.
