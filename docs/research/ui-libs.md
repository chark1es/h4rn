# UI libraries for h4rn — Thinking Orbs and blobatar

Research for the h4rn control surface (Tauri v2 + React). Two libraries are evaluated as
candidates for agent-state indication and for deterministic agent/harness identities.

**Method.** Every package was downloaded from npm into `/tmp/h4rn-research/pkgs`
(`npm pack`, then extracted and read — nothing was executed). Sizes are measured with
`stat` and `gzip -c` (gzip default level, i.e. `-6`). Behaviour claims come from the shipped
`dist/*.js`, the shipped `.d.ts` files and each project's README, not from the marketing site.

Legend used below:

| Mark | Meaning |
|---|---|
| **measured** | Read out of the tarball, or measured with `stat`/`gzip` in this session |
| **verified** | Observed in the shipped implementation (source of truth is the tarball) |
| **UNVERIFIED** | Stated by a website or README but not confirmable from the package itself |

Both packages exist and match their descriptions. No substitution or renaming was needed.

---

## 1. Package inventory

| | thinking-orbs | blobatar | @blobatar/react |
|---|---|---|---|
| npm name | `thinking-orbs` | `blobatar` | `@blobatar/react` |
| Latest version | **0.3.2** (2026-09-22) | **2.7.0** (2026-08-29) | **2.7.0** (2026-08-29) |
| License | MIT | MIT | MIT |
| Runtime dependencies | **none** | **none** | none (re-export only) |
| Peer deps | `react >=18.0.0` | `react >=18`, `vue >=3` — *both optional* | `blobatar 2.x`, `react >=18` |
| Module format | ESM + CJS, `type: module`, `sideEffects: false` | ESM only, `sideEffects: ["*.css"]` | ESM only |
| Tarball | 38,860 B | 178,616 B | 12,589 B |
| Unpacked | 130,686 B | — | 43,500 B |
| Main bundle (gzip) | **14.5 KB**, two ESM chunks (`index.es.js` 24,977 B + `index-B8WsUNf5.js` 26,541 B = 51,518 B raw) | **5.9 KB** (`dist/index.js`: 13,116 B raw) | 153 B raw (pure re-export) |
| Slim entry (gzip) | `thinking-orbs/engine` — framework-free geometry | **5.1 KB** (`blobatar/blob`) | — |
| Rendering | **2D canvas** (path/arc drawing) | **SVG markup** (string) | `<img>` or inline SVG |
| Repo | `Jakubantalik/Libraries.dev`, `packages/thinking-orbs` | `Alain00/blobatar`, `packages/blobatar` | same, `packages/react` |

Two packaging facts that matter:

- `thinking-orbs` marks `sideEffects: false`, so an unused orb tree-shakes away cleanly.
- `blobatar` deliberately does **not** mark `sideEffects: false` — the comment in its
  `package.json` explains why: doing so would let a bundler drop `import "blobatar/motion.css"`
  and animated blobatars would silently render static. Keep that in mind if h4rn ever adds a
  blanket `sideEffects: false` assumption in bundler config.
- `@blobatar/react` is a 153-byte module: `import { Blobatar } from "blobatar/react"; export { ... }`.
  The component itself still lives in `blobatar/react`, which is **deprecated and frozen** and is
  removed in `blobatar` v3. Use `@blobatar/react` for the import path, but expect the peer on
  core `blobatar` to remain.

---

## 2. `thinking-orbs` 0.3.2

### 2.1 Rendering and performance model

Plain 2D canvas, path-and-arc dotted geometry. **No WebGL, no SVG filters.** One nuance worth
stating because the README overstates it: the *orb painter* (the second chunk,
`dist/index-B8WsUNf5.js`) uses `beginPath`/`arc`/`moveTo`/`lineTo` and no `ctx.filter`, but the
**optional `gravity` layer** in `dist/index.es.js` does use `ctx.filter = "blur(…)"`,
`getImageData`/`putImageData` with `willReadFrequently: true`, and a multi-pass sprite warp
(`bendSprite`, `PASSES`, `destination-in`/`destination-over`). None of that runs unless
`gravity` is enabled — but it is the one part of the package that is genuinely expensive and
the one part that degrades where `ctx.filter` is unsupported (WebKit). See §2.2.

