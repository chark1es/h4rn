// End-to-end smoke test for the thread UI against a real daemon.
//
// Starts `h4rnd serve` with the fake harness on ephemeral ports, serves the
// built UI with `vite preview`, then drives a real browser: create a thread,
// watch streamed text, settle it, reload, and confirm the state comes back.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const uiRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(uiRoot, "..", "..");
const daemonBin = process.env.H4RN_DAEMON_BIN ?? join(repoRoot, "target", "debug", "h4rnd");
const cargoBin =
  process.env.CARGO ??
  (existsSync(join(homedir(), ".cargo", "bin", "cargo"))
    ? join(homedir(), ".cargo", "bin", "cargo")
    : "cargo");

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "string" || address === null) {
        server.close();
        reject(new Error("could not allocate a port"));
        return;
      }
      const port = address.port;
      server.close(() => resolvePort(port));
    });
  });
}

async function waitForHttp(url, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((resolveWait) => setTimeout(resolveWait, 100));
  }
  throw new Error(`timed out waiting for ${url}`);
}

/** Playwright-core may expect a build older than the one installed. */
function findChromium() {
  if (process.env.H4RN_CHROMIUM) return process.env.H4RN_CHROMIUM;
  const browsers = join(homedir(), ".cache", "ms-playwright");
  if (existsSync(browsers)) {
    const builds = readdirSync(browsers).sort().reverse();
    // Prefer the headless shell: it has fewer system library dependencies than
    // the full Chrome for Testing build.
    for (const build of builds) {
      if (build.startsWith("chromium_headless_shell-")) {
        const candidate = join(
          browsers,
          build,
          "chrome-headless-shell-linux64",
          "chrome-headless-shell",
        );
        if (existsSync(candidate)) return candidate;
      }
    }
    for (const build of builds) {
      if (!build.startsWith("chromium-")) continue;
      for (const layout of ["chrome-linux64", "chrome-linux"]) {
        const candidate = join(browsers, build, layout, "chrome");
        if (existsSync(candidate)) return candidate;
      }
    }
  }
  return chromium.executablePath();
}

function run(command, args, options) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  if (result.error || result.status !== 0) {
    throw new Error(
      `${command} ${args.join(" ")} failed (${result.status ?? result.error?.message})\n${
        result.stderr ?? ""
      }`,
    );
  }
  return result.stdout ?? "";
}

