// End-to-end smoke test for the thread UI against a real daemon.
//
// Starts `h4rnd serve` with the fake harness on ephemeral ports, serves the
// built UI with `vite preview`, then drives a real browser: create a thread,
// watch streamed text, settle it, reload, delegate and cancel a subagent, and
// check usage, limits, MCP and skills wiring against a real daemon.

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
    run("git", ["init", "--quiet", "-b", "main", workspace]);
    run("git", ["-C", workspace, "config", "user.email", "e2e@h4rn.dev"]);
    run("git", ["-C", workspace, "config", "user.name", "E2E"]);
    writeFileSync(join(workspace, "README.md"), "# e2e workspace\n");
    run("git", ["-C", workspace, "add", "README.md"]);
    run("git", ["-C", workspace, "commit", "--quiet", "-m", "init"]);
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
        // be asserted and acted on before it fires, and keep an echo turn in
        // flight long enough to open and cancel a delegated child. This is a
        // realistic harness delay (longer than the client's 10 s request
        // timeout); Resume acknowledges immediately and reports the resumed
        // turn through events instead of waiting for its first send.
        env: {
          ...process.env,
          H4RN_FAKE_HARNESS: "1",
          H4RN_FAKE_LIMIT_MS: "120000",
          H4RN_FAKE_HARNESS_DELAY_MS: "12000",
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
    await page.locator("#new-thread-task").fill("[[write:notes.txt]]hello from the fake harness");
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent?.trim() === "Start",
      );
      return button !== undefined && !button.disabled;
    });
    await page.getByRole("button", { name: "Start", exact: true }).click();

    // The fake harness streams `echo: <prompt>` through the daemon and writes
    // the file named by the write directive.
    await page.getByText(/wrote notes\.txt/).waitFor({ timeout: 20_000 });

    const header = page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) });
    await header.getByText(/^Done/).waitFor({ timeout: 20_000 });

    // The thread header names the checkout's branch.
    await header.getByText("main", { exact: true }).waitFor({ timeout: 10_000 });

    // The completed turn carries its change set, and the stream shows a card
    // before the panel is opened.
    const card = page.locator(".q-changes");
    await card.waitFor({ timeout: 10_000 });
    await card.getByText("1 file changed").waitFor({ timeout: 10_000 });
    await card.getByText("notes.txt").waitFor({ timeout: 10_000 });
    await card.getByRole("button", { name: "Review" }).click();

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

    // The panel lists the turn's file and renders the diff against the agent's
    // starting point.
    await panel.getByText("notes.txt").first().waitFor({ timeout: 10_000 });
    await panel.getByText("hello from the fake harness").first().waitFor({ timeout: 10_000 });

    // Per-file revert goes back to the pre-turn state, which for a new file
    // means deleting it.
    await panel.getByRole("button", { name: "Revert notes.txt" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Revert", exact: true }).click();
    await page.getByText("Reverted 1 file.").waitFor({ timeout: 20_000 });
    for (let attempt = 0; attempt < 50 && existsSync(join(workspace, "notes.txt")); attempt += 1) {
      await new Promise((resolveWait) => setTimeout(resolveWait, 100));
    }
    if (existsSync(join(workspace, "notes.txt"))) {
      throw new Error("Revert did not remove notes.txt from the workspace");
    }

    await page.getByRole("button", { name: "Thread actions" }).click();
    await page.getByRole("menuitem", { name: "Settle now" }).click();
    await header.getByText("Settled", { exact: true }).waitFor({ timeout: 20_000 });

    await page.reload();
    await page.getByText(/wrote notes\.txt/).waitFor({ timeout: 20_000 });
    await page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) })
      .getByText("Settled", { exact: true })
      .waitFor({ timeout: 20_000 });

    // ------------------------------------------------------------------
    // Delegation: spawn a real child turn, see it in the tree, open its
    // transcript, then cancel it while the fake harness is still working.
    // ------------------------------------------------------------------
    if ((await panelToggle.getAttribute("aria-pressed")) !== "true") await panelToggle.click();
    await panel.getByRole("tab", { name: /^Subagents/ }).click();
    await panel.getByText("Nothing delegated").waitFor({ timeout: 10_000 });
    await panel.getByRole("button", { name: "Delegate", exact: true }).first().click();

    const delegate = page.getByRole("dialog");
    await delegate.getByRole("heading", { name: "Delegate a subagent" }).waitFor();
    const childTask = "Reproduce the relay fixture race";
    await delegate.locator("#delegate-task").fill(childTask);
    await delegate.getByRole("button", { name: "Delegate", exact: true }).click();

    const childRow = panel.locator(".q-sub-node").filter({ hasText: childTask });
    await childRow.waitFor({ timeout: 15_000 });
    await childRow.click();
    await panel.getByText(childTask, { exact: true }).first().waitFor();

    await panel.getByRole("button", { name: "Open thread", exact: true }).click();
    await page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) })
      .getByText(childTask, { exact: true })
      .waitFor({ timeout: 15_000 });

    // Back to the parent through the sidebar, then cancel the live child. The
    // parent is settled, so switch the sidebar back to that state first.
    const states = page.getByRole("tablist", { name: "Thread state" });
    await states.getByRole("tab", { name: /^Settled/ }).click();
    await page
      .getByRole("option")
      .filter({ hasText: "hello from the fake harness" })
      .first()
      .click();
    await panel.getByRole("tab", { name: /^Subagents/ }).click();
    await childRow.waitFor({ timeout: 10_000 });
    await childRow.click();
    await panel.getByRole("button", { name: "Cancel subagent", exact: true }).click();
    await panel.getByRole("button", { name: "Cancel subagent", exact: true }).click();
    await childRow.getByText("Cancelled", { exact: false }).waitFor({ timeout: 15_000 });
    await panel.getByRole("button", { name: "Cancel subagent", exact: true }).waitFor({
      state: "detached",
      timeout: 15_000,
    });

    // The delegation check left the sidebar on Settled; the limit flow below
    // asserts the new thread's inbox row, which lives under Working.
    await states.getByRole("tab", { name: /^Working/ }).click();

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
      "UI e2e passed: thread streamed, settled, and restored; delegation spawned and cancelled; usage footer and Usage screen show metered tokens; a scripted limit armed, cancelled and resumed a schedule; MCP server and skill persisted.",
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
