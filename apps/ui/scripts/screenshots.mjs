/**
 * Capture the Phase 1b and Phase 2 (IDE) surfaces in mock mode.
 *
 * Usage: node scripts/screenshots.mjs [baseURL]
 *
 * Requires `bun run dev` and a Playwright Chromium in ~/.cache/ms-playwright.
 * Override the binary with H4RN_CHROMIUM.
 */
import { mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const BASE = process.argv[2] ?? "http://127.0.0.1:1420";
const OUT = fileURLToPath(new URL("../screenshots/", import.meta.url));
const EXECUTABLE =
  process.env.H4RN_CHROMIUM ??
  `${process.env.HOME}/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell`;

const DESKTOP = { width: 1440, height: 900 };
const PHONE = { width: 390, height: 844 };

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
    name: "terminal",
    path: "/t/thr_b",
    wait: '[aria-label="Thread stream"]',
    settle: 2200,
    action: async (page) => {
      await page.getByRole("button", { name: "Toggle terminal" }).click();
      const panel = page.locator('[aria-label="Terminal panel"]');
      await panel.waitFor({ state: "visible" });
      await panel.locator(".xterm-screen").first().waitFor({ state: "visible" });
      // The panel mounts before the attach response; input is dropped until the
      // stream id arrives, so give the mock session a beat to finish opening.
      await page.waitForTimeout(700);
      await panel.locator(".terminal-surface").click();
      await page.keyboard.type("ls");
      await page.keyboard.press("Enter");
      await page.waitForTimeout(900);
    },
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
    name: "ide",
    path: "/ide",
    wait: '[aria-label="IDE"]',
    settle: 1600,
  },
  {
    name: "ide-diff",
    path: "/ide",
    wait: '[aria-label="IDE"]',
    settle: 1600,
    action: async (page) => {
      await page.getByRole("button", { name: "Toggle diff" }).click();
      await page.locator('section[aria-label="Diff"]').waitFor({ timeout: 10_000 });
      await page.waitForTimeout(500);
    },
  },
  {
    name: "ide-search",
    path: "/ide",
    wait: '[aria-label="IDE"]',
    settle: 1600,
    action: async (page) => {
      await page.getByRole("button", { name: "Search workspace" }).click();
      const input = page.getByLabel("Find in workspace");
      await input.waitFor({ timeout: 10_000 });
      await input.fill("verifySession");
      await input.press("Enter");
      await page.waitForTimeout(600);
    },
  },
];

const browser = await chromium.launch({ executablePath: EXECUTABLE, args: ["--no-sandbox"] });
await mkdir(OUT, { recursive: true });

// H4RN_SHOTS=ide,ide-diff re-captures a subset while iterating on one screen.
const only = process.env.H4RN_SHOTS?.split(",")
  .map((name) => name.trim())
  .filter(Boolean);
const selected = only?.length ? SHOTS.filter((shot) => only.includes(shot.name)) : SHOTS;

let failures = 0;
for (const shot of selected) {
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
