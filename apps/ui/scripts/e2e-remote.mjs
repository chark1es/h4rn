// End-to-end remote access through the shipped web UI.
//
// Starts Postgres and a real OrbitID in Docker, runs the real `h4rn-connect`
// relay and a fake-harness daemon, then drives a headless browser: sign in with
// a browser redirect through OrbitID, pair this browser with the machine, open a
// thread over the relay, revoke the browser's device in Settings, and finally
// prove the device-code fallback still signs in.
//
// Skips cleanly when Docker is not reachable. Everything this script starts is
// torn down with `docker compose down -v --remove-orphans`.

import { spawn, spawnSync } from "node:child_process";
import { createHmac, randomBytes } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const uiRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(uiRoot, "..", "..");
const daemonBin = process.env.H4RN_DAEMON_BIN ?? join(repoRoot, "target", "debug", "h4rnd");
const connectBin =
  process.env.H4RN_CONNECT_BIN ?? join(repoRoot, "target", "debug", "h4rn-connect");
const cargoBin =
  process.env.CARGO ??
  (existsSync(join(homedir(), ".cargo", "bin", "cargo"))
    ? join(homedir(), ".cargo", "bin", "cargo")
    : "cargo");
const dockerCandidates = [
  process.env.DOCKER,
  "docker",
  "/mnt/c/Program Files/Docker/Docker/resources/bin/docker.exe",
].filter(Boolean);

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

/** Docker Desktop publishes on Windows, which rejects much of the ephemeral range. */
async function publishedPort() {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const port = 20_000 + Math.floor(Math.random() * 10_000);
    const available = await new Promise((resolveAvailable) => {
      const server = createServer();
      server.unref();
      server.once("error", () => resolveAvailable(false));
      server.listen(port, "127.0.0.1", () => {
        server.close(() => resolveAvailable(true));
      });
    });
    if (available) return port;
  }
  throw new Error("no free Docker-publishable port in 20000-29999");
}