**verified** behaviour in `dist/index.es.js`:

| Mechanism | Implementation | Consequence for h4rn |
|---|---|---|
| Clock | One `requestAnimationFrame` per mounted instance, all reading the same `performance.now()` timebase | Instances stay in phase with each other, but N visible orbs *is* N rAF callbacks — unlike a single-ticker design |
| DPR | `devicePixelRatio`, **capped at 2** | 4K/retina phones do not render at 3× cost |
| Offscreen | `IntersectionObserver` pauses each instance and resumes it in phase | Long chat transcripts stay cheap |
| Tab hidden | `visibilitychange` + `document.visibilityState` pause | Background threads cost nothing |
| Theme | `MutationObserver` on ancestor `class` / `data-theme`, plus `matchMedia('(prefers-color-scheme: dark)')` subscription | Live theme switch without remount |
| Reduced motion | `matchMedia('(prefers-reduced-motion: reduce)')`, live-updating via `addEventListener("change")` | Animates → single static representative frame |

Perf implication on mobile WebViews: a 2D canvas painting a few dozen paths at ≤2× DPR is
about as cheap as an animated indicator gets. Each *visible* orb is one canvas, one rAF
callback and one composited layer (24–64 CSS px); offscreen and hidden-tab instances are
stopped entirely, so a long transcript only costs for what is on screen. This is the right
trade for a Tauri app, where WebKitGTK/WKWebView/WebView2 all have fast 2D canvas paths and
WebGL is the thing that historically regresses on Linux WebKitGTK.

### 2.2 API

Component export: `ThinkingOrb`. Everything else is exported for advanced use.

| Prop | Type | Default | Notes |
|---|---|---|---|
| `state` | `OrbState` (9 values) | `working` | See §2.3 |
| `size` | `64 \| 32 \| 20` | `64` | 64 and 20 are separately hand-tuned; **32 is interpolated, not tuned** |
| `theme` | `auto \| dark \| light` | `auto` | `auto` resolves ancestor theme → OS → SSR-safe |
| `speed` | `number` | `1` | Multiplier on the preset's baked speed |
| `paused` | `boolean` | `false` | Freezes on the current frame |
| `color` | CSS color string | — | Tint; the depth-shading ramp is preserved |
| `dots` | `number` | `1` | Density multiplier (floor 0.1) |
| `dotSize` | `number` | `1` | Radius multiplier |
| `opts` | `ModeOpts` | — | Raw per-mode knobs merged last (`ghostA`, `particles`, `scanMul`, `signals`, `turns`, `shape`, …) |
| `frame` | `ModeFrame` | — | Escape hatch: replace the geometry function entirely |
| `gravity` | `boolean \| GravityOptions` | off | Pointer-pull effect; needs a cursor raster sprite |
| `aria-label` | string | per-state default | Overrides the built-in label |
| rest | `CanvasHTMLAttributes` | — | `className`, `style`, `data-*` pass through |

Also exported: `resolvePreset`, `STATE_TO_MODE`, `MODE_DRAWS`, `MODE_FRAMES`, `countDots`,
`scaleCounts`, `scaleRadii`, and geometry helpers (`finalizeFrame`, `makeProj`, `radiusScale`,
`fibDir`, `hashD`, `vnoise`, `lerp`, `frac`, `angleDelta`), plus the gravity helpers.

`thinking-orbs/engine` is the same geometry with **no React and no canvas binding**:
`MODE_FRAMES` (pure geometry), `paintFrame`/`paint`/`paintLines` (2D canvas binding). The
`.d.ts` states the React Native port imports exactly these functions, so native output is
identical by construction. This is the subpath to use if h4rn ever wants orb geometry
outside React (e.g. a Rust-side preview or a non-React surface).