async function main() {
  const dataDir = mkdtempSync(join(tmpdir(), "h4rn-e2e-data-"));
  const workspace = mkdtempSync(join(tmpdir(), "h4rn-e2e-work-"));
  const homeDir = mkdtempSync(join(tmpdir(), "h4rn-e2e-home-"));
  const skillDir = mkdtempSync(join(tmpdir(), "h4rn-e2e-skill-"));
  const children = [];
  let browser = null;
  let page = null;
  let daemonLog = "";

  try {
    run("git", ["init", "--quiet", workspace]);
    mkdirSync(join(homeDir, ".config"), { recursive: true });
    writeFileSync(
      join(skillDir, "SKILL.md"),
      "---\nname: e2e-skill\ndescription: Proves the Skills screen installs a local folder\n---\n\nWhen this skill is invoked, reply with exactly: e2e.\n",
    );

    run(cargoBin, ["build", "--quiet", "-p", "h4rn-daemon", "--bin", "h4rnd"], {
      cwd: repoRoot,
    });
    run(cargoBin, ["build", "--quiet", "-p", "h4rn-mcp", "--bin", "mcp-fixture"], {
      cwd: repoRoot,
    });
    const fixtureBin = join(repoRoot, "target", "debug", "mcp-fixture");

    const [daemonPort, uiPort] = await Promise.all([freePort(), freePort()]);
    const token = run(daemonBin, ["--data-dir", dataDir, "token"]).trim();

    const daemon = spawn(
      daemonBin,
      ["--data-dir", dataDir, "--bind", `127.0.0.1:${daemonPort}`, "serve"],
      {
        // Keep a scripted limit pending long enough for the countdown card to
        // be asserted and acted on before it fires.
        env: {
          ...process.env,
          H4RN_FAKE_HARNESS: "1",
          H4RN_FAKE_LIMIT_MS: "120000",
          HOME: homeDir,
          XDG_CONFIG_HOME: join(homeDir, ".config"),
          XDG_DATA_HOME: join(homeDir, ".local", "share"),
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    children.push(daemon);
    daemon.stdout.on("data", (chunk) => {
      daemonLog += chunk.toString();
    });
    daemon.stderr.on("data", (chunk) => {
      daemonLog += chunk.toString();
    });
    await waitForHttp(`http://127.0.0.1:${daemonPort}/healthz`);

    const vite = spawn(
      join(uiRoot, "node_modules", ".bin", "vite"),
      ["preview", "--host", "127.0.0.1", "--port", String(uiPort), "--strictPort"],
      { cwd: uiRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(vite);
    vite.stderr.on("data", (chunk) => {
      daemonLog += chunk.toString();
    });
    await waitForHttp(`http://127.0.0.1:${uiPort}/`);

    const executablePath = findChromium();
    browser = await chromium.launch({ executablePath, headless: true });
    page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    page.on("pageerror", (error) => {
      console.error(`--- page error ---\n${error.stack ?? error.message}`);
    });
    page.on("console", (message) => {
      if (message.type() === "error") console.error(`--- console error ---\n${message.text()}`);
    });

    const url =
      `http://127.0.0.1:${uiPort}/?token=${encodeURIComponent(token)}` +
      `&daemon=${encodeURIComponent(`ws://127.0.0.1:${daemonPort}/ws`)}`;
    await page.goto(url);

    await page.getByRole("button", { name: "New thread" }).first().click();
    await page.locator("#new-thread-workspace").fill(workspace);
    // The fake adapter registers as Claude Code; pick it explicitly so a real
    // Codex/OpenCode install on the test machine cannot take the turn.
    await page.locator("#new-thread-harness").selectOption("claude_code");
    await page.locator("#new-thread-task").fill("Reply with exactly: pong");
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent?.trim() === "Start",
      );
      return button !== undefined && !button.disabled;
    });
    await page.getByRole("button", { name: "Start", exact: true }).click();

    // The fake harness streams `echo: <prompt>` through the daemon.
    await page.getByText(/echo: Reply with exactly: pong/).waitFor({ timeout: 20_000 });

    const header = page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) });
    await header.getByText(/^Done/).waitFor({ timeout: 20_000 });

    // Layout v2: the view cluster switches the main area to the IDE for this
    // thread's project, and the right panel follows the selected thread.
    await page.getByRole("tab", { name: "IDE" }).click();
    await page.locator('[aria-label="IDE"]').waitFor({ timeout: 10_000 });
    await page
      .getByRole("heading", { name: workspace.split("/").filter(Boolean).at(-1) })
      .waitFor({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Thread" }).click();
    // Entering the IDE reveals the file tree, so the panel is usually already
    // open when we return to the thread; only toggle it when it is closed.
    const panelToggle = page.getByRole("button", { name: "Right panel" });
    if ((await panelToggle.getAttribute("aria-pressed")) !== "true") await panelToggle.click();
    const panel = page.getByRole("complementary", { name: "Thread panel" });
    await panel.waitFor({ timeout: 10_000 });
    await panel.getByRole("tab", { name: /Changes/ }).click();
    await panel.getByRole("tab", { name: "vs agent" }).waitFor({ timeout: 10_000 });
    if (
      (await panel.getByRole("tab", { name: "vs agent" }).getAttribute("aria-selected")) !== "true"
    ) {
      throw new Error("Changes tab did not open in the right panel");
    }

    await page.getByRole("button", { name: "Thread actions" }).click();
    await page.getByRole("menuitem", { name: "Settle now" }).click();
    await header.getByText("Settled", { exact: true }).waitFor({ timeout: 20_000 });

    await page.reload();
    await page.getByText(/echo: Reply with exactly: pong/).waitFor({ timeout: 20_000 });
    await page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) })
      .getByText("Settled", { exact: true })
      .waitFor({ timeout: 20_000 });

    // ------------------------------------------------------------------
    // Usage wiring: the footer and the Usage screen read real ledger rows.
    // ------------------------------------------------------------------
    const footer = page.locator(".q-usage-foot");
    await footer.waitFor({ timeout: 10_000 });
    const footerText = (await footer.textContent()) ?? "";
    const footerMatch = footerText.match(/([\d.,]+)\s*([kM]?)\s*tokens/);
    const footerTokens = footerMatch
      ? Number(footerMatch[1]?.replace(/,/g, "")) *
        (footerMatch[2] === "k" ? 1_000 : footerMatch[2] === "M" ? 1_000_000 : 1)
      : 0;
    if (!(footerTokens > 0)) {
      throw new Error(`usage footer did not show metered tokens: ${footerText}`);
    }
    if (!footerText.includes("TTFT")) {
      throw new Error(`usage footer did not show TTFT: ${footerText}`);
    }

    await page.getByRole("button", { name: "Usage" }).first().click();
    await page.getByRole("heading", { name: "Usage" }).waitFor({ timeout: 10_000 });
    await page.getByText("Tokens per day by harness").waitFor({ timeout: 10_000 });
    const tokenStat = page.locator(".q-stats .q-stat").first();
    await tokenStat.waitFor({ timeout: 10_000 });
    const tokenStatText = (await tokenStat.textContent()) ?? "";
    if (tokenStatText.includes("0") && !/\d{2,}/.test(tokenStatText.replace(/\D/g, ""))) {
      throw new Error(`Usage screen showed no metered tokens: ${tokenStatText}`);
    }

    // --------------------------------------------------------------
    // Limits and auto-resume: a scripted FakeHarness limit arms a schedule.
    // --------------------------------------------------------------
    await page.getByRole("button", { name: "New thread" }).first().click();
    await page.locator("#new-thread-workspace").fill(workspace);
    await page.locator("#new-thread-harness").selectOption("claude_code");
    await page.locator("#new-thread-task").fill("Keep going [h4rn-limit]");
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent?.trim() === "Start",
      );
      return button !== undefined && !button.disabled;
    });
    await page.getByRole("button", { name: "Start", exact: true }).click();

    const resumeCard = page.locator(".q-resume").first();
    await resumeCard.waitFor({ timeout: 20_000 });
    await resumeCard.getByText(/until auto-resume/).waitFor({ timeout: 10_000 });
    await resumeCard.getByText(/Resume now/).waitFor({ timeout: 10_000 });
    // The inbox row follows ScheduleChanged without a reload.
    await page
      .getByText(/Resumes \d{1,2}:\d{2}/)
      .first()
      .waitFor({ timeout: 10_000 });

    await resumeCard.getByRole("button", { name: "Cancel" }).click();
    await page.locator(".q-resume").first().waitFor({ state: "detached", timeout: 15_000 });

    // Cancel cleared the schedule; a new limited turn arms a fresh one so the
    // Resume now path can be exercised too.
    const composer = page.getByRole("textbox", { name: /Reply to/ });
    await composer.fill("Try once more [h4rn-limit]");
    await composer.press("Enter");
    await page.locator(".q-resume").first().waitFor({ timeout: 20_000 });

    await page.getByRole("button", { name: "Usage" }).first().click();
    await page.getByRole("heading", { name: "Usage" }).waitFor({ timeout: 10_000 });
    const usageResume = page.locator(".q-resume").first();
    await usageResume.waitFor({ timeout: 20_000 });
    await usageResume.getByText(/until auto-resume/).waitFor({ timeout: 10_000 });
    await usageResume.getByRole("button", { name: "Resume now" }).click();
    await page.locator(".q-resume").first().waitFor({ state: "detached", timeout: 15_000 });


    // Settings → Tools: add a real stdio server, turn it off for one harness,
    // then install a local skill from a folder. Both must survive a reload.
    await page.getByRole("button", { name: "MCP & skills" }).click();
    await page
      .getByRole("button", { name: /^MCP servers/ })
      .first()
      .click();
    const addButton = page.getByRole("button", { name: /^Add( server)?$/ }).first();
    await addButton.waitFor({ timeout: 15_000 });
    await addButton.click();
    const addServer = page.getByRole("dialog");
    await addServer.locator("#mcp-name").fill("e2e-fixture");
    await addServer.locator("#mcp-command").fill(fixtureBin);
    await addServer.getByRole("button", { name: "Add server" }).click();
    await page.getByRole("heading", { name: "e2e-fixture" }).waitFor({ timeout: 20_000 });

    const codexSwitch = page.getByRole("switch", { name: /for Codex/ });
    if ((await codexSwitch.getAttribute("aria-checked")) !== "true") {
      throw new Error("a new MCP server should start enabled for Codex");
    }
    await codexSwitch.click();
    await page
      .locator('button[role="switch"][aria-label*="for Codex"][aria-checked="false"]')
      .waitFor({ timeout: 20_000 });

    await page.getByRole("button", { name: /^Skills/ }).click();
    await page.getByRole("heading", { name: "Skills", exact: true }).waitFor({ timeout: 15_000 });
    await page.getByRole("button", { name: "Add skill" }).first().click();
    const addSkill = page.getByRole("dialog");
    await addSkill.locator("#skill-path").fill(skillDir);
    await addSkill.getByRole("button", { name: "Add skill" }).click();
    await page
      .getByRole("button", { name: /e2e-skill/ })
      .first()
      .waitFor({ timeout: 20_000 });

    await page.reload();

    await page.getByRole("button", { name: "MCP & skills" }).click();
    await page
      .getByRole("button", { name: /^MCP servers/ })
      .first()
      .click();
    await page.getByRole("heading", { name: "e2e-fixture" }).waitFor({ timeout: 20_000 });
    const restoredCodex = page.getByRole("switch", { name: /for Codex/ });
    if ((await restoredCodex.getAttribute("aria-checked")) !== "false") {
      throw new Error("the per-harness MCP switch did not persist");
    }
    await page.getByRole("button", { name: /^Skills/ }).click();
    await page
      .getByRole("button", { name: /e2e-skill/ })
      .first()
      .waitFor({ timeout: 20_000 });

    console.log(
      "UI e2e passed: thread streamed, settled, and restored; usage footer and Usage screen show metered tokens; a scripted limit armed, cancelled and resumed a schedule; MCP server and skill persisted.",
    );
  } catch (error) {
    if (page) {
      console.error(`--- page url ---\n${page.url()}`);
      console.error(
        `--- page text ---\n${await page
          .locator("body")
          .textContent({ timeout: 2_000 })
          .catch(() => "(page unavailable)")}`,
      );
    }
    if (daemonLog) {
      console.error(`--- daemon/ui logs ---\n${daemonLog}`);
    }
    throw error;
  } finally {
    if (browser) await browser.close();
    for (const child of children) {
      child.kill("SIGTERM");
    }
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(workspace, { recursive: true, force: true });
    rmSync(homeDir, { recursive: true, force: true });
    rmSync(skillDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
