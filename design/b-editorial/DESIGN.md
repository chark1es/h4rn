---
name: h4rn — Editorial workbench
description: A calm, precise control surface for coding agents across machines.
colors:
  paper: "oklch(97% .009 85)"
  surface: "oklch(99% .004 85)"
  panel: "oklch(94% .013 85)"
  ink: "oklch(25% .018 170)"
  muted: "oklch(46% .018 170)"
  line: "oklch(86% .016 85)"
  edge: "oklch(57% .018 170)"
  accent: "oklch(39% .065 180)"
  on: "oklch(99% .004 85)"
  tint: "oklch(91% .026 175)"
  danger: "oklch(43% .14 27)"
  danger-bg: "oklch(94% .022 27)"
  warn: "oklch(43% .075 65)"
  warn-bg: "oklch(94% .036 85)"
  dark-paper: "oklch(21% .012 170)"
  dark-surface: "oklch(24% .012 170)"
  dark-panel: "oklch(18% .012 170)"
  dark-ink: "oklch(93% .009 85)"
  dark-muted: "oklch(73% .015 150)"
  dark-line: "oklch(35% .016 170)"
  dark-edge: "oklch(61% .018 170)"
  dark-accent: "oklch(79% .075 175)"
  dark-on: "oklch(20% .025 175)"
  dark-tint: "oklch(31% .035 175)"
  dark-danger: "oklch(79% .105 27)"
  dark-danger-bg: "oklch(30% .043 27)"
  dark-warn: "oklch(81% .075 85)"
  dark-warn-bg: "oklch(29% .025 85)"
typography:
  display:
    fontFamily: "Manrope, sans-serif"
    fontSize: "26px"
    fontWeight: 750
    lineHeight: 1.25
    letterSpacing: "-0.035em"
  collection:
    fontFamily: "Manrope, sans-serif"
    fontSize: "23px"
    fontWeight: 750
    lineHeight: 1.25
    letterSpacing: "-0.035em"
  thread:
    fontFamily: "Manrope, sans-serif"
    fontSize: "21px"
    fontWeight: 750
    lineHeight: 1.25
    letterSpacing: "-0.035em"
  heading:
    fontFamily: "Manrope, sans-serif"
    fontSize: "19px"
    fontWeight: 700
    lineHeight: 1.3
    letterSpacing: "-0.025em"
  title:
    fontFamily: "Manrope, sans-serif"
    fontSize: "15px"
    fontWeight: 700
    lineHeight: 1.45
    letterSpacing: "0em"
  body:
    fontFamily: "Source Sans 3, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "0em"
  control:
    fontFamily: "Source Sans 3, sans-serif"
    fontSize: "14px"
    fontWeight: 500
    lineHeight: 1.45
    letterSpacing: "0em"
  compact:
    fontFamily: "Source Sans 3, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "0em"
  micro:
    fontFamily: "Source Sans 3, sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "0em"
  code:
    fontFamily: "Source Code Pro, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.8
    letterSpacing: "0em"
rounded:
  tag: "4px"
  control: "6px"
  group: "8px"
  approval: "10px"
  dialog: "12px"
spacing:
  1: "4px"
  2: "8px"
  3: "12px"
  4: "16px"
  5: "20px"
  6: "24px"
  7: "28px"
  8: "32px"
  10: "40px"
  12: "48px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on}"
    rounded: "{rounded.control}"
    padding: "7px 11px"
    height: "34px"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "7px 11px"
    height: "34px"
  input:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "8px 10px"
  navigation-selected:
    backgroundColor: "{colors.tint}"
    textColor: "{colors.accent}"
    rounded: "{rounded.control}"
    padding: "9px 12px"
  approval:
    backgroundColor: "{colors.warn-bg}"
    textColor: "{colors.ink}"
    rounded: "{rounded.approval}"
    padding: "18px 20px"
---

# Design System: h4rn / Direction B

## Overview

**Creative North Star: “Editorial workbench”**

Mode: Operate. An editorial sensibility expressed through hierarchy, measure and alignment. Warm mineral light surfaces and green-black dark surfaces support long desk sessions and brief phone checks. The interface is a working application, without newspaper styling, serif flourishes, ruled paper, decorative numbering or invented jargon.

This is an isolated redesign proposal. It supersedes the incumbent look only inside `design/b-editorial/`; it does not update the application's root system. The user explicitly selected Direction B and asked for a self-contained code prototype. Demonstration data and model names are illustrative, not claims of live host connections or real usage. Google Fonts are the only external dependencies; system fallbacks remain usable offline.

**Key characteristics and reasoning — ten decisions:**

