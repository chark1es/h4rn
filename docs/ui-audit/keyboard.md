# Keyboard-only check

The verification pass for the UI refinement. Every shared surface was driven with the keyboard only: Tab to the trigger, open it, move with arrows or typeahead, act with Enter or Space, close with Escape, and check that focus lands back where it started with a visible ring. One pass per surface. The pointer is used in one place only, to time the tooltip's hover delay.

Run it against a dev server in mock mode:

```sh
cd apps/ui
bun run dev -- --port <port>
H4RN_KB_OUT=<dir> node scripts/keyboard-audit.mjs http://127.0.0.1:<port>
# One surface while iterating:
H4RN_KB="file tree" node scripts/keyboard-audit.mjs http://127.0.0.1:<port>
```

It prints the table below and exits 1 if any surface fails. Final run: **40 of 40 surfaces pass**. The two that failed on the first run (toaster, inline revoke confirm) were fixed afterwards; see "Fixed after the first run".

## Results

| Surface | Steps | Result | Notes |
|---|---|---|---|
| New thread dialog | Tab to New thread, Enter, Tab ×20, Esc | Pass | starts on textarea "" |
| Shortcuts dialog | Tab to Right panel, Ctrl+/, Tab ×20, Esc | Pass | starts on dialog "Keyboard shortcuts" |
| Rename dialog | Thread actions, R…, Enter, Esc | Pass | starts on input "Thread title" |
| Git actions dialog | Thread actions, C…, Enter, Esc | Pass | starts on textarea "Commit message" |
| Guest share dialog | Thread actions, S…, Enter, Esc | Pass | starts on dialog "Share with guest" |
| Delegate dialog | Right panel, Subagents tab, Tab to Delegate, Enter, Esc | Pass | starts on textarea "" |
| Add project dialog | Tab to Add project, Enter, Tab ×20, Esc | Pass | starts on input "Folder path" |
| Add MCP server dialog | MCP page, Tab to Add MCP server, Enter, Esc | Pass | starts on input "" |
| Edit MCP server dialog | MCP page, Tab to Edit, Enter, Esc | Pass | starts on input "" |
| Skill import dialog | Skills page, Tab to Find existing, Enter, Esc | Pass | starts on input "" |
| Skill add dialog | Skills page, Tab to Add skill, Enter, Esc | Pass | starts on input "" |
| Confirm dialog (remove forge account) | Forges, Tab to Remove, Enter, Tab ×20, Esc | Pass | starts on button "Cancel" |
| Confirm dialog (revoke device) | Hosts, Tab to Revoke, Enter, Tab ×20, Esc; again, Tab to Revoke, Enter | Pass | starts on button "Cancel"; after revoking, focus on button "Refresh" (was Fail: inline confirm dropped focus to the body) |
| Thread actions menu | Tab to Thread actions, Enter, ArrowDown, typeahead S, Esc | Pass | typeahead "s" → Settle now |
| Row actions menu (Shift+F10) | Tab to a thread row, Shift+F10, ArrowDown, Esc | Pass |  |
| Row actions menu (button) | Tab to the row's Actions button, Enter, ArrowDown, Esc | Pass |  |
| Hosts menu | Tab to the hosts button, Enter, ArrowDown, Esc | Pass |  |
| Project sort menu | Tab to Sort projects, Enter, ArrowDown, Esc | Pass |  |
| Project actions menu | Tab to <project> actions, Enter, ArrowDown, Esc | Pass |  |
| Turn picker | Right panel, Tab to Turn, Enter, ArrowDown, Esc | Pass | trigger is button; 1 enabled item(s) |
| Composer model chip | Tab to the model chip, Enter, Tab to Model, ArrowDown, Esc ×2 | Pass | chip "claude-sonnet-5-5" |
| Select (settings) | Tab to Longest wait, Enter, ArrowDown, Enter, check value | Pass | trigger "Longest wait: 6 hours" |
| SuggestInput (New thread model) | New thread, Tab to Model, ArrowDown ×2, Esc, Esc | Pass |  |
| Command palette | Tab to Search or run, Enter, type, ArrowDown, End, Home, Esc | Pass | End → Show Settled, Home → Settle thread; one Esc closed with the query typed |
| Command palette (Ctrl+K) | Ctrl+K from the composer, Esc | Pass |  |
| Segmented control (inbox state) | Tab to Working, ArrowRight, check Done | Pass | Working → Done (aria-checked=true) |
| Tabs (Thread / IDE) | Tab to Thread tab, ArrowRight, Enter or auto-select | Pass |  |
| Tabs (right panel) | Right panel, Tab to Changes, ArrowRight ×3, Home | Pass | Changes → Subagents → Home: Changes |
| Switch | Appearance, Tab to Terminal switch, Space ×2 | Pass | true → false → true |
| Segmented control (color scheme) | Appearance, Tab to the scheme radios, ArrowRight | Pass | Light → Dark |
| File tree | Right panel, Files tab, Tab into tree, ArrowDown, ArrowRight, ArrowLeft | Pass | api → src; folder expanded false → true |
| Resizer (sidebar) | Tab to Resize sidebar, ArrowRight ×3, check width | Pass | width 288 → 336, valuenow 20 → 23 (percent), `aria-valuetext` "336 px", focus line in the ring colour |
| Settings form (Forges) | Tab through Forge select, Base URL, Token, Add account | Pass | Forge: GitHub → Forge base URL → Forge token |
| Settings nav | Tab into the settings nav, Enter on Appearance | Pass |  |
| Toaster | Tab past the last control into the toast, then Tab again | Pass | after Dismiss, Tab went to button "Back to threads" (focus-visible true), the first control on the page (was Fail: Tab looped back into the toast) |
| Tooltip delay | Hover Right panel, time until visible; move to Toggle terminal | Pass | first 318 ms, next 25 ms (pointer used only to time hover) |
| Tooltip on keyboard focus | Tab to Right panel, wait | Pass |  |
| No tooltip on touch | hasTouch context, tap Right panel twice | Pass |  |
| Focus ring screenshot (light) | Tab to Working segment, Stop button, Right panel; screenshot each | Pass | segment ring yes; stop ring yes; icon-button ring yes |
| Focus ring screenshot (dark) | Tab to Working segment, Stop button, Right panel; screenshot each | Pass | segment ring yes; stop ring yes; icon-button ring yes |

