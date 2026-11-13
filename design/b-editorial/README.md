# Direction B · Editorial workbench

Open `index.html` directly in a browser. It contains all application CSS and JavaScript. Only Google Fonts loads externally. Screen, theme and viewport controls sit in the prototype toolbar; Cmd/Ctrl K opens the command palette. The Phone control previews a 390×844 app frame; an actual 390×844 browser viewport uses the same mobile composition.

All connections, costs, model names, host state, terminal output and QR codes are illustrative. Actions update local prototype state; no real host command, permission grant, PR comment or pairing request is sent.

- `DESIGN.md`: full OKLCH system, component rules, AA contrast measurements and ten design decisions.
- `.impeccable/design.json`: machine-readable extensions and component examples.
- `REVIEW.md`: direction contract, critique and fix verification.
- `shots/`: 96 screen captures: 22 routes plus Done and Settled, at 1440×900 and 390×844, in light and dark. Twelve contact sheets support quick review. JSON reports record viewport/error and contrast/interaction checks.

Reproduce captures using `node design/b-editorial/capture.cjs` from the repository root. `verify.cjs` exercises approval, switches, comparison, command palette navigation, auto-resume, terminal input, Done, phone frame and the More sheet. `review.cjs` creates contact sheets. Scripts use the repository's Playwright Core installation and the installed Chromium 1248 executable. On this machine Chromium needs additional shared libraries; the captured runs used `LD_LIBRARY_PATH=/tmp/h4rn-browser-libs/unpack/usr/lib/x86_64-linux-gnu`. These libraries are verification infrastructure, not prototype dependencies.

No application source or root product/design files are part of this direction.
