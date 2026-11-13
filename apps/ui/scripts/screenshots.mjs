/**
 * Capture the Phase 1b surfaces in mock mode.
 *
 * Usage: node scripts/screenshots.mjs [baseURL]
 *
 * Requires `bun run dev` and a Playwright Chromium in ~/.cache/ms-playwright.
 * Override the binary with H4RN_CHROMIUM.
 */
import { mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const BASE = process.argv[2] ?? "http://127.0.0.1:1420";
const OUT = fileURLToPath(new URL("../screenshots/", import.meta.url));
const EXECUTABLE =
  process.env.H4RN_CHROMIUM ??
  `${process.env.HOME}/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell`;

const DESKTOP = { width: 1440, height: 900 };
const PHONE = { width: 390, height: 844 };

const FIXTURE_NAME = "reconnect-trace.svg";
const FIXTURE_DIR = join(tmpdir(), "h4rn-attachments");
const FIXTURE = join(FIXTURE_DIR, FIXTURE_NAME);

/** Small deterministic image so the composer chip and transcript preview have content. */
const FIXTURE_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="720" height="420" viewBox="0 0 720 420">
  <rect width="720" height="420" fill="#f1e8d6"/>
  <rect width="720" height="34" fill="#e3d8c0"/>
  <text x="360" y="21" text-anchor="middle" font-family="ui-monospace, monospace" font-size="12" fill="#6b5f4c">relay/reconnect.ts — retry trace</text>
  <rect x="0" y="34" width="180" height="386" fill="#eae0cc"/>
  <text x="16" y="64" font-family="ui-monospace, monospace" font-size="11" fill="#7a6c56">src</text>
  <text x="24" y="88" font-family="ui-monospace, monospace" font-size="11" fill="#9a5b14">reconnect.ts</text>
  <text x="24" y="108" font-family="ui-monospace, monospace" font-size="11" fill="#6b5f4c">backoff.ts</text>
  <rect x="180" y="34" width="540" height="386" fill="#f6efe2"/>
  <text x="200" y="68" font-family="ui-monospace, monospace" font-size="12" fill="#6b5f4c">attempt 1 · 250 ms</text>
  <text x="200" y="96" font-family="ui-monospace, monospace" font-size="12" fill="#9a3f3a">  close 1011 · retry after jitter</text>
  <text x="200" y="128" font-family="ui-monospace, monospace" font-size="12" fill="#6b5f4c">attempt 2 · 610 ms</text>
  <text x="200" y="156" font-family="ui-monospace, monospace" font-size="12" fill="#3f6a45">  resumed at seq 4182 · no replay</text>
  <text x="200" y="188" font-family="ui-monospace, monospace" font-size="12" fill="#6b5f4c">attempt 3 · 1.4 s</text>
  <text x="200" y="216" font-family="ui-monospace, monospace" font-size="12" fill="#3f6a45">  stream caught up · events 4183–4201</text>
  <rect x="200" y="254" width="470" height="84" rx="4" fill="#e6f0e3" stroke="#9bb59a"/>
  <text x="214" y="278" font-family="ui-monospace, monospace" font-size="12" fill="#3f6a45">PASS  relay/reconnect.test.ts</text>
  <text x="214" y="300" font-family="ui-monospace, monospace" font-size="11" fill="#5c7a58">resume does not replay applied events</text>
  <text x="214" y="320" font-family="ui-monospace, monospace" font-size="11" fill="#5c7a58">18 tests · 1.9 s</text>
</svg>`;

/** @param {string} path @param {string} theme */
const url = (path, theme) => `${BASE}${path}${path.includes("?") ? "&" : "?"}mock=1&theme=${theme}`;

const SHOTS = [
  {
    name: "empty",
    path: "/?scenario=empty",
    wait: "text=No threads yet.",
  },
  {
    name: "inbox",
    path: "/",
    wait: "text=Fix the flaky auth test",
  },
  {
    name: "thread-stream",
    path: "/t/thr_b",
    wait: '[aria-label="Thread stream"]',
    settle: 3200,
  },
  {
    name: "thread-approval",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
  },
  {
    name: "palette",
    path: "/",
    wait: "text=Fix the flaky auth test",
    action: async (page) => {
      await page.keyboard.press("Control+k");
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(350);
    },
  },
  {
    name: "attachments-composer",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.locator('input[type="file"]').setInputFiles(FIXTURE);
      await page.getByText(FIXTURE_NAME).first().waitFor({ timeout: 15_000 });
      await page.waitForTimeout(600);
    },
  },
  {
    name: "attachments-lightbox",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Open auth-race.svg" }).click();
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(500);
    },
  },
];

const browser = await chromium.launch({ executablePath: EXECUTABLE, args: ["--no-sandbox"] });
await mkdir(OUT, { recursive: true });
await mkdir(FIXTURE_DIR, { recursive: true });
await writeFile(FIXTURE, FIXTURE_SVG);

let failures = 0;
for (const shot of SHOTS) {
  for (const [sizeName, size] of [
    ["1440", DESKTOP],
    ["390", PHONE],
  ]) {
    for (const theme of ["light", "dark"]) {
      const context = await browser.newContext({
        viewport: size,
        deviceScaleFactor: 1,
        colorScheme: theme,
      });
      const page = await context.newPage();
      const file = `${OUT}${shot.name}-${sizeName}-${theme}.png`;
      try {
        await page.goto(url(shot.path, theme), { waitUntil: "domcontentloaded" });
        await page.locator(shot.wait).first().waitFor({ timeout: 15_000 });
        await page.waitForTimeout(shot.settle ?? 700);
        if (shot.action) await shot.action(page);
        await page.screenshot({ path: file, animations: "disabled" });
        console.log(`ok  ${file}`);
      } catch (error) {
        failures += 1;
        console.error(`FAIL ${file}: ${error instanceof Error ? error.message : error}`);
        await page.screenshot({ path: file, animations: "disabled" }).catch(() => undefined);
      } finally {
        await context.close();
      }
    }
  }
}

await browser.close();
if (failures > 0) {
  console.error(`${failures} screenshot(s) failed`);
  process.exit(1);
}