Notes on the table:

- "starts on dialog" (Shortcuts, Guest share): those dialogs have no field, so UI SDK focuses the dialog itself. Tab moves into it and stays trapped.
- The shortcuts sheet opens from anywhere except a text field: `shortcuts.open` is bound with `inInputs: false`, so Ctrl+/ in the composer types a slash. That's by design (see the sheet's own copy).
- Command palette: the keys match the cmdk palette on main. One Escape closes even with a query typed (UI SDK Command would clear the query first). Home and End move the highlight to the first and last result, arrows stop at the ends, and Ctrl+N/J/P/K, ⌘+arrow and Alt+arrow work as in cmdk. Focus returns to the trigger.
- Turn picker: the mock thread has one turn, so the menu has a single enabled item. It opens with Enter and closes with Escape, and focus returns to the trigger.
- Tooltips open after about 320 ms, move between triggers in about 20 ms, also open on keyboard focus, and never open on tap in a `hasTouch` context.

## Focus rings

Each control was focused with Tab and captured in light and dark.

| Control | Light | Dark |
|---|---|---|
| Segmented control (Working) | ![](keyboard/focus-segment-light.png) | ![](keyboard/focus-segment-dark.png) |
| Secondary button (Stop) | ![](keyboard/focus-stop-light.png) | ![](keyboard/focus-stop-dark.png) |
| Icon button (Right panel) | ![](keyboard/focus-icon-button-light.png) | ![](keyboard/focus-icon-button-dark.png) |

## Fixed during this pass

| Commit | Problem | Fix |
|---|---|---|
| `f7e6d140` | Segmented controls, tabs, switches, checkboxes, radios, collapsibles, dialog close and `Button` showed **no focus ring**. Tailwind v4's `outline-none` sets `--tw-outline-style: none`, and `focus-visible:outline-2` only sets the width. | Added `focus-visible:outline-solid` in the 14 files, plus `src/components/ui/focus-ring.test.ts`, which fails if the pair comes back without it. |
| `a7e1a683` | ArrowRight on the Thread/IDE tabs switched the view and **dropped focus to the body**, because each view mounts its own switch. | The switch that mounts next focuses its selected tab when the change came from the tab list. Test in `ThreadHeader.dom.test.tsx`. |
| `16bae384` | The settings nav group headers still shrank 3–6 px when the MCP list opened (A6). | `.setnav .q-side-h` is `flex: none`. |
| `d7f2e628` | With the right panel open, the header's provenance line squeezed the harness name to zero width. | The branch shrinks first, then the harness name ellipsizes, with the full name in its title. |

## Fixed after the first run

Both failures from the first run are fixed, and the audit passes on all 40 surfaces.

| Commit | Problem | Fix |
|---|---|---|
| `7b4f5f38` | **The toaster trapped Tab.** Sonner's list `onBlur` refocuses the element focused before the toast whenever focus leaves the stack, so Tab after Dismiss went back there and the next Tab went into the toast again. | `components/ui/sonner.tsx` watches Tab at the edges of the stack. When the stack was reached by Tab or a click, Tab past the last toast control moves to the next control in document order (wrapping to the first on the page) and Shift+Tab before the first moves to the previous one. Sonner's one refocus call is skipped for that move. Entry by Alt+T still returns focus to where it came from. `aria-live`, hover expand, de-duplication and positions are untouched. `sonner.test.tsx` "lets Tab leave the stack in document order" fails without the fix. |
| `44c6b8a8` | **Inline confirms dropped focus, with no Alert Dialog.** Revoke in `NetworkAccess.tsx`, Revoke / Sign out on the device row in `SettingsRemote.tsx` and Sign out in `ConnectSignIn.tsx` replaced the button with an inline Cancel / confirm pair, and focus fell to `<body>`. | All three open the shared `ConfirmDialog` (UI SDK Alert Dialog), as Forges' Remove does: focus starts on Cancel, is trapped, and Esc or Cancel returns it to the trigger. If the confirmed action removes the row, focus moves to the next control after it, or the one before (`rememberNeighbours` in `lib/focus.ts`). The titles reuse the existing questions (`devices.confirmRevoke` now names the device) and a new `devices.confirmDescription.*` line says what happens, in all four catalogues. Test in `DirectDevicesTable.dom.test.tsx`; `e2e-remote.mjs` confirms Sign out in the dialog. |