`gravity` is the one prop to leave off by default. It needs the platform's real pointer raster
(`gravity.sprite` / `setGravitySprite`) and self-disables under reduced motion, forced colours,
coarse pointers, page zoom, and any cursor other than the plain arrow. In a WebView that is
three platform-specific rasters to ship and maintain for a novelty effect — and it is also the
only code path in the package that pays for `getImageData`, a per-frame sprite warp, and
`ctx.filter` blur, so it is the one place where a low-end device can actually feel the orb.

### 2.3 The nine states

Each state maps 1:1 to a mode key (`STATE_TO_MODE`) and is a distinct animation:

| `state` | Mode key | Visual |
|---|---|---|
| `working` | `orbits` | Particles on tilted orbits |
| `searching` | `globe` | A scan meridian sweeps a dotted globe |
| `solving` | `rubik` | Bands scramble in quarter turns, then click back solved |
| `listening` | `wave` | A waveform rolls through latitude rings |
| `connecting` | `web` | A constellation wires itself; packets run the edges |
| `weaving` | `braid` | Three strands plait around the sphere |
| `composing` | `ribbon` | An undulating multi-band sash |
| `breathing` | `ring` | A face-on ring slowly morphing |
| `shaping` | `morph` | Dotted outline morphs circle → triangle → square |

There is **no** `idle`, `error`, `done`, or `waiting` state. Those must be composed from
`paused`, `speed`, `color`, and `opts` (see §4). Notably `shaping` accepts
`opts: { shape: 0 | 1 | 2 }` to *hold* circle/triangle/square instead of cycling — that is the
hook for a terminal/static glyph.

### 2.4 Theme resolution (`theme="auto"`)

Three layers, live-updating (verified in `theme.d.ts` + bundle):

1. Nearest ancestor with `data-theme="dark|light"` or a `dark`/`light` class (Tailwind/shadcn
   convention), observed with `MutationObserver`.
2. Otherwise `prefers-color-scheme` via `matchMedia`, subscribed for live switch.
3. SSR: the canvas paints only on the client, after the theme resolves.

Practical note for h4rn: if the app sets `class="dark"` on `<html>`, orbs follow the app theme
with no wiring. If it uses a custom token system, pass `theme` explicitly or add `data-theme`.

### 2.5 Accessibility and reduced motion

- `role="img"` with a per-state `aria-label` by default; override per usage.
- `prefers-reduced-motion: reduce` → a **static representative frame**, not a blank box, and
  the theme still follows live changes. A reduced-motion user sees a stable glyph per state,
  which is exactly the fallback h4rn would otherwise have to build.
- Ambient pause on scroll-off/hidden-tab is not user-visible but keeps a long thread honest.

### 2.6 SSR and WebView compatibility

- SSR-safe: renders the `<canvas>` element on the server, paints only after mount/theme
  resolution, so no hydration mismatch and no `window` access during render. `matchMedia`
  is guarded with `typeof ... === "undefined"` checks.
- WebView compatibility is broad because the surface is limited: 2D canvas, `matchMedia`,
  `IntersectionObserver`, `MutationObserver`, `devicePixelRatio`. All exist in WebKitGTK
  (Tauri v2 Linux), WKWebView (macOS) and WebView2 (Windows). No `ctx.filter`, which is the
  usual WebKitGTK gap.
- The one WebView caveat is `gravity`: pointer events plus a raster cursor sprite. It is
  explicitly disabled for coarse pointers and forced-colours mode, but on Linux it still
  depends on shipping a cursor image. Recommend `gravity={false}`.

### 2.7 Ports and the marketing site

The libraries.dev page advertises **React, SwiftUI, React Native** tabs. The npm package is
**React only** (`peerDependencies: react >=18`); there is no first-party React Native or
SwiftUI package on npm. The site's other tabs are Studio/codegen output (the package's own
`package.json` has a `codegen:swift` script and the engine subpath exists so a native port can
reuse the geometry) — **UNVERIFIED** as published artifacts.