- A wordmark replaces the orange logo tile so the task owns the strongest visual emphasis.
- A low-chroma warm ground softens bright environments without adding simulated paper texture.
- Deep teal is the single working accent; red and ochre communicate exceptions only.
- Manrope gives headings a compact, clear silhouette; Source Sans 3 carries dense operational text.
- A shared left alignment links titles, tool names, file summaries and reply controls, making scan paths predictable.
- Provenance is a plain sentence beneath the task, not a collection of header badges.
- Inbox rows use title, location and state; removing decorative counters strengthens meaningful ordering.
- Identity marks appear at host, harness and child-agent boundaries; live orbs appear only beside active work.
- Approvals expose the command, machine, directory and permission scope before the action.
- Phone layouts change the navigation and decision flow: one pane, bottom navigation, sheets and large targets.

## Colors

The YAML above is normative. Names correspond directly to CSS custom properties; the `dark-` prefix becomes the same property inside `[data-theme=dark]`.

| Role | Rule |
| --- | --- |
| paper | Main work surface. |
| surface | Collection, header, inputs, dialogs and composer. |
| panel | Shell, user context, terminal and secondary zones. |
| ink / muted | Primary and secondary text. Muted is still readable body text. |
| line | Structural separators only; never a control's only boundary. |
| edge | Essential input borders and inactive switch tracks. |
| accent / on | Primary actions and their text, focus, active navigation, live work. |
| tint | Selected rows and calm operational context. |
| warn / warn-bg | Permission or recovery decisions, always named in words. |
| danger / danger-bg | Errors, conflicts, removals and revoke actions. |

**The Decision Rule.** Accent signals something actionable, selected or live. Identity color is subdued. Do not use teal as a decorative field.

AA measurements use Chromium's sRGB rendering of the actual OKLCH tokens, sampled through canvas and evaluated with WCAG relative luminance. Ratios rounded to two decimals; text must be ≥4.5:1 and essential non-text ≥3:1.

| Foreground / background | Light | Dark | Use |
| --- | ---: | ---: | --- |
| ink / paper | 14.54 | 14.33 | Main text |
| ink / surface | 15.43 | 13.29 | Collection and forms |
| muted / paper | 6.51 | 7.40 | Secondary text |
| muted / panel | 5.96 | 7.87 | Shell text |
| muted / tint | 5.46 | 5.43 | Selected-row metadata |
| accent / paper | 8.55 | 9.47 | Actions |
| accent / tint | 7.18 | 6.95 | Selected navigation |
| on / accent | 9.08 | 9.61 | Primary-button text |
| warn / warn-bg | 6.95 | 7.85 | Warning text |
| danger / danger-bg | 7.23 | 6.89 | Error/conflict text |
| edge / surface | 4.31 | 4.37 | Control boundary, non-text |
| accent / panel | 7.83 | 10.07 | Terminal status |

`line` is decorative separation and intentionally lighter. It must not be used as text, a focus indicator or an essential form outline. QR placeholders use dark ink on white for conventional optical contrast; they are explicitly illustrative, not scannable.

## Typography

Use the fixed scale in frontmatter. Headings use Manrope; body, labels, buttons and measurements use Source Sans 3. Source Code Pro is confined to commands, terminal output and source code. Numeric comparisons use tabular numerals, not a change of typeface.

Phone adjustments are structural and deliberate: collection title 26px, page title 25px, thread title 18px, thread-row title 17px, bottom-nav labels 11px. Desktop measurement values use 25px, phone 24px. Terminal and editor use 13px on desktop; phone terminal uses 11px. The wordmark is 26px / 800 with −0.04em tracking as a dedicated identity treatment; general headings stop at −0.035em. Prose stays at approximately 65–75 characters; dense tool metadata may be shorter.

**The Code Rule.** Monospace has semantic work to do. Never use it for navigation, section labels, counters or ornamental technical flavor.

## Layout

Desktop at 1440×900: 44px prototype toolbar, 184px application navigation, 318px collection, remaining width for detail. Detail header and composer stay fixed; only the transcript scrolls. Explicit grid row constraints and zero pane minimum heights preserve this behavior. Settings pages use a maximum 1120px content width with 40px side insets. The stream has a 790px outer measure and 30px side padding.

The default spacing rhythm is 4px. Related elements use 8–12px; groups use 16–24px; separate tasks use 28–40px; distinct columns use 40–48px. Optical alignment in headers uses 22px collection padding and 30px detail padding. Dense file rows use 7px vertical padding within a 16px padded group.

Below 768px, one pane fills the device. Inbox and thread are separate destinations. The 390×844 frame has a 30px device-status strip, a 66px bottom navigation zone and 20px page gutters. A thread has a back control and horizontally scrollable context tabs. A reply stays above navigation. Approval becomes a focused decision view; it omits already-reviewed tools and the file summary. The command palette and secondary actions become bottom sheets. Management tables scroll horizontally rather than collapsing provenance or inventing harness support. Usage stacks sections and wraps measurements into two columns. PR review/checks stack. The desktop Phone control previews the same mobile rules in a centered 390×844 frame.

