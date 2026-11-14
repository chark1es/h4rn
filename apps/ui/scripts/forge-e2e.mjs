// UI end-to-end test for the forge surfaces against a live Gitea.
//
// `scripts/test-forges.sh --ui` owns the container, seeds the repository and
// PR, then runs this script. It starts a daemon in a temp data dir, serves the
// built UI, adds the Gitea account through Settings, opens the PR list and
// detail, posts a comment and hands the PR to the fake agent.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
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
const bunBin = process.env.BUN ?? join(homedir(), ".bun", "bin", "bun");

const forgeUrl = process.env.H4RN_GITEA_URL;
const token = process.env.H4RN_GITEA_TOKEN;
const repo = process.env.H4RN_GITEA_REPO;
const prNumber = Number(process.env.H4RN_GITEA_PR ?? 0);

if (!forgeUrl || !token || !repo || !prNumber) {
  console.error(
    "forge UI e2e needs H4RN_GITEA_URL, H4RN_GITEA_TOKEN, H4RN_GITEA_REPO and H4RN_GITEA_PR",
  );
  process.exit(2);
}

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

function findChromium() {
  if (process.env.H4RN_CHROMIUM) return process.env.H4RN_CHROMIUM;
  const browsers = join(homedir(), ".cache", "ms-playwright");
  if (existsSync(browsers)) {
    const builds = readdirSync(browsers).sort().reverse();
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
  const dataDir = mkdtempSync(join(tmpdir(), "h4rn-forge-e2e-data-"));
  const workspace = mkdtempSync(join(tmpdir(), "h4rn-forge-e2e-work-"));
  const children = [];
  let browser = null;
  let page = null;
  let logs = "";

  try {
    run("git", ["init", "--quiet", "-b", "main", workspace]);
    run("git", ["-C", workspace, "config", "user.email", "e2e@h4rn.dev"]);
    run("git", ["-C", workspace, "config", "user.name", "E2E"]);
    run("git", ["-C", workspace, "commit", "--quiet", "--allow-empty", "-m", "init"]);

    run(cargoBin, ["build", "--quiet", "-p", "h4rn-daemon", "--bin", "h4rnd"], {
      cwd: repoRoot,
    });
    run(bunBin, ["run", "build"], { cwd: uiRoot });

    const [daemonPort, uiPort] = await Promise.all([freePort(), freePort()]);
    const deviceToken = run(daemonBin, ["--data-dir", dataDir, "token"]).trim();

    const daemon = spawn(
      daemonBin,
      ["--data-dir", dataDir, "--bind", `127.0.0.1:${daemonPort}`, "serve"],
      {
        env: { ...process.env, H4RN_FAKE_HARNESS: "1" },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    children.push(daemon);
    daemon.stdout.on("data", (chunk) => {
      logs += chunk.toString();
    });
    daemon.stderr.on("data", (chunk) => {
      logs += chunk.toString();
    });
    await waitForHttp(`http://127.0.0.1:${daemonPort}/healthz`);

    const vite = spawn(
      join(uiRoot, "node_modules", ".bin", "vite"),
      ["preview", "--host", "127.0.0.1", "--port", String(uiPort), "--strictPort"],
      { cwd: uiRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(vite);
    vite.stderr.on("data", (chunk) => {
      logs += chunk.toString();
    });
    await waitForHttp(`http://127.0.0.1:${uiPort}/`);

    browser = await chromium.launch({ executablePath: findChromium(), headless: true });
    page = await browser.newPage({ viewport: { width: 1440, height: 900 } });

    const query = new URLSearchParams({
      token: deviceToken,
      daemon: `ws://127.0.0.1:${daemonPort}/ws`,
      repo,
      forge: process.env.H4RN_GITEA_KIND === "forgejo" ? "forgejo" : "gitea",
      base: forgeUrl.replace(/\/$/, ""),
    });
    await page.goto(`http://127.0.0.1:${uiPort}/?${query.toString()}`);

    // A thread gives the handoff a workspace and harness.
    await page.getByRole("button", { name: "New thread" }).first().click();
    await page.locator("#new-thread-workspace").fill(workspace);
    await page.locator("#new-thread-harness").click();
    await page.getByRole("menuitem", { name: "Claude Code" }).click();
    await page.locator("#new-thread-task").fill("Reply with exactly: pong");
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent?.trim() === "Start",
      );
      return button !== undefined && !button.disabled;
    });
    await page.getByRole("button", { name: "Start", exact: true }).click();
    await page.getByText(/echo: Reply with exactly: pong/).waitFor({ timeout: 20_000 });

    // Add the Gitea account through the real settings surface.
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page
      .locator('nav[aria-label="Settings"]')
      .getByRole("button", { name: "Forges", exact: true })
      .click();
    await page.getByRole("heading", { name: "Forges" }).waitFor({ timeout: 10_000 });
    await page.getByRole("button", { name: /^Forge:/ }).click();
    await page.getByRole("menuitem", { name: "Gitea" }).click();
    await page.getByLabel("Forge base URL").fill(forgeUrl.replace(/\/$/, ""));
    await page.getByLabel("Forge token").fill(token);
    await page.getByRole("button", { name: "Add account" }).click();
    await page
      .locator(".q-tbl")
      .getByText(forgeUrl.replace(/\/$/, ""), { exact: false })
      .waitFor({ timeout: 20_000 });

    // The PR list answers from Gitea.
    await page.getByRole("button", { name: "Pull requests", exact: true }).click();
    const row = page.locator(".q-row").filter({ hasText: `#${prNumber}` });
    await row.waitFor({ timeout: 20_000 });
    await row.click();

    await page.getByRole("heading", { name: /Add the feature file/ }).waitFor({ timeout: 20_000 });
    await page
      .getByText("Seed comment for the h4rn forge smoke test.")
      .waitFor({ timeout: 20_000 });

    // The Commits tab lists the PR's commits oldest first, straight from the
    // forge: the seed script made two on the branch.
    await page.getByRole("tab", { name: /^Commits/ }).click();
    const commitTitles = await page.locator(".q-commit .t").allTextContents();
    if (
      commitTitles.length !== 2 ||
      commitTitles[0] !== "add the feature file" ||
      commitTitles[1] !== "add the second file"
    ) {
      throw new Error(`unexpected Commits tab: ${JSON.stringify(commitTitles)}`);
    }
    const shaLinks = await page.locator(".q-commit .q-commit-sha").count();
    if (shaLinks !== 2) throw new Error(`expected 2 commit links, found ${shaLinks}`);

    // Checks: the seeded status shows, a Gitea status is not "required" (the
    // branch has no protection) and Re-run failed is disabled with the reason.
    await page.getByRole("tab", { name: /^Checks/ }).click();
    await page.getByText("h4rn/smoke").first().waitFor({ timeout: 10_000 });
    const rerun = page.getByRole("button", { name: "Re-run failed" }).first();
    if (!(await rerun.isDisabled())) throw new Error("Re-run failed should be disabled on Gitea");
    const reason = await rerun.locator("xpath=..").getAttribute("title");
    if (!reason?.includes("commit statuses")) {
      throw new Error(`Re-run failed has no reason tooltip: ${reason}`);
    }
    await page.getByRole("tab", { name: /^Conversation/ }).click();

    // Post a general comment through the daemon.
    const body = `UI e2e comment ${Date.now()}`;
    await page.getByLabel(`Comment on #${prNumber}`).fill(body);
    await page.getByRole("button", { name: "Comment", exact: true }).click();
    await page.getByText(body).waitFor({ timeout: 20_000 });

    // Hand the PR to the thread's agent.
    await page.getByRole("button", { name: "Hand PR to agent" }).click();
    await page.getByText(new RegExp(`Handed #${prNumber} to`)).waitFor({ timeout: 20_000 });

    console.log(
      "Forge UI e2e passed: account, PR list, detail, commits, checks, comment and handoff.",
    );
  } catch (error) {
    if (page) {
      console.error(`--- page url ---\n${page.url()}`);
      console.error(`--- page text ---\n${await page.locator("body").textContent()}`);
    }
    if (logs) console.error(`--- daemon/ui logs ---\n${logs}`);
    throw error;
  } finally {
    if (browser) await browser.close();
    for (const child of children) child.kill("SIGTERM");
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(workspace, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
