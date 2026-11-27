/**
 * Capture the Quiet native surfaces in mock mode.
 *
 * Usage: node scripts/screenshots.mjs [baseURL]
 *
 * Requires `bun run dev` and a Playwright Chromium in ~/.cache/ms-playwright.
 * Override the binary with H4RN_CHROMIUM.
 *
 * The shot list mirrors design/a-quiet-native/shots one for one, plus the
 * surfaces the design shots do not cover (PR list, the change-set card, and
 * the phone PR screens). Desktop screenshots are 1440×900, phone screenshots
 * are 390×844 (plus the 360×780 floor), and every screen is captured in light
 * and dark.
 */
import { mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const BASE = process.argv[2] ?? "http://127.0.0.1:1420";
const OUT = process.env.H4RN_SHOTS_OUT
  ? `${process.env.H4RN_SHOTS_OUT.replace(/\/$/, "")}/`
  : fileURLToPath(new URL("../screenshots/", import.meta.url));
const EXECUTABLE =
  process.env.H4RN_CHROMIUM ??
  `${process.env.HOME}/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell`;

const DESKTOP = { width: 1440, height: 900 };
const PHONE = { width: 390, height: 844 };
/** The narrowest phone we promise to support. */
const PHONE_SMALL = { width: 360, height: 780 };

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

/** @param {import("playwright-core").Page} page */
const openPanelTab = async (page, name) => {
  await page.getByRole("button", { name: "Right panel" }).click();
  const panel = page.getByRole("complementary", { name: "Thread panel" });
  await panel.waitFor({ state: "visible" });
  await panel.getByRole("tab", { name: new RegExp(name) }).click();
  return panel;
};

/** Expand a directory row in the right panel's file tree. */
const expandTree = async (page, path) => {
  const tree = page.getByRole("tree", { name: "Workspace files" });
  await tree.waitFor({ timeout: 10_000 });
  const node = tree.locator(`button[title="${path}"]`).first();
  if (!(await node.count())) return;
  if ((await node.getAttribute("aria-expanded")) === "false") {
    await node.click();
    await page.waitForTimeout(250);
  }
};

/** Open a file from the right panel's file tree. */
const openTreeFile = async (page, path) => {
  const tree = page.getByRole("tree", { name: "Workspace files" });
  await tree.waitFor({ timeout: 10_000 });
  const node = tree.locator(`button[title="${path}"]`).first();
  const appeared = await node
    .waitFor({ state: "visible", timeout: 5_000 })
    .then(() => true)
    .catch(() => false);
  if (appeared) {
    await node.click();
    await page.waitForTimeout(250);
  }
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
    name: "panel-changes",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      const panel = await openPanelTab(page, "Changes");
      await panel.getByText("tests/setup.ts").first().waitFor({ timeout: 10_000 });
      await page.waitForTimeout(500);
    },
  },
  {
    name: "subagents",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      const panel = await openPanelTab(page, "Subagents");
      await panel
        .getByText("Bisect the auth fixture change on CI")
        .first()
        .waitFor({ timeout: 10_000 });
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
      await expandTree(page, "src");
      await expandTree(page, "src/auth");
      await openTreeFile(page, "src/auth/session.test.ts");
      await expandTree(page, "tests");
      await openTreeFile(page, "tests/setup.ts");
      await expandTree(page, "src/db");
      await openTreeFile(page, "src/db/pool.ts");
      const tab = page.getByRole("tab", { name: /session\.test\.ts/ }).first();
      if (await tab.count()) await tab.click();
      // The reference document has an unsaved edit, so the tab shows a dirty
      // dot. Leave a trailing space in the buffer to capture the same state.
      await page.getByRole("tab", { name: "Edit" }).click();
      await page.waitForTimeout(400);
      await page.locator(".q-code .cm-content").first().click();
      await page.keyboard.press("Control+End");
      await page.keyboard.type(" ");
      await page.getByRole("tab", { name: "Diff" }).click();
      await page.getByRole("combobox", { name: /Diff source/ }).click();
      await page.getByRole("option", { name: /^Turn/ }).click();
      await page.waitForTimeout(700);
    },
  },
  {
    name: "ide-switch",
    device: "desktop",
    path: "/ide",
    wait: '[aria-label="IDE"]',
    settle: 1600,
    action: async (page) => {
      const sidebar = page.getByRole("navigation", { name: "Threads" });
      // Open three files in the relay project, leave, and come back: the
      // project keeps its tabs and says so quietly.
      await sidebar.locator('[data-thread-id="thr_b"]').click();
      await page.getByRole("heading", { name: "h4rn-connect" }).waitFor({ timeout: 10_000 });
      await expandTree(page, "src");
      await expandTree(page, "src/auth");
      await openTreeFile(page, "src/auth/session.test.ts");
      await expandTree(page, "tests");
      await openTreeFile(page, "tests/setup.ts");
      await expandTree(page, "src/db");
      await openTreeFile(page, "src/db/pool.ts");
      await page
        .getByRole("tab", { name: /session\.test\.ts/ })
        .first()
        .click();
      await page.getByRole("tab", { name: "vs agent" }).click();
      await sidebar.locator('[data-thread-id="thr_a"]').click();
      await page.getByRole("heading", { name: "api" }).waitFor({ timeout: 10_000 });
      await sidebar.locator('[data-thread-id="thr_b"]').click();
      await page.getByRole("heading", { name: "h4rn-connect" }).waitFor({ timeout: 10_000 });
      await page.getByText(/Restored \d+ tabs/).waitFor({ timeout: 10_000 });
      await page.getByRole("tab", { name: /Changes/ }).click();
      await page.getByText("tests/setup.ts").first().waitFor({ timeout: 10_000 });
      await page.waitForTimeout(500);
    },
  },
  {
    name: "sidebar-collapsed",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Hide sidebar" }).click();
      await page.getByRole("button", { name: /Show sidebar/ }).waitFor({ timeout: 10_000 });
      await page.waitForTimeout(400);
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
    name: "skills",
    device: "desktop",
    path: "/library/skills",
    wait: "text=Installed skills",
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
    action: async (page) => {
      await page.locator(".q-row").filter({ hasText: "#482" }).first().click();
      await page.getByRole("heading", { name: /Fix flaky auth test/ }).waitFor({ timeout: 10_000 });
      await page.waitForTimeout(700);
    },
  },
  {
    name: "pr-list",
    device: "desktop",
    path: "/pulls",
    wait: "text=Fix flaky auth test",
    settle: 1200,
  },
  {
    name: "change-set",
    device: "desktop",
    path: "/t/thr_h",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      const card = page.locator(".q-changes").first();
      await card.waitFor({ timeout: 10_000 });
      await card.scrollIntoViewIfNeeded();
      await page.waitForTimeout(600);
    },
  },
  {
    name: "set-hosts",
    device: "desktop",
    path: "/settings/hosts",
    wait: "text=Machines that run agents",
    settle: 1200,
  },
  {
    name: "set-harnesses",
    device: "desktop",
    path: "/settings/harnesses",
    wait: "text=CLI versions",
    settle: 1200,
    action: async (page) => {
      await page.getByRole("button", { name: "Update Codex" }).click();
      await page.getByText("added 1 package in 4s").waitFor({ timeout: 5_000 });
    },
  },
  {
    name: "set-about",
    device: "desktop",
    path: "/settings/general?appupdate=1",
    wait: "text=Check for updates",
    settle: 1200,
  },
  {
    name: "update-notice",
    device: "desktop",
    path: "/t/thr_a?appupdate=1",
    wait: "text=is available",
    settle: 2200,
  },
  {
    name: "update-downloading",
    device: "desktop",
    path: "/t/thr_a?appupdate=1",
    wait: "text=is available",
    settle: 2200,
    action: async (page) => {
      await page.getByRole("button", { name: "Restart to update" }).click();
      await page.getByText(/Downloading h4rn 0\.2\.0 · \d+%/).waitFor({ timeout: 5_000 });
    },
  },
  {
    name: "hosts-menu",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByTitle("Hosts and connection").click();
      await page.getByRole("menu", { name: "Hosts" }).waitFor();
      await page.waitForTimeout(350);
    },
  },
  {
    name: "set-devices",
    device: "desktop",
    path: "/settings/devices",
    wait: "text=Pair through relay",
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
    wait: "text=No projects yet",
    settle: 500,
  },
  {
    name: "state-loading",
    device: "desktop",
    path: "/t/thr_a?scenario=loading",
    wait: '[aria-label="Loading thread"]',
    // The mock opens the handshake, publishes cached hosts, then holds lists.
    settle: 1800,
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
    // The list reload fails after the simulated relay drop (~1.2 s), which is
    // when the sidebar banner appears. The reference also shows the hosts
    // popover open from the footer trigger.
    settle: 2200,
    action: async (page) => {
      await page.getByTitle("Hosts and connection").click();
      await page.getByRole("menu", { name: "Hosts" }).waitFor();
      await page.waitForTimeout(350);
    },
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
    name: "p-changes",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Changes and subagents" }).click();
      const sheet = page.getByRole("dialog", { name: "Changes and subagents" });
      await sheet.getByText("tests/setup.ts").first().waitFor({ timeout: 10_000 });
      await page.waitForTimeout(500);
    },
  },
  {
    name: "p-pr-list",
    device: "phone",
    path: "/pulls",
    wait: "text=Fix flaky auth test",
    settle: 1000,
  },
  {
    name: "p-pr",
    device: "phone",
    path: "/pulls",
    wait: "text=Fix flaky auth test",
    settle: 1000,
    action: async (page) => {
      await page.locator(".q-row").filter({ hasText: "#482" }).first().click();
      await page.getByRole("heading", { name: /Fix flaky auth test/ }).waitFor({ timeout: 10_000 });
      await page.waitForTimeout(700);
    },
  },
  {
    name: "p-change-set",
    device: "phone",
    path: "/t/thr_h",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      const card = page.locator(".q-changes").first();
      await card.waitFor({ timeout: 10_000 });
      await card.scrollIntoViewIfNeeded();
      await page.waitForTimeout(600);
    },
  },
  {
    name: "p-subagents",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Changes and subagents" }).click();
      const sheet = page.getByRole("dialog", { name: "Changes and subagents" });
      await sheet.getByRole("tab", { name: /Subagents/ }).click();
      await sheet
        .getByText("Bisect the auth fixture change on CI")
        .first()
        .waitFor({ timeout: 10_000 });
      await page.waitForTimeout(400);
    },
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
    wait: "text=Pair with orion",
    settle: 500,
  },
  {
    name: "p-usage",
    device: "phone",
    path: "/usage",
    wait: "text=Auto-resumes at",
    settle: 1200,
  },
  {
    name: "p-mcp",
    device: "phone",
    path: "/library",
    wait: "text=playwright",
    settle: 1000,
  },
  {
    name: "p-skills",
    device: "phone",
    path: "/library/skills",
    wait: "text=Installed skills",
    settle: 1000,
  },
];