Third-party ports exist on npm (`rn-thinking-orbs`, `solid-thinking-orbs`, `thinking-orbs-vue`,
`expo-thinking-orbs`, `morphing-orbs`) but are not by the same author and are out of scope.
For h4rn, take the React package and the engine subpath only.

---

## 3. `blobatar` 2.7.0

### 3.1 Rendering and size

Deterministic **SVG markup from a string**, no dependencies, no DOM, no network.

| Entry | What it is | Measured gzip |
|---|---|---|
| `blobatar` | Generator + palette/trait utilities | 5.9 KB |
| `blobatar/blob` | Generator only, ~1 KB less | 5.1 KB |
| `blobatar/react` | React component (deprecated path) | 6.3 KB |
| `@blobatar/react` | 153-byte re-export of the above | negligible |
| `blobatar/uri` | `data:` URI helper | small, tree-shakes |
| `blobatar/expression` | 14 poses, opt-in per import | ~0 unless imported |
| `blobatar/idle` | Idle motion as arithmetic (for no-CSS renderers) | opt-in |
| `blobatar/gaze` | Pointer-tracking eyes, opt-in | opt-in |

The README's "~4.4 KB gzipped" claim is for the blob entry with the project's own `size`
script (likely brotli or a tighter minifier). Measured with plain gzip on the shipped
`dist/blob.js` it is **5.1 KB**. Budget 6 KB for the full entry.

Structural properties that matter at scale:

- **No `<defs>`, gradients, filters, or element ids** (README; the renderer emits plain paths).
  So N blobatars on a page cannot collide on ids — a real problem with identicon libraries
  that inject `<defs>`.
- Static render is a **single `<img>`** (data URI). That is the right default for a sidebar or
  a list of hundreds: one element, no extra DOM nodes, no per-node CSS.
- Animated render is **inline SVG, ~a dozen nodes per avatar**, because `:hover` cannot reach
  inside an `<img>` and host CSS cannot style it. This is an explicit, documented trade.

### 3.2 API

**String API (SSR-friendly):**

- `blobatar(name, opts?) => string` — full `<svg>…</svg>` markup.
- `blobatarUri(name, opts?) => string` — `data:image/svg+xml,…`, percent-encoded (not base64;
  the source comments explain base64 would inflate ~33%).
- `_parts`, `_marks`, `_posed`, `_layout` — underscored, non-public, for renderers with no
  markup string (used by React Native). Do not build on these.

**`BlobatarOptions`** (shared by string API and adapters):

| Option | Default | Notes |
|---|---|---|
| `size` | — | Emits `width`/`height`; omit to let CSS size it |
| `background` | none | `"squircle" \| "circle" \| "square" \| false` |
| `hue` | — | Locks hue in degrees; the name then drives shape only |
| `tone` | — | Locks swatch as 0–1, pale → ink. `1` collapses to `0`; use `0.999` for ink |
| `traits` | — | Pins individual trait positions (0–1) or narrows to a list of allowed values |
| `palette` | — | Per-key hex overrides; **bypasses the 4.5:1 contrast guarantee** |
| `normalize` | `true` | NFC + trim + lowercase |
| `contrast` | `true` | Enforce contrast floors |
| `title` | — | Adds `<title>` for screen readers |
| `animate` | — | `"hover" \| "always"` — adapters only; the string API stays static |
| `expression` | `idle` | A pose **value** imported from `blobatar/expression` |

**React component** (`@blobatar/react`):

```
<Blobatar name={…} size={…} background="circle" animate="hover" expression={thinking} />
```

- `name` is the only required prop. It is named for what the value *is* (a person/harness id),
  not for the hashing step — internally it is still the seed.
- Props switch by mode: `animate` absent/false → `ImgHTMLAttributes`; `animate` set →
  `SVGProps<SVGSVGElement>`. This is deliberate: `onLoad` stops type-checking when animation
  turns on, because it stops firing.
