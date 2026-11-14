/**
 * Mobile platform adapter.
 *
 * This is the only place the shared React bundle knows it is running inside the
 * Tauri mobile shell. It exists because the UI is a pure view over the daemon
 * (ADR-001) and has no pairing screen of its own: the phone still needs a way
 * to learn `ws(s)://daemon/ws` plus the access token before the app can connect.
 *
 * Responsibilities, all Android/iOS-only and inert in a browser or on desktop:
 *
 * - read a pairing from a cold-start deep link (`h4rn://connect?daemon=&token=`),
 *   a warm deep link, the Android Keystore, or the form below;
 * - persist it through the platform secret store (Android Keystore / iOS
 *   Keychain behind `secret_*`), never `localStorage` (ADR-016, ADR-038);
 * - put only the non-secret `?daemon=` on the URL; the token stays in the
 *   secret store and an in-memory cache that `lib/client/config.ts` reads;
 * - scan a pairing QR with the barcode-scanner plugin;
 * - make `env(safe-area-inset-*)` available by forcing `viewport-fit=cover`.
 *
 * The visual content of this overlay is intentionally plain: Phase 3 is the
 * shell, build pipeline and native integrations. A later UI pass replaces it.
 */

import { rememberConnectPair, rememberOidcCallback } from "./pending";
import { getSecret, peekSecret, prepareSecrets, setSecret } from "./secrets";

const PAIRING_KEY = "h4rn.pairing";
const TOKEN_KEY = "h4rn.token";

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

type Pairing = { daemon: string; token: string };

type ScanResult = { content: string };

/** Tauri mobile plugins reject with `{ message }` objects, not `Error`s. */
function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

function tauri(): TauriInternals | null {
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" ? candidate : null;
}

const internals = tauri();
const isMobileShell =
  internals !== null && /Android|iPhone|iPad|iPod/i.test(window.navigator.userAgent);

/** Normalises what someone types or scans into a WebSocket URL. */
export function normaliseDaemon(raw: string): string {
  let value = raw.trim();
  if (value.length === 0) throw new Error("Enter a daemon URL.");

  // `10.0.2.2:47800` and `http://host:47800` are the two things people paste
  // most often; the protocol must become ws/wss, and `/ws` is the daemon route.
  if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(value)) value = `ws://${value}`;
  const url = new URL(value);
  if (url.protocol === "http:") url.protocol = "ws:";
  if (url.protocol === "https:") url.protocol = "wss:";
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new Error("The daemon URL must be ws:// or wss:// (http/https also work).");
  }
  if (url.pathname === "" || url.pathname === "/") url.pathname = "/ws";
  return url.toString();
}

/** Parses `h4rn://connect|pair?daemon=…&token=…`, returning null for other URLs. */
export function parsePairingUrl(raw: string): Pairing | null {
  const url = new URL(raw);
  if (url.protocol !== "h4rn:") return null;
  const target = (url.hostname || url.pathname.replace(/^\/+/, "")).toLowerCase();
  if (!["connect", "pair", "oidc", "callback"].includes(target)) return null;

  if (target === "oidc" || target === "callback") {
    // The platform has no auth screen of its own; hand the callback to the UI
    // (or to a future auth flow) without touching the daemon connection. A
    // cold-start link arrives before React mounts, so keep it in memory for
    // the remote store; an authorization code is never written to storage.
    const detail = Object.fromEntries(url.searchParams.entries());
    rememberOidcCallback(detail);
    window.dispatchEvent(new CustomEvent("h4rn-oidc-callback", { detail }));
    return null;
  }

  // A connect pairing QR names the relay, machine and daemon Noise key; the
  // remote feature claims it. It carries no daemon token, so handle it before
  // the local pairing shape below.
  if (target === "pair" && url.searchParams.get("c") && url.searchParams.get("m")) {
    rememberConnectPair(raw);
    window.dispatchEvent(new CustomEvent("h4rn-connect-pair", { detail: raw }));
    return null;
  }

  const daemon = url.searchParams.get("daemon");
  const token = url.searchParams.get("token");
  if (!daemon || !token) {
    throw new Error("The pairing link is missing daemon or token.");
  }
  return { daemon: normaliseDaemon(daemon), token };
}

function currentQueryPairing(): Pairing | null {
  const params = new URLSearchParams(window.location.search);
  const daemon = params.get("daemon");
  const token = params.get("token");
  if (!daemon || !token) return null;
  return { daemon: normaliseDaemon(daemon), token };
}