/** Raise a toast through the dev server's own store module (mock mode only). */
const raiseToast = async (page, input) => {
  await page.evaluate(async (toast) => {
    const store = await import("/src/lib/store/appStore.ts");
    store.pushToast(toast);
  }, input);
  await page.waitForTimeout(450);
};

/** Surfaces the UI audit adds on top of the design shots (`H4RN_AUDIT=1`). */
const AUDIT_SHOTS = [
  {
    name: "a-new-thread",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "New thread", exact: true }).first().click();
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(400);
    },
  },
  {
    name: "a-shortcuts",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.keyboard.press("Shift+?");
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(400);
    },
  },
  {
    name: "a-thread-actions",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Thread actions" }).click();
      await page.getByRole("menu").first().waitFor({ state: "visible" });
      await page.waitForTimeout(350);
    },
  },
  {
    name: "a-row-actions",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page
        .getByRole("button", { name: "Actions for Migrate session store to Redis" })
        .click();
      await page.getByRole("menu").first().waitFor({ state: "visible" });
      await page.waitForTimeout(350);
    },
  },
  {
    name: "a-model-select",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Harness and model" }).click();
      await page.waitForTimeout(450);
    },
  },
  {
    name: "a-tooltip",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Right panel" }).hover();
      await page.waitForTimeout(1200);
    },
  },
  {
    name: "a-toasts",
    device: "desktop",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await raiseToast(page, { variant: "success", title: "Delegated to codex." });
      await raiseToast(page, {
        variant: "error",
        title: "Couldn't send the reply to atlas.",
        description: "The relay closed the stream. Your draft is still in the composer.",
      });
    },
  },
  {
    name: "a-settings-appearance",
    device: "desktop",
    path: "/settings/appearance",
    wait: 'main[aria-label="Settings"]',
    settle: 1200,
  },
  {
    name: "a-add-mcp",
    device: "desktop",
    path: "/library",
    wait: "text=playwright",
    settle: 1200,
    action: async (page) => {
      await page.getByRole("button", { name: "Add MCP server" }).first().click();
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(400);
    },
  },
  {
    name: "a-settings-autoresume",
    device: "desktop",
    path: "/settings/resume",
    wait: 'main[aria-label="Settings"]',
    settle: 1200,
  },
  {
    name: "a-settings-forges",
    device: "desktop",
    path: "/settings/forges",
    wait: "text=Forges",
    settle: 1200,
  },
  {
    name: "a-settings-account",
    device: "desktop",
    path: "/settings/account",
    wait: "text=Account",
    settle: 1200,
  },
  {
    name: "a-p-new-thread",
    device: "phone",
    path: "/",
    wait: "text=Fix flaky auth test",
    action: async (page) => {
      await page.getByRole("button", { name: "New thread" }).first().click();
      await page.getByRole("dialog").waitFor({ state: "visible" });
      await page.waitForTimeout(450);
    },
  },
  {
    name: "a-p-thread-actions",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await page.getByRole("button", { name: "Thread actions" }).click();
      await page.getByRole("menu").first().waitFor({ state: "visible" });
      await page.waitForTimeout(350);
    },
  },
  {
    name: "a-p-settings",
    device: "phone",
    path: "/settings/appearance",
    wait: "main",
    settle: 1200,
  },
  {
    name: "a-p-toasts",
    device: "phone",
    path: "/t/thr_a",
    wait: '[aria-label="Thread stream"]',
    action: async (page) => {
      await raiseToast(page, {
        variant: "error",
        title: "Couldn't send the reply to atlas.",
        description: "The relay closed the stream. Your draft is still in the composer.",
      });
    },
  },
];

if (process.env.H4RN_AUDIT === "1") SHOTS.push(...AUDIT_SHOTS);

const browser = await chromium.launch({ executablePath: EXECUTABLE, args: ["--no-sandbox"] });
await mkdir(OUT, { recursive: true });

// H4RN_SHOTS=inbox,ide re-captures a subset while iterating on one screen.
const only = process.env.H4RN_SHOTS?.split(",")
  .map((name) => name.trim())
  .filter(Boolean);
const selected = only?.length ? SHOTS.filter((shot) => only.includes(shot.name)) : SHOTS;

let failures = 0;
for (const shot of selected) {
  // H4RN_PHONE_FLOOR=0 skips the 360 px floor (the audit only needs 390).
  const phones = process.env.H4RN_PHONE_FLOOR === "0" ? [PHONE] : [PHONE, PHONE_SMALL];
  const sizes = shot.device === "phone" ? phones : [DESKTOP];
  for (const size of sizes) {
    const sizeName = String(size.width);
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
}

await browser.close();
if (failures > 0) {
  console.error(`${failures} screenshot(s) failed`);
  process.exit(1);
}