- `animate="hover"` is the default recommendation for grids; `"always"` is for the single
  large avatar (profile header / onboarding).
- Animated usage requires `import "blobatar/motion.css"`. Gaze additionally requires
  `blobatar/gaze.css` and is opt-in via `useGaze` from `@blobatar/react/gaze`.

**Expressions** — 14 poses, each passed as an imported value so unimported poses cost zero:
`idle`, `happy`, `sad`, `mad`, `surprised`, `wink`, `sleepy`, `smug`, `unsure`, `scared`,
`love`, `shy`, `sick`, **`thinking`**.

`thinking` is worth calling out for h4rn: it is explicitly designed as a *duration* pose, not a
still frame — mid-lidded eyes at two heights trading places (a `rock` channel), i.e. the
two-dot loader idiom on a face. A pose set by the caller is held until changed; there are no
timers. That makes it a natural mapping for an agent's `thinking` state if the design wants the
avatar to carry the state rather than a separate orb.

### 3.3 Determinism and hashing (verified from `src/hash.ts`)

This is the part h4rn should rely on, so the mechanism is worth stating precisely:

1. **Normalize**: `seed.normalize("NFC").trim().toLowerCase()`. So `Alain@Example.com` and
   `alain@example.com` agree, and precomposed/decomposed `café` agree.
2. **Seed once**: hash UTF-8 bytes (not UTF-16 units, so astral-plane names behave the same
   everywhere) with an FNV-1a-style `feed()` seeded from `1779033703 ^ length`.
3. **Per-trait stream**: each trait key continues from that state via
   `finalize(feed(feed(state, SEP), utf8(key))) / 2^32`, where `finalize` is murmur3 `fmix32`.
   This gives two guarantees the file documents:
   - **avalanche** — `alain` vs `alaim` are visually unrelated (plain FNV-1a is not enough);
   - **streaming / key-addressed traits** — adding a trait in a later minor cannot disturb
     existing blobatars, because values are addressed by key rather than drawn sequentially.
4. **Frozen per major**: numeric ranges, shape thresholds, tone set, and expression roster are
   part of the contract, enforced by a golden fixture (`1312` renders + a 20k-seed shape
   histogram). Adding a *shape* or *tone* moves thresholds and is therefore a **major** —
   i.e. the package major is the generation selector.

Consequences for h4rn:

- The same string always renders the same face **within a major version**. Pin
  `blobatar@2.7.0` exactly (or at minimum `~2.7`) and add a golden test for a handful of ids.
  Upgrading to `blobatar@3` will change every existing face by design.
- Normalization means ids are effectively case-insensitive. That is good (two spellings of the
  same machine id agree) but it means `Agent-A` and `agent-a` are the *same* creature — pick a
  canonical form and stick to it.
- Pinning `traits` is possible but pinning *everything* makes the name irrelevant. The useful
  middle ground is to lock one or two brand traits (e.g. `background`, or a shape list) and let
  the id drive the rest.

### 3.4 Shapes, traits, contrast

Ten silhouettes: `round`, `organic`, `boxy`, `nub`, `cloud`, `sun`, `capsule`, `triangle`,
`hexagon`, `droplet`. Weighted so rounds/pebbles are common and louder shapes are rare.

- Every axis is a named trait at a 0–1 position (`shape`, `eye.ratio`, `eye.gap`, …). Keys are
  stable across minors; the *ranges* are frozen per major.
- Trait keys are not enumerated in the README on purpose — they follow the layout. Read them
  off `styles/compose.ts` / `styles/shapes.ts` if h4rn needs a tuned mapping.
- Contrast: eyes clear 4.5:1 against the body at every hue and tone, verified at 1° resolution
  in the project's test suite; polarity flips automatically for near-black bodies. Passing
  `palette` opts out by definition — do not use `palette` if the design cares about a11y.
- Layout is always solved in full, so no trait combination can put an eye outside the body or
  geometry outside the frame. An extreme pin can land short of the requested position;
  `_layout()` reports the resolved value.