async function readStoredPairing(): Promise<Pairing | null> {
  try {
    const raw = (await getSecret(PAIRING_KEY)) ?? (await getSecret(TOKEN_KEY)) ?? null;
    if (!raw) return null;
    // Older builds stored only the token in `h4rn.token`; the pairing record
    // is JSON. Accept either so an upgrade keeps the device connected.
    if (!raw.trimStart().startsWith("{")) {
      const daemon = new URLSearchParams(window.location.search).get("daemon");
      return daemon ? { daemon: normaliseDaemon(daemon), token: raw } : null;
    }
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return null;
    const record = parsed as Record<string, unknown>;
    if (typeof record.daemon !== "string" || typeof record.token !== "string") return null;
    return { daemon: normaliseDaemon(record.daemon), token: record.token };
  } catch {
    return null;
  }
}

/**
 * Persists a pairing everywhere the app needs it, reloading only when the URL
 * or the token actually changed.
 *
 * The daemon URL is a non-secret address and stays in the query string so a
 * reload boots against the right host. The token is written to the platform
 * secret store and never to `localStorage` or the URL.
 */
export async function commitPairing(pairing: Pairing): Promise<void> {
  const tokenChanged = peekSecret(TOKEN_KEY) !== pairing.token;
  const url = new URL(window.location.href);
  const daemonChanged = url.searchParams.get("daemon") !== pairing.daemon;

  await setSecret(TOKEN_KEY, pairing.token);
  await setSecret(PAIRING_KEY, JSON.stringify(pairing));
  // Clean up anything a pre-ADR-038 build left behind, including the token
  // from a dev deep link that put it in the address bar.
  try {
    window.localStorage.removeItem(TOKEN_KEY);
    window.localStorage.removeItem("h4rn.daemon");
    window.localStorage.removeItem(PAIRING_KEY);
  } catch {
    // A locked-down webview has nothing to clean.
  }

  if (!tokenChanged && !daemonChanged) return;

  url.searchParams.set("daemon", pairing.daemon);
  url.searchParams.delete("token");
  window.location.replace(url.toString());
}

async function coldStartPairing(): Promise<Pairing | null> {
  if (!internals) return null;
  try {
    const urls = await internals.invoke<string[]>("deep_link_current");
    for (const raw of urls) {
      try {
        const pairing = parsePairingUrl(raw);
        if (pairing) return pairing;
      } catch {
        // Keep looking: one malformed URL should not hide a good one.
      }
    }
  } catch {
    // Older shells may not expose the command; the form still works.
  }
  return null;
}

function safeAreaStyle(): void {
  const meta = document.querySelector<HTMLMetaElement>('meta[name="viewport"]');
  const content = meta?.getAttribute("content") ?? "width=device-width, initial-scale=1.0";
  if (meta && !/viewport-fit\s*=\s*cover/.test(content)) {
    meta.setAttribute("content", `${content.trim()}, viewport-fit=cover`);
  }

  const style = document.createElement("style");
  style.dataset.h4rn = "mobile-safe-area";
  // The pairing overlay follows the system light/dark preference because the
  // Android status bar icons follow it too (MainActivity enables edge-to-edge
  // with the platform default styles). Keeping the two in sync means the clock
  // never disappears into the overlay background.
  style.textContent = `
    :root {
      --h4rn-safe-area-top: env(safe-area-inset-top, 0px);
      --h4rn-safe-area-right: env(safe-area-inset-right, 0px);
      --h4rn-safe-area-bottom: env(safe-area-inset-bottom, 0px);
      --h4rn-safe-area-left: env(safe-area-inset-left, 0px);
      --h4rn-pair-bg: #0d1017;
      --h4rn-pair-fg: #f3f5f9;
      --h4rn-pair-muted: #98a2b3;
      --h4rn-pair-faint: #667085;
      --h4rn-pair-field-bg: #141a23;
      --h4rn-pair-field-border: #2b3442;
      --h4rn-pair-accent: #7ee0b8;
      --h4rn-pair-accent-fg: #07110c;
      --h4rn-pair-error: #ff9b9b;
      --h4rn-page-bg: #0d1017;
      color-scheme: light dark;
    }
    @media (prefers-color-scheme: light) {
      :root {
        --h4rn-pair-bg: #f7f6f2;
        --h4rn-pair-fg: #171a1f;
        --h4rn-pair-muted: #5b6472;
        --h4rn-pair-faint: #7a8493;
        --h4rn-pair-field-bg: #ffffff;
        --h4rn-pair-field-border: #d5d7d3;
        --h4rn-pair-accent: #b45309;
        --h4rn-pair-accent-fg: #ffffff;
        --h4rn-pair-error: #b42318;
        --h4rn-page-bg: #f7f6f2;
      }
    }
    html, body { background: var(--h4rn-page-bg); }
    html { -webkit-text-size-adjust: 100%; }
    /* Android 15+ draws the WebView behind the status bar. The app shell is a
       fixed-height flex column that already pads the navigation bar for the
       bottom inset; give the same treatment to the top so the clock and the
       host status are not covered by the system status bar. Desktop and the
       browser report a zero inset, so this rule is inert there. */
    #root > :first-child {
      box-sizing: border-box;
      padding-top: env(safe-area-inset-top, 0px);
    }
  `;
  document.head.append(style);
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  text?: string,
  style?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (style !== undefined) node.setAttribute("style", style);
  return node;
}