**The Provenance Rule.** At a decision, show host and harness. A model without its execution location is incomplete information.

## Elevation & Depth

Structural surfaces use tonal separation and a single 1px rule. No card shadows. Dialogs and transient feedback use one offset, soft shadow: `0 16px 50px #172c2630` in light and `0 16px 50px #0006` in dark. Overlay scrim: `#10262066`. Never combine a dialog border with its shadow. No glass, background blur, gradients or decorative material simulation.

## Shapes

Controls have 6px radii, compact state labels 4px, grouped file changes and composer 8px, approvals 10px, dialogs 12px. Round forms are limited to switches, tiny status dots and intentional blobatar/orb placeholders. The external phone-preview frame uses 24px corners and is prototype chrome.

Blobatars are 18px identity placeholders using asymmetric rounded contours, with a small contrasting cutout. They identify provenance, not unrelated content. Orbs are 12px state placeholders used only alongside a named active operation. Implementation should replace these placeholders with `blobatar` and `thinking-orbs` while preserving scale and placement.

## Components

- **Buttons:** primary teal, secondary outlined with `edge`, ghost text/icon. Desktop minimum 34px; phone targets at least 44px. Hover uses panel tint; primary hover slightly darkens. Active shifts 1px. Disabled is 50% opacity with a native disabled attribute and an explanation. Destructive actions name what will be revoked or removed and require a focused confirmation.
- **Inputs:** surface fill, 1px edge outline, 6px radius, real labels. Placeholders use muted text. Caret uses accent. Selected text uses accent/on. Native select controls remain recognizable.
- **Focus:** 2px accent outline, 3px offset. No color-only state. Dialogs trap focus; Escape closes; command palette opens with Cmd/Ctrl K, filters commands and supports Enter.
- **Navigation:** text plus consistently stroked 18px SVG icons. Active tint and weight provide redundant state. Bottom navigation labels stay visible. No decorative activity rail.
- **Inbox tabs:** Working, Done, Settled. Working includes active, queued, approval and scheduled-resume work. Done requires acknowledgement; Settled is acknowledged history. Selected tab has an underline and weight, not a filled pill.
- **Thread rows:** title first, host/harness next, state/time last. No decorative numbering. Selection uses a full tint; attention is expressed in text, with a compact semantic label when necessary.
- **Tools and file changes:** tools are expandable native details with name, target and timing. File groups show readable paths and signed additions/deletions. Review opens the IDE comparison.
- **Composer:** anchored below the independently scrolling transcript. Usage sits immediately above it, showing tokens, cost, speed and elapsed time. Phone hides optional model context, retaining reply and send.
- **Approval:** show reason, exact command, execution directory, host and grant scope. “Allow once” sends a single approval; “Hold command” pauses without executing. Never imply a blanket permission grant.
- **Subagents:** indentation communicates ancestry. Each child names its harness, model and host; progress, tokens and elapsed time belong to the child. A Codex child may have a Claude parent.
- **Library:** machine availability and per-harness delivery are separate controls. Shared servers state where execution occurs. Conflicts require choosing one definition. Secret labels identify host-local references. Unsupported targets are text, not false toggles.
- **Switches:** native ARIA switch semantics and explicit on/off state. Desktop track 32×20px; phone uses a 44×44px transparent hit area around the same track.
- **Usage:** one aligned measurement strip, chart with labels and legend, model table and recovery context. Reset countdown, resume, cancellation and failover remain together. Synthetic costs are marked estimates.
- **States:** loading uses static skeletons and `aria-busy`; empty teaches the next action; errors name the failing service and offer recovery; offline retains cached work and removes send. Toasts use a polite live region.
- **Motion:** controls transition background and color for 180ms with `cubic-bezier(.16,1,.3,1)`, within the 150–250ms interaction range. The live orb's slower 2s loop communicates active work rather than transition choreography. Demo streaming reveals text at 65ms per character. Reduced motion stops all transitions/orb motion and shows the complete stream immediately; countdown text still updates because it is information.

## Do's and Don'ts

Do preserve the familiar workbench structure, align operational text, keep location visible, and let the work carry the visual weight. Do test both themes and the actual phone viewport. Do retain manual recovery from automatic actions.

Don't revive stage terminology, cue counters, amber gutters, numbered rows, logo blocks, pill-filled headers or decorative blob fields. Don't replace missing content with ornament. Don't use serif display typography or paper simulation to signal “editorial.” Don't pretend a prototype executes real host commands, pairs real devices or reports real usage.