### 3.5 Reduced motion

- Animated (`motion.css`) blobatars go **fully static** under `prefers-reduced-motion: reduce`
  (media query at `motion.css:1193`, which neutralizes `.mo-root`, `.mo-breathe`, `.mo-bob`,
  `.mo-eyes`, `.mo-eye`, and children).
- `gaze` does not attach at all under reduced motion or without a fine pointer, and both are
  watched rather than sampled once.
- Static rendering (the default) is unaffected either way — it is one `<img>`.

### 3.6 SSR and WebView compatibility

- **Fully SSR-safe**: the core is a pure string → string function with no DOM access. It can run
  in a Rust-side pre-render, a build step, or an email/OG-image pipeline.
- `@blobatar/react` static mode emits `<img src="data:image/svg+xml,…">`, which every WebView
  decodes. Animated mode emits inline SVG and relies on **CSS `@property` registration** —
  `motion.css` registers 15 custom properties and `gaze.css` 4. That is the one compatibility
  risk: `@property` needs Chromium 85+ / Safari 16.4+ / WebKitGTK ≈2.42+. On an older Linux
  WebKitGTK the avatar still *renders*, but pose transitions degrade to snaps or no-ops.
  Tauri v2 on a current distro is fine; pin a WebKitGTK floor in the Linux bundle if animation
  is a designed behaviour.
- The `animate` layer is CSS-only (no rAF, no JS) for the idle motion, which is why it is cheap
  for a large grid — but it also means every animated avatar is a live CSS animation. Use
  `"hover"` (one at a time) for lists.

### 3.7 Scale guidance

| Scenario | Recommended mode | Why |
|---|---|---|
| Sidebar / machine list / thread list (10–500 entries) | static `<img>` (omit `animate`) | 1 element each, no live animations, no id collisions |
| One profile header / onboarding | `animate="always"` + optional `useGaze` | ambient motion reads as a creature, not noise |
| Grid where one is hovered | `animate="hover"` | exactly one live animation |
| Server-rendered HTML / OG image | string API (`blobatar` / `blobatarUri`) | no DOM, byte-stable within a major |

---

## 4. Agent state → orb mapping

The orb library has nine states and h4rn has ten. Below is a proposed mapping that uses only
documented props, and synthesizes the four states the library does not have. Everything in the
"props" column is a real, typed prop.

| h4rn agent state | Orb `state` | Extra props | Rationale |
|---|---|---|---|
| `idle` | `breathing` | `paused` (or `speed={0.4}`) | Slow morphing ring reads as alive but not working. Prefer `paused` when the daemon is genuinely doing nothing; keep `speed 0.4` only if a subtle "present" cue is wanted. |
| `thinking` (model reasoning) | `working` | `speed={1}` | Particles on tilted orbits is the canonical "cogitating" animation. |
| `searching` (web/grep/glob) | `searching` | — | Exact match: a scan meridian sweeping a globe. |
| `reading` (file/URL read) | `listening` | `speed={0.9}` | Waveform rolling through rings reads as data flowing *in*, distinct from search's outward scan. |
| `editing` (diff/write) | `weaving` | — | Three strands plaiting = content being merged into a whole. |
| `running tool` (exec/build) | `solving` | — | Bands scramble then click back solved: discrete steps resolving into a fixed state, which is exactly a tool call finishing. |
| `waiting for approval` | `shaping` | `speed={0.8}`, `opts={{ shape: 0 }}` | Circle→triangle→square morph is legible as "choose a shape"; holding circle stops it looking like progress. Pair with the approval affordance, not instead of it. |
| `rate-limited` | `breathing` | `speed={0.35}` | A slow, stalled pulse. Deliberately the same *mode* as idle at a different speed, so it reads as "idle, but against a clock". Add a countdown in text; the orb must not imply work. |
| `error` | `shaping` | `paused`, `opts={{ shape: 2 }}`, `color="#e5484d"` | A held square = stopped, not cycling. Color is the only non-monochrome cue; if the design forbids tint, drop `color` and rely on the icon/affordance. |
| `done` | `shaping` | `paused`, `opts={{ shape: 2 }}` | Same settled glyph as error, tinted normally. Consider replacing both terminal states with a static icon — an orb that never moves again is a glyph with extra steps. |