function renderConnect(initialMessage?: string): void {
  if (document.getElementById("h4rn-connect")) return;

  const overlay = el(
    "div",
    undefined,
    [
      "position:fixed",
      "inset:0",
      "z-index:2147483647",
      "display:flex",
      "flex-direction:column",
      "gap:18px",
      "box-sizing:border-box",
      "padding:calc(env(safe-area-inset-top, 0px) + 32px) 22px calc(env(safe-area-inset-bottom, 0px) + 22px) 22px",
      "background:var(--h4rn-pair-bg)",
      "color:var(--h4rn-pair-fg)",
      "font:15px/1.45 system-ui, -apple-system, sans-serif",
      "overflow:auto",
    ].join(";"),
  );
  overlay.id = "h4rn-connect";

  const mark = el(
    "div",
    "h4rn",
    "font:600 13px/1 ui-monospace, monospace;letter-spacing:.28em;color:var(--h4rn-pair-accent);text-transform:lowercase",
  );
  const title = el(
    "h1",
    "Connect this phone",
    "margin:0;font:600 30px/1.1 system-ui, sans-serif;letter-spacing:-.03em",
  );
  const intro = el(
    "p",
    "Point h4rn at the daemon you already run. The token is the one `h4rnd token` prints; it is stored in the Android Keystore, not in plain text.",
    "margin:0;color:var(--h4rn-pair-muted);max-width:34rem",
  );

  const form = el("form", undefined, "display:flex;flex-direction:column;gap:12px;max-width:34rem");
  const daemonLabel = el(
    "label",
    "Daemon URL",
    "display:flex;flex-direction:column;gap:6px;font-weight:600;font-size:13px;color:var(--h4rn-pair-muted)",
  );
  const daemonInput = el(
    "input",
    undefined,
    "box-sizing:border-box;width:100%;padding:12px 14px;border-radius:10px;border:1px solid var(--h4rn-pair-field-border);background:var(--h4rn-pair-field-bg);color:var(--h4rn-pair-fg);font:14px/1.2 ui-monospace, monospace",
  );
  daemonInput.type = "text";
  daemonInput.inputMode = "url";
  daemonInput.autocapitalize = "off";
  daemonInput.autocomplete = "off";
  daemonInput.spellcheck = false;
  daemonInput.placeholder = "ws://127.0.0.1:47800/ws";
  daemonInput.value = "ws://127.0.0.1:47800/ws";
  daemonLabel.append(daemonInput);

  const tokenLabel = el(
    "label",
    "Access token",
    "display:flex;flex-direction:column;gap:6px;font-weight:600;font-size:13px;color:var(--h4rn-pair-muted)",
  );
  const tokenInput = el(
    "input",
    undefined,
    "box-sizing:border-box;width:100%;padding:12px 14px;border-radius:10px;border:1px solid var(--h4rn-pair-field-border);background:var(--h4rn-pair-field-bg);color:var(--h4rn-pair-fg);font:14px/1.2 ui-monospace, monospace",
  );
  tokenInput.type = "password";
  tokenInput.autocapitalize = "off";
  tokenInput.autocomplete = "off";
  tokenInput.spellcheck = false;
  tokenInput.placeholder = "64-character hex token";
  tokenLabel.append(tokenInput);

  const actions = el("div", undefined, "display:flex;gap:10px;flex-wrap:wrap;align-items:center");
  const connect = el(
    "button",
    "Connect",
    "appearance:none;border:0;border-radius:10px;padding:12px 18px;background:var(--h4rn-pair-accent);color:var(--h4rn-pair-accent-fg);font:650 14px/1 system-ui, sans-serif;cursor:pointer",
  );
  connect.type = "submit";
  const scan = el(
    "button",
    "Scan QR",
    "appearance:none;border:1px solid var(--h4rn-pair-field-border);border-radius:10px;padding:12px 18px;background:var(--h4rn-pair-field-bg);color:var(--h4rn-pair-fg);font:650 14px/1 system-ui, sans-serif;cursor:pointer",
  );
  scan.type = "button";
  actions.append(connect, scan);

  const status = el(
    "p",
    initialMessage ?? "",
    "margin:0;min-height:1.4em;color:var(--h4rn-pair-muted);font-size:13px;white-space:pre-wrap",
  );

  const hint = el(
    "p",
    "Emulator build: run `scripts/mobile-android.sh run`, which starts the daemon, wires `adb reverse tcp:47800 tcp:47800` and passes this URL and token. Or open `h4rn://connect?daemon=…&token=…`.",
    "margin:0;color:var(--h4rn-pair-faint);font-size:12px;max-width:34rem",
  );

  form.append(daemonLabel, tokenLabel, actions, status);
  overlay.append(mark, title, intro, form, hint);
  document.body.append(overlay);
  tokenInput.focus({ preventScroll: true });

  let busy = false;
  const setStatus = (message: string, error = false): void => {
    status.textContent = message;
    status.style.color = error ? "var(--h4rn-pair-error, #ff9b9b)" : "var(--h4rn-pair-muted)";
  };

  const apply = async (pairing: Pairing): Promise<void> => {
    setStatus("Saving the pairing…");
    await commitPairing(pairing);
    setStatus("Connecting…");
  };

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (busy) return;
    busy = true;
    void (async () => {
      try {
        await apply({
          daemon: normaliseDaemon(daemonInput.value),
          token: tokenInput.value.trim(),
        });
      } catch (error) {
        setStatus(error instanceof Error ? error.message : String(error), true);
      } finally {
        busy = false;
      }
    })();
  });

  scan.addEventListener("click", () => {
    if (!internals || busy) return;
    busy = true;
    setStatus("Opening the camera…");
    void (async () => {
      try {
        // The barcode plugin checks instead of requesting, so the camera prompt
        // has to come from the core permission command first.
        await internals.invoke("plugin:barcode-scanner|request_permissions");
        const result = await internals.invoke<ScanResult>("plugin:barcode-scanner|scan", {
          windowed: true,
          formats: ["QR_CODE"],
        });
        const pairing = parsePairingUrl(result.content);
        if (!pairing) throw new Error("That QR code is not an h4rn pairing link.");
        await apply(pairing);
      } catch (error) {
        setStatus(`${describeError(error)} You can still type the URL and token.`, true);
      } finally {
        busy = false;
      }
    })();
  });
}

