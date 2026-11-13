import { blake3Hex } from "./chunker";

/**
 * Demo bytes for `?mock=1`.
 *
 * The image is an SVG so it stays crisp at any thumbnail size and costs a few
 * kilobytes in the bundle. Its content address is computed with the same BLAKE3
 * helper the real uploader uses, so the mock transcript and the mock blob store
 * agree on the id.
 */
export const DEMO_IMAGE_NAME = "auth-race.svg";
export const DEMO_IMAGE_MIME = "image/svg+xml";
export const DEMO_IMAGE_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="720" height="420" viewBox="0 0 720 420">
  <rect width="720" height="420" fill="#15171c"/>
  <rect width="720" height="34" fill="#1d2027"/>
  <circle cx="18" cy="17" r="5" fill="#e06c75"/>
  <circle cx="34" cy="17" r="5" fill="#d8a657"/>
  <circle cx="50" cy="17" r="5" fill="#7fb069"/>
  <text x="360" y="21" text-anchor="middle" font-family="ui-monospace, SFMono-Regular, monospace" font-size="12" fill="#7b8290">auth/session.test.ts — CI run 4182</text>
  <rect x="0" y="34" width="176" height="386" fill="#1a1c22"/>
  <text x="16" y="64" font-family="ui-monospace, monospace" font-size="11" fill="#6f7684">tests</text>
  <text x="24" y="88" font-family="ui-monospace, monospace" font-size="11" fill="#d29a4a">session.test.ts</text>
  <text x="24" y="108" font-family="ui-monospace, monospace" font-size="11" fill="#8a919e">setup.ts</text>
  <text x="24" y="128" font-family="ui-monospace, monospace" font-size="11" fill="#8a919e">helpers.ts</text>
  <text x="16" y="162" font-family="ui-monospace, monospace" font-size="11" fill="#6f7684">src</text>
  <text x="24" y="186" font-family="ui-monospace, monospace" font-size="11" fill="#8a919e">session.ts</text>
  <text x="24" y="206" font-family="ui-monospace, monospace" font-size="11" fill="#8a919e">pool.ts</text>
  <rect x="176" y="34" width="544" height="386" fill="#17191f"/>
  <text x="196" y="68" font-family="ui-monospace, monospace" font-size="12" fill="#7fb069">  34  it("expires at the boundary", async () => {</text>
  <line x1="196" y1="80" x2="700" y2="80" stroke="#3a2a2a" stroke-width="1"/>
  <text x="196" y="98" font-family="ui-monospace, monospace" font-size="12" fill="#e06c75">-  35    await db.connect();</text>
  <text x="196" y="118" font-family="ui-monospace, monospace" font-size="12" fill="#e06c75">-  36    vi.useFakeTimers();</text>
  <text x="196" y="140" font-family="ui-monospace, monospace" font-size="12" fill="#98c379">+  35    vi.useFakeTimers();</text>
  <text x="196" y="160" font-family="ui-monospace, monospace" font-size="12" fill="#98c379">+  36    await db.connect();</text>
  <text x="196" y="180" font-family="ui-monospace, monospace" font-size="12" fill="#7b8290">  37    expect(await store.get(key)).toBeNull();</text>
  <text x="196" y="200" font-family="ui-monospace, monospace" font-size="12" fill="#7b8290">  38  });</text>
  <rect x="196" y="230" width="486" height="70" rx="4" fill="#231f1f" stroke="#5a3232"/>
  <text x="210" y="252" font-family="ui-monospace, monospace" font-size="12" fill="#e06c75">FAIL  tests/session.test.ts</text>
  <text x="210" y="272" font-family="ui-monospace, monospace" font-size="11" fill="#a98a8a">AssertionError: expected 1700000000000 to be null</text>
  <text x="210" y="290" font-family="ui-monospace, monospace" font-size="11" fill="#a98a8a">first lookup raced the timer advance on the CI runner</text>
  <rect x="196" y="318" width="486" height="60" rx="4" fill="#1b231d" stroke="#33513a"/>
  <text x="210" y="340" font-family="ui-monospace, monospace" font-size="12" fill="#98c379">PASS  tests/session.test.ts</text>
  <text x="210" y="360" font-family="ui-monospace, monospace" font-size="11" fill="#8fa996">48 tests passed in 12.4 s · 3 runs with CI=1</text>
</svg>`;

export const DEMO_IMAGE_BYTES = new TextEncoder().encode(DEMO_IMAGE_SVG);
export const DEMO_IMAGE_ID = blake3Hex(DEMO_IMAGE_BYTES);

export const DEMO_LOG_NAME = "flaky-auth.log";
export const DEMO_LOG = `$ CI=1 pnpm test --filter auth
FAIL tests/session.test.ts > expires at the boundary
AssertionError: expected 1700000000000 to be null
  at tests/session.test.ts:37:28

$ CI=1 pnpm test --filter auth
PASS tests/session.test.ts
48 tests passed in 12.4 s
`;
export const DEMO_LOG_BYTES = new TextEncoder().encode(DEMO_LOG);
export const DEMO_LOG_ID = blake3Hex(DEMO_LOG_BYTES);