async function waitForHttp(url, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);
      if (response.ok) return response;
    } catch {
      // Not listening yet.
    }
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${url}`);
}

function sleep(ms) {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
}

/** Playwright-core may expect a build older than the one installed. */
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

function run(command, args, options = {}) {
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

function findDocker() {
  for (const candidate of dockerCandidates) {
    const result = spawnSync(candidate, ["info"], { encoding: "utf8", timeout: 20_000 });
    if (!result.error && result.status === 0) return candidate;
  }
  return null;
}

async function jsonRequest(url, init = {}) {
  const response = await fetch(url, init);
  const text = await response.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  if (!response.ok) {
    throw new Error(`${init.method ?? "GET"} ${url} failed (${response.status}): ${text}`);
  }
  return { body, headers: response.headers };
}

function base64Url(bytes) {
  return Buffer.from(bytes)
    .toString("base64")
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function secretB64() {
  return base64Url(randomBytes(32));
}

/** Boots OrbitID's test user, session and OIDC app. */
async function bootstrapOrbit(orbitBase, encryptionKey, uiCallback) {
  const secretKey =
    "sk_test_" +
    createHmac("sha256", encryptionKey).update("orbitid/secret-key").digest("base64url");

  await jsonRequest(`${orbitBase}/v1/users`, {
    method: "POST",
    headers: { authorization: `Bearer ${secretKey}`, "content-type": "application/json" },
    body: JSON.stringify({
      email_address: ["ada@h4rn.test"],
      password: "Sup3rSecret!",
      first_name: "Ada",
    }),
  });

  const signIn = await jsonRequest(`${orbitBase}/v1/client/sign_ins`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      identifier: "ada@h4rn.test",
      strategy: "password",
      password: "Sup3rSecret!",
    }),
  });
  const sessionId =
    signIn.body?.client?.last_active_session_id ?? signIn.body?.response?.created_session_id;
  if (!sessionId)
    throw new Error(`OrbitID did not create a session: ${JSON.stringify(signIn.body)}`);

  // The sign-in response already carries the headless session token. The
  // tokens endpoint needs the browser cookie jar, and OrbitID issues two
  // same-named `__client` cookies; the token is the stable handle here.
  const activeSession = (signIn.body?.client?.sessions ?? []).find(
    (session) => session.id === sessionId,
  );
  const sessionJwt = activeSession?.last_active_token?.jwt;
  if (!sessionJwt) throw new Error("OrbitID did not return a session JWT");

  // The browser redirect flow needs an OrbitID browser session. The API
  // sign-in above already created one and set the opaque `__client` cookie;
  // hand that cookie to Playwright so the authorize step runs headlessly.
  const setCookie = signIn.headers.getSetCookie
    ? signIn.headers.getSetCookie().join("\n")
    : (signIn.headers.get("set-cookie") ?? "");
  const clientTokens = [...setCookie.matchAll(/(?:^|[\n,;]\s*)__client=([^;,\n]+)/g)].map(
    (match) => match[1],
  );
  const clientCookie = clientTokens.at(-1);
  if (!clientCookie) throw new Error("OrbitID did not set a __client cookie");

  const app = await jsonRequest(`${orbitBase}/v1/oauth_applications`, {
    method: "POST",
    headers: { authorization: `Bearer ${secretKey}`, "content-type": "application/json" },
    body: JSON.stringify({
      name: "h4rn-ui-e2e",
      // The browser redirect lands on the UI; the mobile flow keeps its scheme.
      redirect_uris: ["h4rn://callback", uiCallback],
      scopes: "openid profile email offline_access",
      public: false,
      consent_screen_enabled: false,
      pkce_required: true,
    }),
  });

  await jsonRequest(`${orbitBase}/v1/instance`, {
    method: "PATCH",
    headers: { authorization: `Bearer ${secretKey}`, "content-type": "application/json" },
    body: JSON.stringify({
      application: { homeUrl: orbitBase },
      oidcProvider: {
        deviceAuthorizationEnabled: true,
        deviceCodeIntervalSeconds: 1,
        deviceCodeLifetimeSeconds: 300,
      },
      session: { tokenLifetimeSeconds: 3600 },
    }),
  });

  return {
    secretKey,
    sessionJwt,
    clientCookie,
    clientId: app.body.client_id,
    clientSecret: app.body.client_secret,
  };
}

async function approveDeviceCode(orbitBase, sessionJwt, userCode) {
  await jsonRequest(`${orbitBase}/v1/client/oauth/device/approve`, {
    method: "POST",
    headers: { authorization: `Bearer ${sessionJwt}`, "content-type": "application/json" },
    body: JSON.stringify({ user_code: userCode, action: "approve" }),
  });
}

/** Runs `h4rnd login`, approves the printed code, and waits for the tokens. */
async function loginDaemon({ dataDir, daemonPort, relayUrl, orbitBase, sessionJwt, log }) {
  const args = [
    "--data-dir",
    dataDir,
    "--bind",
    `127.0.0.1:${daemonPort}`,
    "login",
    "--connect",
    relayUrl,
    "--provider",
    "orbitid",
    "--name",
    "atlas",
  ];
  const child = spawn(daemonBin, args, { stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  let codePromiseResolve;
  const codePromise = new Promise((resolveCode) => {
    codePromiseResolve = resolveCode;
  });
  const collect = (chunk) => {
    const text = chunk.toString();
    output += text;
    log.push(text);
    const match = /Code:\s+([A-Za-z0-9-]+)/.exec(output);
    if (match?.[1]) codePromiseResolve(match[1]);
  };
  child.stdout.on("data", collect);
  child.stderr.on("data", collect);

  try {
    const code = await Promise.race([
      codePromise,
      sleep(30_000).then(() => {
        throw new Error(`h4rnd login did not print a device code:\n${output}`);
      }),
    ]);
    await approveDeviceCode(orbitBase, sessionJwt, code);
    const status = await new Promise((resolveExit, rejectExit) => {
      child.on("error", rejectExit);
      child.on("exit", (exitCode) => resolveExit(exitCode));
      setTimeout(() => rejectExit(new Error(`h4rnd login timed out:\n${output}`)), 60_000);
    });
    if (status !== 0) throw new Error(`h4rnd login failed (${status}):\n${output}`);
  } finally {
    if (child.exitCode === null) child.kill("SIGTERM");
  }
}

async function main() {
  const docker = findDocker();
  if (!docker) {
    console.log("Remote UI e2e skipped: Docker is not reachable.");
    return;
  }

  const project = `h4rn-ui-e2e-${Date.now()}`;
  const tempRoot = mkdtempSync(join(tmpdir(), "h4rn-ui-remote-"));
  const dataDir = join(tempRoot, "data");
  const workspace = join(tempRoot, "workspace");
  const envFile = join(tempRoot, "compose.env");
  const children = [];
  const logs = [];
  let browser = null;
  let page = null;
  let composeUp = false;

  const compose = (args) =>
    run(
      docker,
      [
        "compose",
        "--env-file",
        envFile,
        "-p",
        project,
        "--profile",
        "orbitid",
        "-f",
        join(repoRoot, "deploy", "docker-compose.yml"),
        ...args,
      ],
      { cwd: repoRoot, timeout: 180_000 },
    );

  try {
    if (!existsSync(join(uiRoot, "dist", "index.html"))) {
      throw new Error("apps/ui/dist is missing; run `bun run build` before the remote e2e");
    }

    const [pgPort, orbitPort, relayPort, daemonPort, uiPort] = await Promise.all([
      publishedPort(),
      publishedPort(),
      freePort(),
      freePort(),
      freePort(),
    ]);

    const pgPassword = "h4rn-ui-e2e-password";
    const orbitEncryptionKey = secretB64();
    const orbitBase = `http://127.0.0.1:${orbitPort}`;
    const relayUrl = `http://127.0.0.1:${relayPort}`;
    const uiOrigin = `http://127.0.0.1:${uiPort}`;

    writeFileSync(
      envFile,
      [
        `H4RN_PG_PORT=${pgPort}`,
        "H4RN_PG_USER=h4rn",
        `H4RN_PG_PASSWORD=${pgPassword}`,
        "H4RN_PG_DATABASE=h4rn",
        "H4RN_ORBITID_IMAGE=orbitid:local",
        `H4RN_ORBITID_PORT=${orbitPort}`,
        `ORBITID_BASE_URL=${orbitBase}`,
        `ORBITID_ENCRYPTION_KEY=${orbitEncryptionKey}`,
        // Compose interpolates every service, even ones this profile skips.
        "H4RN_CONNECT_PUBLIC_BASE_URL=http://127.0.0.1:8787",
        `H4RN_CONNECT_KEK=${secretB64()}`,
        `H4RN_CONNECT_SIGNING_KEY=${secretB64()}`,
        "",
      ].join("\n"),
    );

    const image = spawnSync(docker, ["image", "inspect", "orbitid:local"], {
      encoding: "utf8",
      timeout: 20_000,
    });
    if (image.status !== 0) {
      throw new Error(
        "orbitid:local is not built; run scripts/test-remote.sh once or set H4RN_ORBITID_IMAGE",
      );
    }

    console.log(`--- starting Docker services (${project})`);
    compose(["up", "-d", "--wait", "postgres"]);
    compose(["up", "-d", "--wait", "orbitid-postgres", "orbitid"]);
    composeUp = true;
    await waitForHttp(`${orbitBase}/.well-known/openid-configuration`, 120_000);

    console.log("--- bootstrapping OrbitID");
    const orbit = await bootstrapOrbit(orbitBase, orbitEncryptionKey, `${uiOrigin}/auth/callback`);

    console.log("--- building the daemon and relay");
    run(
      cargoBin,
      [
        "build",
        "--quiet",
        "-p",
        "h4rn-daemon",
        "--bin",
        "h4rnd",
        "-p",
        "h4rn-connect",
        "--bin",
        "h4rn-connect",
      ],
      { cwd: repoRoot, timeout: 600_000 },
    );

    const connectEnv = {
      ...process.env,
      H4RN_CONNECT_PUBLIC_BASE_URL: relayUrl,
      H4RN_CONNECT_BIND: `127.0.0.1:${relayPort}`,
      H4RN_CONNECT_DATABASE_URL: `postgres://h4rn:${pgPassword}@127.0.0.1:${pgPort}/h4rn`,
      H4RN_CONNECT_KEK: secretB64(),
      H4RN_CONNECT_SIGNING_KEY: secretB64(),
      H4RN_CONNECT_ALLOW_INSECURE: "1",
      H4RN_CONNECT_CORS_ORIGINS: uiOrigin,
      H4RN_CONNECT_OIDC_PROVIDERS: JSON.stringify([
        {
          id: "orbitid",
          display_name: "OrbitID",
          issuer: orbitBase,
          client_id: orbit.clientId,
          client_secret: orbit.clientSecret,
          scopes: ["openid", "profile", "email", "offline_access"],
          redirect_uris: ["h4rn://callback", `${uiOrigin}/auth/callback`],
          device_grant: true,
        },
      ]),
      H4RN_CONNECT_RATE_AUTH_PER_MINUTE: "1000",
      H4RN_CONNECT_RATE_PER_IP_PER_MINUTE: "5000",
      RUST_LOG: "h4rn_connect=debug,tower_http=debug",
    };
    console.log("--- starting h4rn-connect");
    const connect = spawn(connectBin, [], { env: connectEnv, stdio: ["ignore", "pipe", "pipe"] });
    children.push(connect);
    connect.stdout.on("data", (chunk) => logs.push(`connect: ${chunk}`));
    connect.stderr.on("data", (chunk) => logs.push(`connect: ${chunk}`));
    await waitForHttp(`${relayUrl}/healthz`, 60_000);

    console.log("--- signing the daemon in");
    run("git", ["init", "--quiet", workspace]);
    await loginDaemon({
      dataDir,
      daemonPort,
      relayUrl,
      orbitBase,
      sessionJwt: orbit.sessionJwt,
      log: logs,
    });

    console.log("--- starting h4rnd");
    const daemon = spawn(
      daemonBin,
      ["--data-dir", dataDir, "--bind", `127.0.0.1:${daemonPort}`, "serve"],
      {
        env: {
          ...process.env,
          H4RN_FAKE_HARNESS: "1",
          RUST_LOG: "h4rn_daemon=debug,h4rn_transport=debug",
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    children.push(daemon);
    daemon.stdout.on("data", (chunk) => logs.push(`daemon: ${chunk}`));
    daemon.stderr.on("data", (chunk) => logs.push(`daemon: ${chunk}`));
    await waitForHttp(`http://127.0.0.1:${daemonPort}/healthz`, 30_000);
    await waitForLog(logs, "relay connected", 30_000);

    const token = run(daemonBin, ["--data-dir", dataDir, "token"]).trim();

    console.log("--- starting vite preview");
    const vite = spawn(
      join(uiRoot, "node_modules", ".bin", "vite"),
      ["preview", "--host", "127.0.0.1", "--port", String(uiPort), "--strictPort"],
      { cwd: uiRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(vite);
    vite.stderr.on("data", (chunk) => logs.push(`vite: ${chunk}`));
    await waitForHttp(`${uiOrigin}/`, 30_000);

    const executablePath = findChromium();
    browser = await chromium.launch({ executablePath, headless: true });
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    // The redirect flow authorizes against OrbitID; this is the browser session
    // the API bootstrap above signed in.
    await context.addCookies([
      {
        name: "__client",
        value: orbit.clientCookie,
        url: orbitBase,
        httpOnly: true,
        sameSite: "Lax",
      },
    ]);
    await context.addInitScript(() => {
      const writeText = async (text) => {
        window.__h4rnCopied = String(text);
      };
      if (!navigator.clipboard) {
        Object.defineProperty(navigator, "clipboard", { value: { writeText } });
      } else {
        Object.defineProperty(navigator.clipboard, "writeText", {
          value: writeText,
          configurable: true,
        });
      }
    });
    page = await context.newPage();
    page.setDefaultTimeout(20_000);
    page.on("console", (message) => logs.push(`browser: ${message.type()} ${message.text()}\n`));
    page.on("pageerror", (error) => logs.push(`browser: pageerror ${error.message}\n`));
    page.on("requestfailed", (request) =>
      logs.push(`requestfailed: ${request.url().split("?")[0]} ${request.failure()?.errorText}\n`),
    );
    // Relay and secure sockets are the interesting ones; full payloads stay out
    // of the log, only frame kinds and sizes.
    page.on("websocket", (socket) => {
      const url = socket.url().split("?")[0];
      if (!url.endsWith("/relay") && !url.endsWith("/secure")) return;
      logs.push(`ws ${url} opening\n`);
      socket.on("framereceived", (event) =>
        logs.push(`ws ${url} received ${String(event.payload).length} bytes\n`),
      );
      socket.on("framesent", (event) =>
        logs.push(`ws ${url} sent ${String(event.payload).length} bytes\n`),
      );
      socket.on("socketerror", (error) => logs.push(`ws ${url} error ${error}\n`));
      socket.on("close", () => logs.push(`ws ${url} closed\n`));
    });

    await page.goto(
      `${uiOrigin}/?token=${encodeURIComponent(token)}` +
        `&daemon=${encodeURIComponent(`ws://127.0.0.1:${daemonPort}/ws`)}`,
    );

    // A thread created over the direct loopback path survives the switch to
    // the relay; opening it afterwards proves the relay carries real state.
    await page.getByRole("button", { name: "New thread" }).first().click();
    await page.locator("#new-thread-workspace").fill(workspace);
    await page.locator("#new-thread-harness").click();
    await page.getByRole("menuitem", { name: "Claude Code" }).click();
    await page.locator("#new-thread-task").fill("Reply with exactly: remote pong");
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent?.trim() === "Start",
      );
      return button !== undefined && !button.disabled;
    });
    await page.getByRole("button", { name: "Start", exact: true }).click();
    await page.getByText(/echo: Reply with exactly: remote pong/).waitFor({ timeout: 20_000 });
    const header = page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) });
    await page.getByRole("button", { name: "Thread actions" }).click();
    await page.getByRole("menuitem", { name: "Settle now" }).click();
    await header.getByText("Settled", { exact: true }).waitFor({ timeout: 20_000 });
    const threadPath = new URL(page.url()).pathname;
    if (!threadPath.startsWith("/t/")) {
      throw new Error(`the thread did not get a /t/<id> route: ${threadPath}`);
    }

    // Sign in through the shipped Settings UI with the browser redirect flow.
    const footer = page.locator('[title="Hosts and connection"]');
    await footer.click();
    await page.getByRole("menuitem", { name: /Sign in to h4rn-connect/ }).click();
    await page.getByRole("heading", { name: "Devices", exact: true }).waitFor();
    const relayInput = page.getByLabel("h4rn-connect URL");
    await relayInput.fill(relayUrl);
    await page.getByRole("button", { name: "Use", exact: true }).click();
    await page.getByRole("button", { name: /Sign in with OrbitID/ }).click();
    // OrbitID authorizes the pre-seeded browser session, redirects to
    // /auth/callback, and the app reloads the route it left. Reaching the
    // signed-in Devices panel proves the full redirect round trip.
    await page.getByRole("heading", { name: /^Pair a device/ }).waitFor({
      timeout: 60_000,
    });
    if (new URL(page.url()).origin !== uiOrigin) {
      throw new Error(`the redirect flow did not return to the UI: ${page.url()}`);
    }
    const qr = page.getByRole("img", { name: "Pairing QR code for atlas" });
    await qr.waitFor({ timeout: 30_000 });
    await page.getByRole("button", { name: "Copy link", exact: true }).click();
    const pairingLink = await page.evaluate(() => window.__h4rnCopied ?? null);
    if (!pairingLink?.startsWith("h4rn://pair?")) {
      throw new Error(`the pairing link was not copied: ${pairingLink}`);
    }
    await page.evaluate(
      (link) => window.dispatchEvent(new CustomEvent("h4rn-connect-pair", { detail: link })),
      pairingLink,
    );
    await page.getByText(/Paired with atlas/).waitFor({ timeout: 30_000 });

    // Signing in and pairing happen on the Settings route. Booting the app
    // again at the thread route with no query string is a full reload, so the
    // thread log and the footer path must come from persisted relay state.
    await page.goto(`${uiOrigin}${threadPath}`);
    await page.getByText(/echo: Reply with exactly: remote pong/).waitFor({ timeout: 40_000 });
    await page
      .locator("header")
      .filter({ has: page.getByRole("button", { name: "Thread actions" }) })
      .getByText("Settled", { exact: true })
      .waitFor({ timeout: 20_000 });

    await footer.click();
    const hostsMenu = page.getByRole("menu", { name: "Hosts" });
    await hostsMenu.waitFor();
    // The machine is shown per host, so the line names the active host ("Relay
    // to Debian") rather than the connect machine ("atlas"); the relay noun is
    // the contract here.
    const connectionLine = hostsMenu.locator(".q-hp-conn");
    await page.getByText(/^Relay to /).waitFor({ timeout: 30_000 });
    console.log(`--- relay footer: ${(await connectionLine.textContent())?.trim()}`);

    // Revoke this browser's device; the daemon drops the allowlisted key and
    // the live relay session ends.
    await hostsMenu.getByRole("menuitem", { name: /Pair a device/ }).click();
    await page.getByRole("heading", { name: "Devices", exact: true }).waitFor();
    const deviceRow = page.getByRole("row").filter({ hasText: "this device" });
    await deviceRow.waitFor({ timeout: 20_000 });
    const logMark = logs.length;
    // This device offers Sign out (the same revoke) instead of Revoke.
    await deviceRow.getByRole("button", { name: "Sign out", exact: true }).click();
    await deviceRow.getByRole("button", { name: "Sign out", exact: true }).click();
    await deviceRow.waitFor({ state: "detached", timeout: 20_000 });

    // Server-side proof the session ended: the daemon dropped the allowlisted
    // key and the relay closed the browser's tunnel.
    await waitForLogSince(logs, logMark, "revoked a paired device", 30_000);
    await waitForLogSince(logs, logMark, "/relay closed", 30_000);
    // Revoking this device also drops its dead credentials, so the Devices
    // panel returns to the signed-out sign-in affordances.
    await page
      .getByRole("button", { name: /Sign in on another device|Sign in with OrbitID/ })
      .first()
      .waitFor({ timeout: 30_000 });

    // The device-code grant stays available as a fallback for shells without a
    // usable browser redirect. The relay refuses to re-register the key of a
    // revoked device, so the fallback signs in from a fresh browser profile
    // (a new device identity), exactly like a second device would.
    const fallbackContext = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    page = await fallbackContext.newPage();
    page.setDefaultTimeout(20_000);
    page.on("console", (message) => logs.push(`browser2: ${message.type()} ${message.text()}\n`));
    await page.goto(
      `${uiOrigin}/?token=${encodeURIComponent(token)}` +
        `&daemon=${encodeURIComponent(`ws://127.0.0.1:${daemonPort}/ws`)}`,
    );
    await page.locator('[title="Hosts and connection"]').click();
    await page.getByRole("menuitem", { name: /Sign in to h4rn-connect/ }).click();
    await page.getByLabel("h4rn-connect URL").fill(relayUrl);
    await page.getByRole("button", { name: "Use", exact: true }).click();
    await page.getByRole("button", { name: /Sign in on another device/ }).click();
    const code = (await page.locator(".q-code-big").textContent())?.trim();
    if (!code) throw new Error("the sign-in panel did not show a device code");
    await approveDeviceCode(orbitBase, orbit.sessionJwt, code);
    await page.getByRole("heading", { name: /^Pair a device/ }).waitFor({ timeout: 30_000 });

    // Pair by the typed short code with a restricted scope. This second
    // browser makes the pairing, drops "Approve and reply" (connect recreates
    // the code with the new scopes), then claims the code on the phone
    // pairing screen, so it ends up holding `view` alone.
    const codeLabel = page.locator(".q-code-big");
    await codeLabel.waitFor({ timeout: 30_000 });
    const offeredCode = (await codeLabel.textContent())?.trim() ?? "";
    await page.getByText("Approve and reply", { exact: true }).click();
    await page.waitForFunction(
      (previous) => {
        const text = document.querySelector(".q-code-big")?.textContent?.trim();
        return Boolean(text) && text !== previous;
      },
      offeredCode,
      { timeout: 30_000 },
    );
    const restrictedCode = (await codeLabel.textContent())?.trim() ?? "";
    if (!/^[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}$/.test(restrictedCode)) {
      throw new Error(`the short code is not 8 Crockford characters: ${restrictedCode}`);
    }

    await page.goto(`${uiOrigin}/pairing`);
    const codeInput = page.getByRole("textbox", { name: "Pairing code" });
    await codeInput.fill(restrictedCode.toLowerCase());
    await page.getByRole("button", { name: "Pair with code" }).click();
    await page.getByText(/Paired with atlas/).waitFor({ timeout: 30_000 });

    // The code is single-use.
    await page.goto(`${uiOrigin}/pairing`);
    await page.getByRole("textbox", { name: "Pairing code" }).fill(restrictedCode);
    await page.getByRole("button", { name: "Pair with code" }).click();
    await page
      .getByText(/generate a new one|invalid or expired/)
      .first()
      .waitFor({ timeout: 20_000 });

    // Over the relay this device can read threads but not approve or reply: the
    // daemon enforces the scope the pairing granted.
    await page.goto(`${uiOrigin}${threadPath}`);
    await page.getByText(/echo: Reply with exactly: remote pong/).waitFor({ timeout: 40_000 });
    await page
      .getByText(/^Relay to /)
      .waitFor({ timeout: 30_000 })
      .catch(() => {});
    const restrictedComposer = page.getByRole("textbox", { name: /Reply to/ });
    const refusal = page.getByText(/paired without the .approve. permission/).first();
    // The relay session may still be coming up when the first reply goes out,
    // so retry until the daemon's refusal arrives.
    let refused = false;
    for (let attempt = 0; attempt < 4 && !refused; attempt += 1) {
      await restrictedComposer.fill("This should be refused");
      await restrictedComposer.press("Enter");
      refused = await refusal
        .waitFor({ timeout: 10_000 })
        .then(() => true)
        .catch(() => false);
    }
    if (!refused) throw new Error("the daemon never refused the reply of a view-only device");
    console.log(
      "Remote UI e2e passed: browser redirect signed in, paired, relayed a thread, revoked the device, the device-code fallback signed in again, and a view-only code pairing was refused approval by the daemon.",
    );
  } catch (error) {
    if (page) {
      console.error(`--- page url ---\n${page.url()}`);
      console.error(`--- page text ---\n${await page.locator("body").textContent()}`);
    }
    if (logs.length) console.error(`--- logs ---\n${logs.join("")}`);
    throw error;
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children) {
      if (child.exitCode === null) child.kill("SIGTERM");
    }
    if (composeUp) {
      try {
        compose(["down", "-v", "--remove-orphans"]);
      } catch (error) {
        console.error(`--- compose teardown failed: ${error.message}`);
      }
    }
    rmSync(tempRoot, { recursive: true, force: true });
  }
}

async function waitForLog(logs, needle, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (logs.some((entry) => entry.includes(needle))) return;
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${needle} in the daemon log`);
}

async function waitForLogSince(logs, mark, needle, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (logs.slice(mark).some((entry) => entry.includes(needle))) return;
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${needle} in the daemon log`);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