async function boot(): Promise<void> {
  safeAreaStyle();

  // 1. A warm link or a cold-start intent wins over stored state: the person
  //    just chose to pair with a specific daemon.
  const linked = (await coldStartPairing()) ?? currentQueryPairing();
  if (linked) {
    await commitPairing(linked);
    return;
  }

  // 2. Nothing in the URL: reuse the Keystore pairing without a UI detour.
  const stored = await readStoredPairing();
  if (stored) {
    await commitPairing(stored);
    return;
  }

  // 3. First launch on this device.
  renderConnect();
}

/**
 * Boots the platform layer before React mounts.
 *
 * Every shell loads its secret store first: `readBootConfig()` is synchronous
 * and needs the daemon token from the cache. On a mobile shell this also
 * consumes a cold-start pairing/deep link and repaints the safe-area CSS.
 */
export async function preparePlatform(): Promise<void> {
  await prepareSecrets();
  if (!isMobileShell) return;
  watchDeepLinks();
  await boot();
}

function watchDeepLinks(): void {
  window.addEventListener("h4rn-deep-link", (event) => {
    const detail = (event as CustomEvent<unknown>).detail;
    if (!Array.isArray(detail)) return;
    for (const raw of detail) {
      if (typeof raw !== "string") continue;
      try {
        const pairing = parsePairingUrl(raw);
        if (pairing) {
          void commitPairing(pairing).catch((error: unknown) => {
            renderConnect(error instanceof Error ? error.message : String(error));
          });
          return;
        }
      } catch (error) {
        renderConnect(error instanceof Error ? error.message : String(error));
        return;
      }
    }
  });
}