Notes and caveats:

- **Only `working`, `searching`, and `solving` are unambiguous.** `listening` vs `searching` and
  `weaving` vs `composing` are aesthetic choices; the mapping above picks one and should be
  treated as a design decision, not a fact.
- **Do not use `connecting` for `connecting`.** It is tempting, but h4rn's "connecting" (transport
  handshake) is short and usually better shown by a disabled control than by an animation that
  takes ~2 s to read. Keep `connecting` unused, or reserve it for first-run harness setup.
- **`speed` is a multiplier on a baked preset**, not seconds. Values outside ~0.3–2 start to
  fight the tuning; 0.35 for rate-limited is at the slow edge on purpose.
- **`size`**: use `20` inline (next to a status word, in a row of thread metadata) and `64` for a
  chat avatar. Avoid `32`: the type comment says it is interpolated in log space and has **not
  had a tuning pass**.
- **Reduced motion** automatically collapses every row above to a static frame — the mapping
  survives with no extra work. That is a strong argument for this library over a bespoke
  animation.
- **One orb per visible turn**, not per message. Offscreen pausing keeps a long transcript
  cheap, but each visible orb is still a canvas plus a rAF callback — so
  `IntersectionObserver` is per-instance and a 500-row transcript of orbs is still 500 canvases.

---

## 5. Blobatar seed scheme

The seed is any string; normalization is NFC + trim + lowercase, and every trait is derived by
key from that one hash. So the scheme is really about **what string identifies whom**, and
about never re-using a string for two different kinds of entity.

Proposed canonical forms (choose a separator that cannot appear in an id; `:` is safe for
UUIDs and slugs, and normalization will lowercase the whole thing anyway):

| Entity | Seed string | Example |
|---|---|---|
| Machine | `machine:<machine_id>` | `machine:0f3a…` |
| Harness (provider on a machine) | `harness:<machine_id>:<harness_kind>` | `harness:0f3a…:claude-code` |
| Harness instance (multi-account) | `harness:<machine_id>:<harness_kind>:<instance_id>` | `harness:0f3a…:antigravity:work` |
| Subagent | `subagent:<machine_id>:<parent_thread_id>:<subagent_id>` | `subagent:0f3a…:th_7c…:sa_2` |
| Thread / session (if an avatar is wanted) | `thread:<machine_id>:<thread_id>` | `thread:0f3a…:th_7c…` |
| Human user | `user:<user_id>` | `user:u_91…` |

Design rules that follow from the implementation:

1. **Type-prefix every seed.** Because traits are key-addressed and avalanche is strong, a
   machine `0f3a…` and a thread `0f3a…` would otherwise be unrelated faces with no way to tell
   them apart; the prefix makes the namespace explicit and prevents an id collision between
   entity kinds from silently aliasing two creatures.
2. **Never put mutable display data in the seed.** `harness:…:Claude Code` would change the face
   when the harness is renamed. Use the stable kind/instance id, not the label.
3. **Expect case-insensitivity.** `subagent:…:SA_2` and `subagent:…:sa_2` are the same face.
   Emit lowercase from the daemon so the UI and the server agree without thinking about it.
4. **Keep the parent in the child's seed** (as above). A subagent's face is then stable across
   restarts and unique per parent, without a global registry — and it visually belongs to its
   thread by construction of the string, which is a nice property even though the geometry
   itself is not hierarchical.
5. **Do not pin `hue`/`tone` per role unless the design wants role colour.** Pinning hue makes
   all harnesses share a colour family, which is a legitimate choice, but it removes the
   "nobody chose their colour" property and makes two harnesses of the same kind look like the
   same entity. If role colour is wanted, lock `hue` only, and keep shape driven by the id.
