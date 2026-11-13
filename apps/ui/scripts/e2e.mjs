// End-to-end smoke test for the thread UI against a real daemon.
//
// Starts `h4rnd serve` with the fake harness on ephemeral ports, serves the
// built UI with `vite preview`, then drives a real browser: create a thread,
// watch streamed text, settle it, reload, and confirm the state comes back.

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
  const children = [];
  let browser = null;
  let page = null;
  let daemonLog = "";

  try {
    run("git", ["init", "--quiet", workspace]);

    run(cargoBin, ["build", "--quiet", "-p", "h4rn-daemon", "--bin", "h4rnd"], {
      cwd: repoRoot,
    });

    const [daemonPort, uiPort] = await Promise.all([freePort(), freePort()]);
    const token = run(daemonBin, ["--data-dir", dataDir, "token"]).trim();

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

    const url =
      `http://127.0.0.1:${uiPort}/?token=${encodeURIComponent(token)}` +
      `&daemon=${encodeURIComponent(`ws://127.0.0.1:${daemonPort}/ws`)}`;
    await page.goto(url);

    await page.getByRole("button", { name: "New thread" }).first().click();
    await page.locator("#new-thread-workspace").fill(workspace);
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

    console.log("UI e2e passed: thread streamed, settled, and restored after reload.");
  } catch (error) {
    if (page) {
      console.error(`--- page url ---\n${page.url()}`);
      console.error(`--- page text ---\n${await page.locator("body").textContent()}`);
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
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
