/**
 * Capture the Quiet native surfaces in mock mode.
 *
 * Usage: node scripts/screenshots.mjs [baseURL]
 *
 * Requires `bun run dev` and a Playwright Chromium in ~/.cache/ms-playwright.
 * Override the binary with H4RN_CHROMIUM.
 *
 * The shot list mirrors design/a-quiet-native/shots one for one. Desktop
 * screenshots are 1440×900, phone screenshots are 390×844, and every screen is
 * captured in light and dark.
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

/** @param {import("playwright-core").Page} page */
const openTerminal = async (page) => {
  await page.getByRole("button", { name: "Toggle terminal" }).click();
  const panel = page.locator('[aria-label="Terminal panel"]');
  await panel.waitFor({ state: "visible" });
  await panel.locator(".xterm-screen").first().waitFor({ state: "visible" });
  // The panel mounts before the attach response; input is dropped until the
  // stream id arrives, so give the mock session a beat to finish opening.
  await page.waitForTimeout(700);
  await panel.locator(".terminal-surface").click();
  await page.keyboard.type("CI=1 pnpm vitest run --project auth --repeat 3");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(900);
};

/**
 * Each shot lists the design reference it ports. `device` decides the viewport
 * so a desktop screen is never captured at phone width (or the reverse).
 * @type {Array<{
 *   name: string;
 *   device: "desktop" | "phone";
 *   path: string;
 *   wait: string | (() => Promise<void>);
 *   settle?: number;
 *   action?: (page: import("playwright-core").Page) => Promise<void>;
 * }>}
 */
const SHOTS = [
  {
    name: "inbox",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
  },
  {
    name: "inbox-done",
    device: "desktop",
    path: "/t/thr_h",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("tab", { name: /Done/ }).click();
      await page.waitForTimeout(400);
    },
  },
  {
    name: "subagents",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Toggle subagents" }).click();
      await page.getByRole("complementary", { name: "Subagents" }).waitFor();
      await page.waitForTimeout(400);
    },
  },
  {
    name: "terminal",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: openTerminal,
  },
  {
    name: "ide",
    device: "desktop",
    path: "/ide",
    wait: '[aria-label="IDE"]',
    settle: 1600,
    action: async (page) => {
      const tree = page.getByRole("tree", { name: "Workspace files" });
      const expand = async (path) => {
        const node = tree.locator(`button[title="${path}"]`).first();
        if (!(await node.count())) return;
        if ((await node.getAttribute("aria-expanded")) === "false") {
          await node.click();
          await page.waitForTimeout(250);
        }
      };
      const open = async (path) => {
        const node = tree.locator(`button[title="${path}"]`).first();
        if (await node.count()) {
          await node.click();
          await page.waitForTimeout(250);
        }
      };
      await expand("src");
      await expand("src/auth");
      await open("src/auth/session.test.ts");
      await expand("tests");
      await open("tests/setup.ts");
      await expand("src/db");
      await open("src/db/pool.ts");
      const tab = page.getByRole("tab", { name: /session\.test\.ts/ }).first();
      if (await tab.count()) await tab.click();
      await page.getByRole("tab", { name: "vs agent" }).click();
      await page.waitForTimeout(700);
    },
  },
  {
    name: "library",
    device: "desktop",
    path: "/library",
    wait: "text=playwright",
    settle: 1200,
  },
  {
    name: "usage",
    device: "desktop",
    path: "/usage",
    wait: "text=Tokens per day by harness",
    settle: 1200,
  },
  {
    name: "pr",
    device: "desktop",
    path: "/pulls",
    wait: "text=Fix flaky auth test",
    settle: 1200,
  },
  {
    name: "set-hosts",
    device: "desktop",
    path: "/settings/hosts",
    wait: "text=Machines that run agents",
    settle: 1200,
  },
  {
    name: "set-devices",
    device: "desktop",
    path: "/settings/devices",
    wait: "text=Pair a device",
    settle: 1200,
  },
  {
    name: "set-providers",
    device: "desktop",
    path: "/settings/providers",
    wait: "text=CLIProxyAPI on atlas",
    settle: 1200,
  },
  {
    name: "palette",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.keyboard.press("Control+k");
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(350);
    },
  },
  {
    name: "state-empty",
    device: "desktop",
    path: "/?scenario=empty",
    wait: "text=No threads yet",
    settle: 500,
  },
  {
    name: "state-loading",
    device: "desktop",
    path: "/t/thr_a?scenario=loading",
    wait: '[aria-label="Loading thread"]',
    settle: 600,
  },
  {
    name: "state-offline",
    device: "desktop",
    path: "/t/thr_a?scenario=offline",
    wait: "text=atlas is unreachable from this device",
    settle: 500,
  },
  {
    name: "state-error",
    device: "desktop",
    path: "/t/thr_e?scenario=error",
    wait: "text=This thread couldn’t be loaded",
    settle: 500,
  },
  {
    name: "p-inbox",
    device: "phone",
    path: "/",
    wait: "text=Fix flaky auth test",
  },
  {
    name: "p-thread",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
  },
  {
    name: "p-approval",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Review" }).click();
      await page.getByRole("dialog", { name: "Approve command" }).waitFor();
      await page.waitForTimeout(450);
    },
  },
  {
    name: "p-terminal",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: openTerminal,
  },
  {
    name: "p-pairing",
    device: "phone",
    path: "/pairing",
    wait: "text=Pair with atlas",
    settle: 500,
  },
  {
    name: "p-usage",
    device: "phone",
    path: "/usage",
    wait: "text=Migrate session store is paused",
    settle: 1200,
  },
];

const browser = await chromium.launch({ executablePath: EXECUTABLE, args: ["--no-sandbox"] });
await mkdir(OUT, { recursive: true });

// H4RN_SHOTS=inbox,ide re-captures a subset while iterating on one screen.
const only = process.env.H4RN_SHOTS?.split(",")
  .map((name) => name.trim())
  .filter(Boolean);
const selected = only?.length ? SHOTS.filter((shot) => only.includes(shot.name)) : SHOTS;

let failures = 0;
for (const shot of selected) {
  const size = shot.device === "phone" ? PHONE : DESKTOP;
  const sizeName = shot.device === "phone" ? "390" : "1440";
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
      if (typeof shot.wait === "string") {
        await page.locator(shot.wait).first().waitFor({ timeout: 15_000 });
      } else {
        await shot.wait();
      }
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

await browser.close();
if (failures > 0) {
  console.error(`${failures} screenshot(s) failed`);
  process.exit(1);
}