6. **Pin the version.** Same string = same face within a major. A `blobatar@3` bump changes
   every face; treat that as a deliberate migration with a golden test, not a dependency bump.
7. **Use `background="circle"` or `"squircle"` for machine/harness chips**, and transparent
   (default) for standalone thread avatars, so chips compose with the surrounding chrome.
8. **`title`** should be the human label, always. The seed is not readable; the `<title>` is what
   a screen reader announces.

If h4rn wants a "stable per machine but visually distinct per harness" rule that is guaranteed
rather than probabilistic, use `traits: { shape: [...] }` to narrow the silhouette per role
(e.g. machines only ever `round`/`cloud`/`sun`, subagents only `nub`/`capsule`/`droplet`).
Lists narrow without fixing, so the id still chooses within the allowed set.

---

## 6. Recommendation for h4rn

1. **Adopt `thinking-orbs@0.3.2` for transient agent state**, behind a small h4rn-owned
   `<AgentGlyph state={…}/>` that owns the mapping table in §4. Reasons: ~14.5 KB gzipped with no
   dependencies, offscreen pausing that stops both painting and rAF (so a long thread is cheap), true
   reduced-motion support, and 2D canvas only — the safest rendering path across WebKitGTK,
   WKWebView and WebView2.
2. **Do not adopt `gravity`.** It needs per-platform cursor rasters and is a novelty in a
   control surface.
3. **Adopt `blobatar@2.7.0` + `@blobatar/react@2.7.0` for machine/harness/subagent identity**,
   static mode by default (`<img>` data URI), `animate="hover"` only where a single large avatar
   is the subject. Use the seed scheme in §5 and pin the version.
4. **Consider `expression={thinking}` on the harness avatar** as a complement to the orb, not a
   replacement: the orb answers "what is happening", the face answers "who is doing it". Do not
   run both animations at full amplitude in the same row — pick one as primary.
5. **Guard `@property` on Linux.** If animated blobatars are a designed behaviour, set a
   WebKitGTK floor (≈2.42) in the Linux bundle metadata, or use static avatars on Linux.
6. **Keep both libraries out of the Rust core.** Neither has a server-side role: orbs are a
   render-time concern and blobatars are a pure function. The one exception worth noting is
   that `blobatar`'s string API is pure enough to pre-render on the Rust side if h4rn ever needs
   avatars in a native notification or an exported report.

---

## 7. Sources

- `thinking-orbs@0.3.2` tarball → `/tmp/h4rn-research/pkgs/orbs/package`:
  `README.md`, `dist/index.d.ts`, `dist/types.d.ts`, `dist/ThinkingOrb.d.ts`, `dist/theme.d.ts`,
  `dist/presets.d.ts`, `dist/engine/registry.d.ts`, `dist/index.es.js`, `LICENSE`.
- `blobatar@2.7.0` tarball → `/tmp/h4rn-research/pkgs/blob/package`:
  `README.md`, `package.json`, `src/hash.ts`, `src/uri.ts`, `dist/render.d.ts`,
  `dist/blobatar.d.ts`, `dist/animate.d.ts`, `dist/expression.d.ts`, `dist/idle.d.ts`,
  `src/motion.css`, `LICENSE`.
- `@blobatar/react@2.7.0` tarball → `/tmp/h4rn-research/pkgs/react/package`:
  `README.md`, `dist/index.js`, `dist/index.d.ts`, `dist/react.d.ts` (via `blobatar`).
- Registry metadata: `npm view thinking-orbs --json`, `npm view blobatar --json`,
  `npm view @blobatar/react --json` (versions, licenses, peers, timestamps).
- Sites read for prose only: <https://libraries.dev/orbs>, <https://blobatar.dev>.
  Claims taken from the sites and not confirmed in the packages are marked **UNVERIFIED**
  above (specifically: published SwiftUI / React Native ports).
