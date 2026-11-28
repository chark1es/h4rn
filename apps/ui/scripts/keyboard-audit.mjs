/**
 * Keyboard-only audit of the shared UI surfaces in mock mode.
 *
 * Usage: node scripts/keyboard-audit.mjs [baseURL]
 *
 * Requires `bun run dev` and a Playwright Chromium in ~/.cache/ms-playwright
 * (override with H4RN_CHROMIUM). Every surface is driven with the keyboard
 * only: Tab to the trigger, open it, move with arrows or typeahead, act with
 * Enter or Space, close with Escape, and check that focus lands back on the
 * trigger with a visible ring. One pass per surface; this is an audit, not an
 * e2e suite. Focus-ring screenshots go to H4RN_KB_OUT (default
 * ../screenshots/keyboard/). Prints a Markdown table and exits 1 on a failure.
 */
import { mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const BASE = process.argv[2] ?? "http://127.0.0.1:1420";
const OUT = process.env.H4RN_KB_OUT
  ? `${process.env.H4RN_KB_OUT.replace(/\/$/, "")}/`
  : fileURLToPath(new URL("../screenshots/keyboard/", import.meta.url));
const EXECUTABLE =
  process.env.H4RN_CHROMIUM ??
  `${process.env.HOME}/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell`;
const DESKTOP = { width: 1440, height: 900 };
const PHONE = { width: 390, height: 844 };
const ONLY = process.env.H4RN_KB?.split(",").map((s) => s.trim());

/** @param {string} path @param {string} [theme] */
const url = (path, theme = "light") =>
  `${BASE}${path}${path.includes("?") ? "&" : "?"}mock=1&theme=${theme}`;

/** What has focus, and whether it shows a ring. */
const active = (page) =>
  page.evaluate(() => {
    const e = /** @type {HTMLElement | null} */ (document.activeElement);
    if (!e || e === document.body)
      return { tag: "body", role: "", name: "", ring: false, fv: false };
    const cs = getComputedStyle(e);
    const outline =
      cs.outlineStyle !== "none" &&
      Number.parseFloat(cs.outlineWidth) > 0 &&
      !/rgba\(0, 0, 0, 0\)|transparent/.test(cs.outlineColor);
    const shadow = cs.boxShadow
      .split(/,(?![^(]*\))/)
      .some(
        (layer) => layer.trim() !== "none" && !/rgba\(0, 0, 0, 0\)|0px 0px 0px 0px/.test(layer),
      );
    const labelled = e.getAttribute("aria-labelledby");
    const name = (
      e.getAttribute("aria-label") ??
      (labelled ? document.getElementById(labelled)?.textContent : null) ??
      e.textContent ??
      ""
    )
      .replace(/\s+/g, " ")
      .trim();
    return {
      tag: e.tagName.toLowerCase(),
      role: e.getAttribute("role") ?? "",
      name,
      ring: outline || shadow,
      fv: e.matches(":focus-visible"),
    };
  });

/** The item a composite widget is pointing at (roving focus or active descendant). */
const highlighted = (page) =>
  page.evaluate(() => {
    const e = document.activeElement;
    const id = e?.getAttribute("aria-activedescendant");
    const target =
      (id && document.getElementById(id)) ||
      (e && /menuitem|option|treeitem|tab|radio/.test(e.getAttribute("role") ?? "") ? e : null) ||
      document.querySelector("[data-highlighted]");
    return target ? (target.textContent ?? "").replace(/\s+/g, " ").trim() : "";
  });

/** Press Tab (or Shift+Tab) until `match` holds for the focused element. */
async function tabTo(page, match, { max = 140, back = false } = {}) {
  for (let i = 0; i < max; i++) {
    await page.keyboard.press(back ? "Shift+Tab" : "Tab");
    const a = await active(page);
    if (match(a)) {
      // Rings fade in over --duration-micro; read the settled state.
      await page.waitForTimeout(160);
      return { ...(await active(page)), presses: i + 1 };
    }
  }
  throw new Error(`could not Tab to the target in ${max} presses`);
}

const byName = (re) => (a) => re.test(a.name);

/** Mark the focused element so we can tell whether focus comes back to it. */
const markOrigin = (page) =>
  page.evaluate(() => {
    for (const e of document.querySelectorAll("[data-kb-origin]"))
      e.removeAttribute("data-kb-origin");
    document.activeElement?.setAttribute("data-kb-origin", "");
  });
const focusReturned = (page) =>
  page.evaluate(() => document.activeElement?.hasAttribute("data-kb-origin") ?? false);

const popupOpen = (page, role) =>
  page
    .locator(`[role="${role}"]:visible`)
    .count()
    .then((n) => n > 0);

/** Tab a few times inside an open dialog and check focus never leaves it. */
async function trapped(page, presses = 20) {
  for (let i = 0; i < presses; i++) {
    await page.keyboard.press("Tab");
    const inside = await page.evaluate(
      () => !!document.activeElement?.closest('[role="dialog"], [role="alertdialog"]'),
    );
    if (!inside) return false;
  }
  return true;
}

/** @type {Array<{ surface: string; steps: string; result: "Pass" | "Fail"; notes: string }>} */
const results = [];
const browser = await chromium.launch({ executablePath: EXECUTABLE, args: ["--no-sandbox"] });
await mkdir(OUT, { recursive: true });

/**
 * @param {string} surface
 * @param {string} steps
 * @param {{ path: string; wait?: string; theme?: string; touch?: boolean; phone?: boolean; reducedMotion?: boolean }} at
 * @param {(page: import("playwright-core").Page, notes: string[]) => Promise<boolean | undefined>} body
 */
async function check(surface, steps, at, body) {
  if (ONLY && !ONLY.some((o) => surface.toLowerCase().includes(o.toLowerCase()))) return;
  const context = await browser.newContext({
    viewport: at.phone ? PHONE : DESKTOP,
    deviceScaleFactor: 1,
    reducedMotion: at.reducedMotion ? "reduce" : "no-preference",
    colorScheme: /** @type {"light" | "dark"} */ (at.theme ?? "light"),
    hasTouch: at.touch ?? false,
  });
  const page = await context.newPage();
  /** @type {string[]} */
  const notes = [];
  let ok = false;
  try {
    await page.goto(url(at.path, at.theme), { waitUntil: "domcontentloaded" });
    await page
      .locator(at.wait ?? "main")
      .first()
      .waitFor({ timeout: 15_000 });
    await page.waitForTimeout(900);
    ok = (await body(page, notes)) !== false;
  } catch (error) {
    notes.push(error instanceof Error ? error.message.split("\n")[0] : String(error));
  } finally {
    await context.close();
  }
  results.push({ surface, steps, result: ok ? "Pass" : "Fail", notes: notes.join("; ") });
  console.error(
    `${ok ? "ok  " : "FAIL"} ${surface}${notes.length ? ` (${notes.join("; ")})` : ""}`,
  );
}

/**
 * Dialog opened from a focused trigger: opens, focus starts inside, Tab is
 * trapped, Escape closes, focus returns to the trigger.
 */
async function dialogFlow(page, notes, open, { returnTo = true } = {}) {
  await markOrigin(page);
  await open();
  const dialog = page.locator('[role="dialog"]:visible, [role="alertdialog"]:visible').first();
  await dialog.waitFor({ timeout: 5_000 });
  await page.waitForTimeout(300);
  const first = await active(page);
  const inside = await page.evaluate(
    () => !!document.activeElement?.closest('[role="dialog"], [role="alertdialog"]'),
  );
  if (!inside) notes.push("focus did not move into the dialog");
  else notes.push(`starts on ${first.role || first.tag} "${first.name.slice(0, 30)}"`);
  if (!first.ring) notes.push("no ring on the first field");
  const trap = await trapped(page);
  if (!trap) notes.push("Tab escaped the dialog");
  await page.keyboard.press("Escape");
  await dialog
    .waitFor({ state: "hidden", timeout: 3_000 })
    .catch(() => notes.push("Esc did not close"));
  await page.waitForTimeout(250);
  const back = await focusReturned(page);
  if (returnTo && !back) {
    const now = await active(page);
    notes.push(`focus went to ${now.role || now.tag} "${now.name.slice(0, 30)}"`);
  }
  return inside && trap && (!returnTo || back);
}

/** Menu opened from a focused trigger with Enter (or `openKey`). */
async function menuFlow(page, notes, { openKey = "Enter", typeahead, role = "menu", act } = {}) {
  await markOrigin(page);
  await page.keyboard.press(openKey);
  await page.locator(`[role="${role}"]:visible`).first().waitFor({ timeout: 5_000 });
  await page.waitForTimeout(200);
  const start = await highlighted(page);
  await page.keyboard.press("ArrowDown");
  const next = await highlighted(page);
  const moved = next !== "" && next !== start;
  if (!moved) notes.push(`ArrowDown did not move (${start} → ${next})`);
  if (typeahead) {
    await page.keyboard.press(typeahead.key);
    await page.waitForTimeout(150);
    const hit = await highlighted(page);
    if (!typeahead.expect.test(hit)) notes.push(`typeahead "${typeahead.key}" landed on "${hit}"`);
    else notes.push(`typeahead "${typeahead.key}" → ${hit.slice(0, 24)}`);
  }
  if (act) return act();
  await page.keyboard.press("Escape");
  await page.waitForTimeout(250);
  const closed = !(await popupOpen(page, role));
  if (!closed) notes.push("Esc did not close");
  const back = await focusReturned(page);
  if (!back) {
    const now = await active(page);
    notes.push(`focus went to ${now.role || now.tag} "${now.name.slice(0, 30)}"`);
  }
  const a = await active(page);
  if (!a.ring) notes.push("trigger has no ring after Esc");
  return moved && closed && back;
}

/** Open the thread actions menu from the header and pick an item by typeahead and Enter. */
async function viaThreadMenu(page, letter, item) {
  await tabTo(page, byName(/^Thread actions$/));
  await markOrigin(page);
  await page.keyboard.press("Enter");
  await page.getByRole("menu").first().waitFor();
  for (let i = 0; i < 14; i++) {
    if (item.test(await highlighted(page))) break;
    await page.keyboard.press(letter);
    await page.waitForTimeout(120);
  }
  if (!item.test(await highlighted(page))) throw new Error(`typeahead could not reach ${item}`);
  await page.keyboard.press("Enter");
}

const THREAD = { path: "/t/thr_a", wait: '[aria-label="Thread stream"]' };

// ---------------------------------------------------------------- dialogs

await check(
  "New thread dialog",
  "Tab to New thread, Enter, Tab ×20, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^New thread$/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

// shortcuts.open is bound with inInputs: false, so it opens from a button, not the composer.
await check(
  "Shortcuts dialog",
  "Tab to Right panel, Ctrl+/, Tab ×20, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    return dialogFlow(page, notes, () => page.keyboard.press("ControlOrMeta+/"));
  },
);

await check("Rename dialog", "Thread actions, R…, Enter, Esc", THREAD, async (page, notes) => {
  let origin = false;
  const ok = await dialogFlow(
    page,
    notes,
    async () => {
      await viaThreadMenu(page, "r", /^Rename/);
      origin = true;
    },
    { returnTo: false },
  );
  const a = await active(page);
  const back = /^Thread actions$/.test(a.name);
  if (!back) notes.push(`focus after close: ${a.role || a.tag} "${a.name.slice(0, 30)}"`);
  return ok && origin && back;
});

await check("Git actions dialog", "Thread actions, C…, Enter, Esc", THREAD, async (page, notes) => {
  const ok = await dialogFlow(page, notes, () => viaThreadMenu(page, "c", /^Commit/), {
    returnTo: false,
  });
  const a = await active(page);
  const back = /^Thread actions$/.test(a.name);
  if (!back) notes.push(`focus after close: ${a.role || a.tag} "${a.name.slice(0, 30)}"`);
  return ok && back;
});

await check("Guest share dialog", "Thread actions, S…, Enter, Esc", THREAD, async (page, notes) => {
  const ok = await dialogFlow(page, notes, () => viaThreadMenu(page, "s", /^Share with guest/), {
    returnTo: false,
  });
  const a = await active(page);
  const back = /^Thread actions$/.test(a.name);
  if (!back) notes.push(`focus after close: ${a.role || a.tag} "${a.name.slice(0, 30)}"`);
  return ok && back;
});

await check(
  "Delegate dialog",
  "Right panel, Subagents tab, Tab to Delegate, Enter, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    await page.keyboard.press("Enter");
    await page.getByRole("complementary", { name: "Thread panel" }).waitFor();
    await tabTo(page, (a) => a.role === "tab" && /Changes|Files|Preview|Subagents/.test(a.name));
    for (let i = 0; i < 4; i++) {
      if (/Subagents/.test((await active(page)).name)) break;
      await page.keyboard.press("ArrowRight");
    }
    await page.keyboard.press("Enter");
    await tabTo(page, byName(/^Delegate/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Add project dialog",
  "Tab to Add project, Enter, Tab ×20, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Add project$/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Add MCP server dialog",
  "MCP page, Tab to Add MCP server, Enter, Esc",
  { path: "/library" },
  async (page, notes) => {
    await tabTo(page, byName(/^Add MCP server$/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Edit MCP server dialog",
  "MCP page, Tab to Edit, Enter, Esc",
  { path: "/library" },
  async (page, notes) => {
    await tabTo(page, byName(/^Edit$/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Skill import dialog",
  "Skills page, Tab to Find existing, Enter, Esc",
  { path: "/library/skills" },
  async (page, notes) => {
    await tabTo(page, byName(/^Find existing/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Skill add dialog",
  "Skills page, Tab to Add skill, Enter, Esc",
  { path: "/library/skills" },
  async (page, notes) => {
    await tabTo(page, byName(/^(Add|New) skill/));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Confirm dialog (remove forge account)",
  "Forges, Tab to Remove, Enter, Tab ×20, Esc",
  { path: "/settings/forges" },
  async (page, notes) => {
    await tabTo(page, byName(/^Remove /));
    return dialogFlow(page, notes, () => page.keyboard.press("Enter"));
  },
);

await check(
  "Confirm dialog (revoke device)",
  "Hosts, Tab to Revoke, Enter, Tab ×20, Esc; again, Tab to Revoke, Enter",
  { path: "/settings/hosts" },
  async (page, notes) => {
    await tabTo(page, byName(/^Revoke /));
    const flow = await dialogFlow(page, notes, () => page.keyboard.press("Enter"));
    await page.keyboard.press("Enter");
    const dialog = page.locator('[role="alertdialog"]:visible').first();
    await dialog.waitFor({ timeout: 5_000 });
    await tabTo(page, byName(/^Revoke$/), { max: 4 });
    await page.keyboard.press("Enter");
    await dialog.waitFor({ state: "hidden", timeout: 3_000 });
    await page.waitForTimeout(400);
    const after = await active(page);
    const kept = after.tag !== "body";
    notes.push(`after revoking, focus on ${after.role || after.tag} "${after.name.slice(0, 30)}"`);
    return flow && kept;
  },
);

// ---------------------------------------------------------------- menus

await check(
  "Thread actions menu",
  "Tab to Thread actions, Enter, ArrowDown, typeahead S, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Thread actions$/));
    return menuFlow(page, notes, { typeahead: { key: "s", expect: /^S/ } });
  },
);

await check(
  "Row actions menu (Shift+F10)",
  "Tab to a thread row, Shift+F10, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, (a) => a.role === "option");
    return menuFlow(page, notes, { openKey: "Shift+F10" });
  },
);

await check(
  "Row actions menu (button)",
  "Tab to the row's Actions button, Enter, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Actions for /));
    return menuFlow(page, notes);
  },
);

await check(
  "Hosts menu",
  "Tab to the hosts button, Enter, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/hosts$/));
    return menuFlow(page, notes);
  },
);

await check(
  "Project sort menu",
  "Tab to Sort projects, Enter, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Sort projects/));
    return menuFlow(page, notes);
  },
);

await check(
  "Project actions menu",
  "Tab to <project> actions, Enter, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/ actions$/));
    return menuFlow(page, notes);
  },
);

await check(
  "Turn picker",
  "Right panel, Tab to Turn, Enter, ArrowDown, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    await page.keyboard.press("Enter");
    await page.getByRole("complementary", { name: "Thread panel" }).waitFor();
    await page.waitForTimeout(500);
    const a = await tabTo(
      page,
      (x) => /^Turn|Diff source|turn/i.test(x.name) && x.tag === "button",
    );
    notes.push(`trigger is ${a.role || a.tag}`);
    const role = a.role === "combobox" ? "listbox" : "menu";
    await page.keyboard.press("Enter");
    await page.locator(`[role="${role}"]:visible`).first().waitFor({ timeout: 5_000 });
    const items = await page
      .locator(
        `[role="${role}"]:visible [role^="menuitem"]:not([aria-disabled="true"]), [role="${role}"]:visible [role="option"]`,
      )
      .count();
    notes.push(`${items} enabled item(s)`);
    await page.keyboard.press("Escape");
    await page.waitForTimeout(250);
    if (items < 2) {
      const back = await active(page);
      if (!/Turn/.test(back.name)) notes.push("focus did not return");
      return /Turn/.test(back.name);
    }
    return menuFlow(page, notes, { role });
  },
);

await check(
  "Composer model chip",
  "Tab to the model chip, Enter, Tab to Model, ArrowDown, Esc ×2",
  THREAD,
  async (page, notes) => {
    const chip = await tabTo(
      page,
      (a) => /claude-sonnet|Harness and model|model/i.test(a.name) && a.tag === "button",
    );
    notes.push(`chip "${chip.name.slice(0, 30)}"`);
    await markOrigin(page);
    await page.keyboard.press("Enter");
    const settings = page.getByRole("dialog", { name: "Thread settings" });
    await settings.waitFor({ timeout: 5_000 });
    const field = await active(page);
    if (field.role !== "combobox") await tabTo(page, (a) => a.role === "combobox", { max: 10 });
    await page.keyboard.press("ArrowDown");
    await page.getByRole("listbox").first().waitFor({ timeout: 3_000 });
    const first = await highlighted(page);
    await page.keyboard.press("ArrowDown");
    const second = await highlighted(page);
    if (!second || first === second) notes.push(`suggestions did not move (${first} → ${second})`);
    await page.keyboard.press("Escape");
    await page.waitForTimeout(150);
    const stillOpen = await settings.isVisible();
    if (!stillOpen) notes.push("first Esc closed the whole popover, not just the suggestions");
    if (stillOpen) await page.keyboard.press("Escape");
    await settings.waitFor({ state: "hidden", timeout: 3_000 });
    await page.waitForTimeout(200);
    const back = await focusReturned(page);
    if (!back) notes.push("focus did not return to the chip");
    return back && second !== "" && first !== second;
  },
);

// ---------------------------------------------------------------- selects

await check(
  "Select (settings)",
  "Tab to Longest wait, Enter, ArrowDown, Enter, check value",
  { path: "/settings/resume" },
  async (page, notes) => {
    const trigger = await tabTo(page, (a) => a.role === "combobox");
    notes.push(`trigger "${trigger.name.slice(0, 30)}"`);
    await markOrigin(page);
    await page.keyboard.press("Enter");
    await page.getByRole("listbox").first().waitFor({ timeout: 3_000 });
    const before = await highlighted(page);
    await page.keyboard.press("ArrowDown");
    const after = await highlighted(page);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(250);
    const closed = !(await popupOpen(page, "listbox"));
    const back = await focusReturned(page);
    const now = await active(page);
    if (!now.name.includes(after)) notes.push(`value "${now.name}" is not "${after}"`);
    if (!back) notes.push("focus did not return to the trigger");
    if (!closed) notes.push("listbox stayed open");
    // Typeahead on the closed trigger.
    await page.keyboard.press("Enter");
    await page.getByRole("listbox").first().waitFor({ timeout: 3_000 });
    await page.keyboard.press("Escape");
    await page.waitForTimeout(200);
    const escBack = await focusReturned(page);
    if (!escBack) notes.push("Esc lost focus");
    return before !== after && closed && back && escBack;
  },
);

for (const reducedMotion of [true, false]) {
  await check(
    `Select sheet on screen (touch${reducedMotion ? ", reduced motion" : ""})`,
    "Phone, touch: tap Interface language twice; listbox inside the viewport; tap an option, then Enter/ArrowDown/Enter",
    { path: "/settings/appearance", touch: true, phone: true, reducedMotion },
    async (page, notes) => {
      const id = await page
        .getByRole("combobox", { name: /Interface language/ })
        .first()
        .evaluate((e) => e.id);
      // Picking a language renames the trigger, so find it by id.
      const trigger = page.locator(`[id="${id}"]`);
      const sheetBox = async () => {
        const listbox = page.locator("[role=listbox][data-open]").first();
        await listbox.waitFor({ timeout: 3_000 });
        await page.waitForTimeout(700);
        return listbox.evaluate((l) => {
          const r = l.getBoundingClientRect();
          return { top: r.top, bottom: r.bottom, sheet: l.getAttribute("data-presentation") };
        });
      };
      const onScreen = (box) => box.top >= 0 && box.bottom <= PHONE.height + 1;
      let ok = true;
      for (const round of [0, 1]) {
        await trigger.tap();
        const box = await sheetBox();
        notes.push(
          `open ${round + 1}: ${box.sheet} at y ${Math.round(box.top)}–${Math.round(box.bottom)}`,
        );
        if (!onScreen(box)) ok = false;
        const option = page.locator("[role=listbox][data-open] [role=option]").nth(1 - round);
        const label = ((await option.textContent()) ?? "").trim();
        await option.tap();
        await page.waitForTimeout(400);
        const value = ((await trigger.textContent()) ?? "").trim();
        if (!label.startsWith(value)) {
          notes.push(`tapped "${label}", trigger shows "${value}"`);
          ok = false;
        }
      }
      await trigger.focus();
      await page.keyboard.press("Enter");
      const box = await sheetBox();
      if (!onScreen(box)) {
        notes.push(`keyboard open at y ${Math.round(box.top)}`);
        ok = false;
      }
      await page.keyboard.press("ArrowDown");
      const target = await highlighted(page);
      await page.keyboard.press("Enter");
      await page.waitForTimeout(400);
      const value = ((await trigger.textContent()) ?? "").trim();
      if (!target.startsWith(value)) {
        notes.push(`keyboard picked "${target}", trigger shows "${value}"`);
        ok = false;
      }
      return ok;
    },
  );
}

await check(
  "SuggestInput (New thread model)",
  "New thread, Tab to Model, ArrowDown ×2, Esc, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^New thread$/));
    await page.keyboard.press("Enter");
    await page.getByRole("dialog").waitFor();
    await tabTo(page, (a) => a.role === "combobox" && a.tag === "input", { max: 12 });
    await page.keyboard.press("ArrowDown");
    await page.getByRole("listbox").first().waitFor({ timeout: 3_000 });
    const a = await highlighted(page);
    await page.keyboard.press("ArrowDown");
    const b = await highlighted(page);
    if (a === b) notes.push("ArrowDown did not move");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(150);
    const dialogOpen = await page.getByRole("dialog").isVisible();
    if (!dialogOpen) notes.push("first Esc closed the dialog instead of the suggestions");
    return a !== b && dialogOpen;
  },
);

// ---------------------------------------------------------------- palette

await check(
  "Command palette",
  "Tab to Search or run, Enter, type, ArrowDown, End, Home, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Search or run/));
    await markOrigin(page);
    await page.keyboard.press("Enter");
    await page.getByRole("dialog").waitFor();
    await page.keyboard.type("settle");
    await page.waitForTimeout(200);
    const first = await highlighted(page);
    await page.keyboard.press("ArrowDown");
    const second = await highlighted(page);
    if (!first || first === second) notes.push(`arrows did not move (${first} → ${second})`);
    await page.keyboard.press("End");
    const last = await highlighted(page);
    await page.keyboard.press("Home");
    const home = await highlighted(page);
    const ends = home === first && !!last;
    notes.push(`End → ${last}, Home → ${home}`);
    if (!ends) notes.push("Home did not return to the first result");
    // As with cmdk: one Escape closes, even with text typed.
    await page.keyboard.press("Escape");
    const closed = await page
      .getByRole("dialog")
      .waitFor({ state: "hidden", timeout: 3_000 })
      .then(() => true)
      .catch(() => false);
    if (!closed) notes.push("one Esc did not close the palette");
    await page.waitForTimeout(200);
    const back = await focusReturned(page);
    if (!back) notes.push("focus did not return to Search or run");
    return !!first && first !== second && ends && closed && back;
  },
);

await check(
  "Command palette (Ctrl+K)",
  "Ctrl+K from the composer, Esc",
  THREAD,
  async (page, notes) => {
    await tabTo(page, (a) => a.tag === "textarea");
    await markOrigin(page);
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByRole("dialog").waitFor();
    await page.keyboard.press("Escape");
    await page.getByRole("dialog").waitFor({ state: "hidden" });
    await page.waitForTimeout(200);
    const back = await focusReturned(page);
    if (!back) notes.push("focus did not return to the composer");
    return back;
  },
);

// ---------------------------------------------------------------- controls

await check(
  "Segmented control (inbox state)",
  "Tab to Working, ArrowRight, check Done",
  THREAD,
  async (page, notes) => {
    const a = await tabTo(page, (x) => x.role === "radio");
    if (!a.ring) notes.push("no ring on the focused segment");
    await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(200);
    const b = await active(page);
    const checked = await page.evaluate(() => document.activeElement?.getAttribute("aria-checked"));
    notes.push(`${a.name} → ${b.name} (checked=${checked})`);
    return b.name !== a.name && checked === "true" && a.ring;
  },
);

await check(
  "Tabs (Thread / IDE)",
  "Tab to Thread tab, ArrowRight, Enter or auto-select",
  THREAD,
  async (page, notes) => {
    const a = await tabTo(page, (x) => x.role === "tab");
    if (!a.ring) notes.push("no ring on the focused tab");
    await page.keyboard.press("ArrowRight");
    const b = await active(page);
    if (a.name === b.name) notes.push("ArrowRight did not move");
    await page.keyboard.press("ArrowLeft");
    const c = await active(page);
    return a.ring && a.name !== b.name && c.name === a.name;
  },
);

await check(
  "Tabs (right panel)",
  "Right panel, Tab to Changes, ArrowRight ×3, Home",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    await page.keyboard.press("Enter");
    await page.getByRole("complementary", { name: "Thread panel" }).waitFor();
    const a = await tabTo(page, (x) => x.role === "tab" && /Changes/.test(x.name));
    for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight");
    const b = await active(page);
    await page.keyboard.press("Home");
    const c = await active(page);
    notes.push(`${a.name} → ${b.name} → Home ${c.name}`);
    return /Subagents/.test(b.name) && /Changes/.test(c.name);
  },
);

await check(
  "Switch",
  "Appearance, Tab to Terminal switch, Space ×2",
  { path: "/settings/appearance" },
  async (page, notes) => {
    const a = await tabTo(page, (x) => x.role === "switch");
    if (!a.ring) notes.push("no ring on the switch");
    const state = () => page.evaluate(() => document.activeElement?.getAttribute("aria-checked"));
    const s0 = await state();
    await page.keyboard.press("Space");
    const s1 = await state();
    await page.keyboard.press("Space");
    const s2 = await state();
    notes.push(`${s0} → ${s1} → ${s2}`);
    return a.ring && s0 !== s1 && s0 === s2;
  },
);

await check(
  "Segmented control (color scheme)",
  "Appearance, Tab to the scheme radios, ArrowRight",
  { path: "/settings/appearance" },
  async (page, notes) => {
    const a = await tabTo(page, (x) => x.role === "radio");
    await page.keyboard.press("ArrowRight");
    const b = await active(page);
    notes.push(`${a.name} → ${b.name}`);
    await page.keyboard.press("ArrowLeft");
    return a.name !== b.name && a.ring;
  },
);

await check(
  "File tree",
  "Right panel, Files tab, Tab into tree, ArrowDown, ArrowRight, ArrowLeft",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    await page.keyboard.press("Enter");
    await page.getByRole("complementary", { name: "Thread panel" }).waitFor();
    await tabTo(page, (x) => x.role === "tab" && /Changes/.test(x.name));
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("Enter");
    await page.getByRole("tree", { name: "Workspace files" }).waitFor({ timeout: 10_000 });
    const a = await tabTo(page, (x) => x.role === "treeitem", { max: 20 });
    if (!a.ring) {
      const labelRing = await page.evaluate(() => {
        const l = document.activeElement?.querySelector('[data-slot="tree-label"]');
        if (!l) return false;
        const cs = getComputedStyle(l);
        return (
          cs.outlineStyle !== "none" ||
          cs.boxShadow !== "none" ||
          cs.backgroundColor !== "rgba(0, 0, 0, 0)"
        );
      });
      if (!labelRing) notes.push("no visible focus on the tree row");
    }
    await page.keyboard.press("ArrowDown");
    const b = await active(page);
    const expanded = () =>
      page.evaluate(() => document.activeElement?.getAttribute("aria-expanded"));
    // Find a folder.
    for (let i = 0; i < 6 && (await expanded()) === null; i++)
      await page.keyboard.press("ArrowDown");
    const e0 = await expanded();
    await page.keyboard.press(e0 === "true" ? "ArrowLeft" : "ArrowRight");
    await page.waitForTimeout(500);
    const e1 = await expanded();
    notes.push(`${a.name} → ${b.name}; folder expanded ${e0} → ${e1}`);
    return a.name !== b.name && e0 !== null && e0 !== e1;
  },
);

await check(
  "Resizer (sidebar)",
  "Tab to Resize sidebar, ArrowRight ×3, check width",
  THREAD,
  async (page, notes) => {
    const a = await tabTo(page, (x) => x.role === "separator");
    const width = () =>
      page.evaluate(
        () => document.querySelector('[aria-label="Threads"]')?.getBoundingClientRect().width ?? 0,
      );
    const value = () => page.evaluate(() => document.activeElement?.getAttribute("aria-valuenow"));
    const w0 = await width();
    const v0 = await value();
    for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight");
    await page.waitForTimeout(150);
    const w1 = await width();
    const v1 = await value();
    const ring = await page.evaluate(() => {
      const e = document.activeElement;
      return e ? getComputedStyle(e, "::after").backgroundColor : "";
    });
    notes.push(
      `${a.name}: width ${Math.round(w0)} → ${Math.round(w1)}, valuenow ${v0} → ${v1}, line ${ring}`,
    );
    return w1 !== w0 || v0 !== v1;
  },
);

// ---------------------------------------------------------------- settings forms

await check(
  "Settings form (Forges)",
  "Tab through Forge select, Base URL, Token, Add account",
  { path: "/settings/forges" },
  async (page, notes) => {
    const select = await tabTo(page, (a) => a.role === "combobox");
    const url = await tabTo(page, (a) => a.tag === "input", { max: 3 });
    const token = await tabTo(page, (a) => a.tag === "input", { max: 3 });
    const missing = [select, url, token].filter((x) => !x.ring).length;
    if (missing) notes.push(`${missing} field(s) without a ring`);
    notes.push(
      `${select.name.slice(0, 20)} → ${url.name.slice(0, 20) || "Base URL"} → ${token.name.slice(0, 20) || "Token"}`,
    );
    return missing === 0;
  },
);

await check(
  "Settings nav",
  "Tab into the settings nav, Enter on Appearance",
  { path: "/settings/general" },
  async (page, notes) => {
    const a = await tabTo(page, byName(/^Appearance/));
    if (!a.ring) notes.push("no ring on the nav row");
    await page.keyboard.press("Enter");
    await page.waitForURL(/appearance/, { timeout: 3_000 });
    return a.ring;
  },
);

await check(
  "Toaster",
  "Tab past the last control into the toast, then Tab again",
  { path: "/settings/appearance" },
  async (page, notes) => {
    await tabTo(page, byName(/^Dismiss$/));
    await page.keyboard.press("Tab");
    const after = await active(page);
    const left = await page.evaluate(
      () => !document.activeElement?.closest("[data-sonner-toaster]"),
    );
    notes.push(
      `after Dismiss, Tab went to ${after.role || after.tag} "${after.name.slice(0, 30)}" (focus-visible ${after.fv})`,
    );
    await page.keyboard.press("Tab");
    const again = await active(page);
    const looped = /Dismiss|Couldn't read/.test(again.name);
    if (looped)
      notes.push("Tab loops back into the toast (Sonner refocuses the last element on blur)");
    if (!left || !after.fv) return false;
    return !looped;
  },
);

// ---------------------------------------------------------------- tooltips

await check(
  "Tooltip delay",
  "Hover Right panel, time until visible; move to Toggle terminal",
  THREAD,
  async (page, notes) => {
    const trigger = page.getByRole("button", { name: "Right panel" });
    const box = await trigger.boundingBox();
    if (!box) throw new Error("no Right panel button");
    const t0 = Date.now();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.getByRole("tooltip").waitFor({ timeout: 3_000 });
    const delay = Date.now() - t0;
    const next = page.getByRole("button", { name: "Toggle terminal" });
    const nb = await next.boundingBox();
    if (!nb) throw new Error("no Toggle terminal button");
    const t1 = Date.now();
    await page.mouse.move(nb.x + nb.width / 2, nb.y + nb.height / 2);
    await page
      .getByRole("tooltip")
      .filter({ hasText: /terminal/i })
      .waitFor({ timeout: 2_000 });
    const instant = Date.now() - t1;
    notes.push(`first ${delay} ms, next ${instant} ms (pointer used only to time hover)`);
    return delay >= 200 && delay <= 700 && instant < 200;
  },
);

await check(
  "Tooltip on keyboard focus",
  "Tab to Right panel, wait",
  THREAD,
  async (page, notes) => {
    await tabTo(page, byName(/^Right panel$/));
    const shown = await page
      .getByRole("tooltip")
      .waitFor({ timeout: 1_500 })
      .then(() => true)
      .catch(() => false);
    if (!shown) notes.push("no tooltip on focus");
    return shown;
  },
);

await check(
  "No tooltip on touch",
  "hasTouch context, tap Right panel twice",
  { ...THREAD, touch: true },
  async (page, notes) => {
    const trigger = page.getByRole("button", { name: "Right panel" });
    await trigger.tap();
    await page.waitForTimeout(700);
    const shown = await page.getByRole("tooltip").count();
    await trigger.tap();
    if (shown) notes.push("tooltip appeared on tap");
    return shown === 0;
  },
);

// ---------------------------------------------------------------- focus ring shots

for (const theme of ["light", "dark"]) {
  await check(
    `Focus ring screenshot (${theme})`,
    "Tab to Working segment, Stop button, Right panel; screenshot each",
    { ...THREAD, theme },
    async (page, notes) => {
      const shots = [
        ["segment", (a) => a.role === "radio"],
        ["stop", byName(/^Stop$/)],
        ["icon-button", byName(/^Right panel$/)],
      ];
      let ok = true;
      for (const [name, match] of shots) {
        const a = await tabTo(page, match);
        ok &&= a.ring;
        const box = await page.evaluate(() => {
          const r = document.activeElement?.getBoundingClientRect();
          return r ? { x: r.x, y: r.y, width: r.width, height: r.height } : null;
        });
        if (box) {
          await page.screenshot({
            path: `${OUT}focus-${name}-${theme}.png`,
            clip: {
              x: Math.max(0, box.x - 16),
              y: Math.max(0, box.y - 12),
              width: box.width + 32,
              height: box.height + 24,
            },
          });
        }
        notes.push(`${name} ring ${a.ring ? "yes" : "no"}`);
      }
      return ok;
    },
  );
}

await browser.close();

console.log("| Surface | Steps | Result | Notes |\n|---|---|---|---|");
for (const r of results) {
  console.log(`| ${r.surface} | ${r.steps} | ${r.result} | ${r.notes.replaceAll("|", "\\|")} |`);
}
const failed = results.filter((r) => r.result === "Fail").length;
console.error(`${results.length - failed}/${results.length} surfaces passed`);
process.exit(failed ? 1 : 0);
