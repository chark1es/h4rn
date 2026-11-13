import type { TransportStatus } from "@/gen";
import type { SecureTransport as WasmSecureTransport } from "@/gen/transport-wasm/h4rn_transport_wasm";
import initWasm, {
  connect as connectTransport,
  generate_identity,
  parse_pairing,
} from "@/gen/transport-wasm/h4rn_transport_wasm";

import type { Transport, TransportContext, TransportFactory, TransportHandlers } from "./types";

/**
 * Browser adapter for the Rust Noise/multipath transport.
 *
 * Every cryptographic operation, the Noise handshake, the session layer and
 * path selection live in `h4rn-transport-wasm`. This file only owns the two
 * things a browser must own: WebSockets and timers. No key material is ever
 * read or produced by TypeScript beyond moving opaque strings between
 * `localStorage` and wasm (ADR-001, ADR-009, ADR-010).
 *
 * The daemon issues a resume token once per logical session. `H4rnClient`
 * throws away and recreates a `Transport` on every reconnect, so the token and
 * the session id are persisted here, outside the transport instance.
 */

const IDENTITY_KEY = "h4rn.secure.identity";
const PIN_KEY = "h4rn.secure.pin";
const PENDING_KEY = "h4rn.secure.pending";
const SESSION_KEY = "h4rn.secure.session";

/** How often the wasm core runs probes, retransmits and path selection. */
const TICK_MS = 50;

type PairingPin = {
  endpoint: string;
  daemonPublic: string;
  label: string | null;
  relay: string | null;
};

type PendingPairing = PairingPin & {
  psk: string;
};

type StoredSession = {
  sessionId: string;
  resumeToken?: string | null;
};

type PathConfig = {
  kind: "direct" | "tailscale" | "relay";
  endpoint: string;
};

export type SecureTransportOptions = {
  /** Raw `h4rn://pair?...` URI from the URL or a QR scan. */
  pairing?: string | null;
  /** Optional relay standby endpoint (`host:port` or `wss://...`). */
  relay?: string | null;
  /** Injectable storage for tests. */
  storage?: Pick<Storage, "getItem" | "setItem" | "removeItem"> | null;
};

let wasmReady: Promise<unknown> | null = null;

function loadWasm(): Promise<unknown> {
  wasmReady ??= initWasm();
  return wasmReady;
}

/**
 * Creates a factory that `H4rnClient` calls once per reconnect attempt.
 *
 * The returned transport keeps its own socket set; session continuity comes
 * from the persisted identity and resume token, not from object identity.
 */
export function createSecureTransportFactory(
  options: SecureTransportOptions = {},
): TransportFactory {
  return (context: TransportContext) => new SecureTransport(context, options);
}

/** Whether this browser has ever pinned a daemon, so boot can choose Noise. */
export function hasStoredSecurity(
  storage: Pick<Storage, "getItem"> | null = defaultStorage(),
): boolean {
  return Boolean(storage?.getItem(PIN_KEY) ?? storage?.getItem(PENDING_KEY));
}

export class SecureTransport implements Transport {
  readonly kind = "websocket" as const;

  private readonly storage: SecureTransportOptions["storage"];
  private handlers: TransportHandlers | null = null;
  private wasm: WasmSecureTransport | null = null;
  private sockets: Array<WebSocket | null> = [];
  private outbound: Array<Uint8Array[]> = [];
  private paths: PathConfig[] = [];
  private timer: ReturnType<typeof setInterval> | null = null;
  private sessionId: string | null = null;
  private established = false;
  private resumeAttempted = false;
  private notifiedClose = false;
  private closedByUs = false;
  private closedSockets = 0;
  private openedOnce = false;

  constructor(
    private readonly context: TransportContext,
    private readonly options: SecureTransportOptions = {},
  ) {
    this.storage = options.storage === undefined ? defaultStorage() : options.storage;
  }

  connect(handlers: TransportHandlers): void {
    this.handlers = handlers;
    this.closedByUs = false;
    this.notifiedClose = false;
    void this.run();
  }

  send(data: string | ArrayBuffer | Uint8Array): void {
    if (!this.wasm) throw new Error("The secure transport is still starting.");
    if (typeof data === "string") {
      this.wasm.send(new TextEncoder().encode(data), false, nowMs());
      return;
    }
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
    this.wasm.send(bytes, true, nowMs());
  }

  close(): void {
    this.closedByUs = true;
    this.stopTimer();
    try {
      this.wasm?.close(nowMs());
    } catch {
      // The core may already be closed; the sockets below still need closing.
    }
    for (const socket of this.sockets) {
      closeSocket(socket);
    }
    this.sockets = [];
    this.wasm?.free();
    this.wasm = null;
  }

  // ---------------------------------------------------------------------
  // Boot
  // ---------------------------------------------------------------------

  private async run(): Promise<void> {
    const handlers = this.handlers;
    if (!handlers) return;

    try {
      await loadWasm();
      if (this.closedByUs) return;

      const resolved = this.resolveConfig();
      if (!resolved) {
        this.finishClose(
          4401,
          "This browser is not paired with a daemon.",
          true,
          "Scan the daemon's pairing code, then reopen the app.",
        );
        return;
      }
      this.paths = resolved.paths;
      this.resumeAttempted = Boolean(resolved.resumeToken);
      this.sessionId = resolved.sessionId;

      const wasm = connectTransport(
        JSON.stringify({
          sessionId: resolved.sessionId,
          identity: resolved.identity,
          daemonPublic: resolved.daemonPublic,
          psk: resolved.psk,
          resumeToken: resolved.resumeToken,
          paths: resolved.paths,
        }),
      );
      this.wasm = wasm;

      // `Core::new` generates a random session id when the config passes all
      // zeroes. Read it back so a reconnect can resume the same session.
      try {
        const status = JSON.parse(wasm.status_json()) as { session_id?: string };
        if (status.session_id) this.sessionId = status.session_id;
      } catch {
        // Keep the id we asked for; status reporting is best-effort.
      }
      this.persistSession({ sessionId: this.sessionId ?? resolved.sessionId });
      this.pinned = resolved.pin;

      wasm.on_packet((index: number, bytes: Uint8Array) => this.onPacket(index, bytes));
      wasm.on_frame((value: string | Uint8Array) => this.onFrame(value));
      wasm.on_status((json: string) => this.onStatus(json));
      wasm.on_ticket((token: string) => this.onTicket(token));
      wasm.on_closed((reason: string) => this.onEngineClosed(reason));

      this.openSockets();
      this.timer = setInterval(() => this.tick(), TICK_MS);
    } catch (error) {
      this.reportFailure(error);
    }
  }

  private pinned: PairingPin | null = null;

  private resolveConfig(): {
    identity: string;
    daemonPublic: string;
    psk: string | undefined;
    sessionId: string;
    resumeToken: string | undefined;
    paths: PathConfig[];
    pin: PairingPin;
  } | null {
    const storedPin = readJson<PairingPin>(this.storage, PIN_KEY);
    const pending = readJson<PendingPairing>(this.storage, PENDING_KEY);
    let pin = storedPin;
    let psk = pending?.psk;

    if (this.options.pairing) {
      try {
        const parsed = JSON.parse(parse_pairing(this.options.pairing)) as {
          endpoint: string;
          daemonPublic: string;
          psk: string;
          label?: string | null;
        };
        const fresh: PendingPairing = {
          endpoint: parsed.endpoint,
          daemonPublic: parsed.daemonPublic,
          psk: parsed.psk,
          label: parsed.label ?? null,
          relay: this.options.relay ?? pin?.relay ?? null,
        };
        writeJson(this.storage, PENDING_KEY, fresh);
        // The pin that survives pairing carries no one-time secret.
        pin = {
          endpoint: fresh.endpoint,
          daemonPublic: fresh.daemonPublic,
          label: fresh.label,
          relay: fresh.relay,
        };
        psk = fresh.psk;
      } catch (error) {
        throw new Error(
          error instanceof Error
            ? `Invalid pairing code: ${error.message}`
            : "Invalid pairing code.",
        );
      }
    } else if (pending) {
      pin = pending;
      psk = pending.psk;
    }

    if (!pin && !this.context.url) return null;
    if (!pin) return null;

    const direct = normalizeSecureUrl(pin.endpoint || this.context.url);
    const relay = pin.relay ?? this.options.relay ?? null;
    const paths: PathConfig[] = [{ kind: kindForEndpoint(direct), endpoint: direct }];
    if (relay) {
      paths.push({ kind: "relay", endpoint: normalizeSecureUrl(relay) });
    }

    const session = readJson<StoredSession>(this.storage, SESSION_KEY);
    return {
      identity: readIdentity(this.storage),
      daemonPublic: pin.daemonPublic,
      psk,
      sessionId: session?.sessionId ?? "00".repeat(16),
      resumeToken: session?.resumeToken ?? undefined,
      paths,
      pin,
    };
  }

  // ---------------------------------------------------------------------
  // Socket plumbing
  // ---------------------------------------------------------------------

  private openSockets(): void {
    this.closedSockets = 0;
    this.sockets = this.paths.map(() => null);
    this.outbound = this.paths.map(() => []);

    this.paths.forEach((path, index) => {
      let socket: WebSocket;
      try {
        socket = new WebSocket(path.endpoint);
      } catch (error) {
        this.handlers?.onError(
          error instanceof Error
            ? `Could not reach ${path.endpoint}.`
            : "The secure URL was rejected.",
        );
        this.socketClosed();
        return;
      }
      socket.binaryType = "arraybuffer";
      this.sockets[index] = socket;

      socket.onopen = () => {
        if (this.closedByUs) return;
        try {
          this.wasm?.path_open(index, nowMs());
        } catch (error) {
          this.reportFailure(error);
          return;
        }
        const queued = this.outbound[index]?.splice(0) ?? [];
        for (const packet of queued) sendSocket(socket, packet);
      };
      socket.onmessage = (message) => {
        if (this.closedByUs) return;
        if (typeof message.data === "string") {
          this.handlers?.onError("The daemon sent plaintext on the secure path.");
          return;
        }
        const buffer =
          message.data instanceof ArrayBuffer
            ? message.data
            : message.data instanceof Blob
              ? null
              : null;
        if (buffer) {
          this.receive(index, buffer);
        } else if (message.data instanceof Blob) {
          void message.data.arrayBuffer().then((data) => this.receive(index, data));
        }
      };
      socket.onerror = () => {
        // `onclose` follows and carries the useful frame.
      };
      socket.onclose = (event) => {
        if (this.closedByUs) return;
        this.socketClosed();
        try {
          this.wasm?.path_closed(index, event.reason || "socket closed", nowMs());
        } catch {
          // The engine may already be closed.
        }
        if (this.closedSockets >= this.paths.length && !this.notifiedClose) {
          this.finishClose(1011, "Every secure path closed.", false);
        }
      };
    });
  }

  private onPacket(index: number, bytes: Uint8Array): void {
    const socket = this.sockets[index];
    if (!socket || socket.readyState === WebSocket.CLOSED) return;
    if (socket.readyState === WebSocket.OPEN) {
      sendSocket(socket, bytes);
    } else {
      this.outbound[index]?.push(bytes);
    }
  }

  private receive(index: number, data: ArrayBuffer): void {
    try {
      this.wasm?.receive(index, new Uint8Array(data), nowMs());
    } catch (error) {
      this.reportFailure(error);
    }
  }

  private tick(): void {
    try {
      this.wasm?.tick(nowMs());
    } catch (error) {
      this.reportFailure(error);
    }
  }

  private socketClosed(): void {
    this.closedSockets += 1;
  }

  // ---------------------------------------------------------------------
  // Core callbacks
  // ---------------------------------------------------------------------

  private onFrame(value: string | Uint8Array): void {
    const handlers = this.handlers;
    if (!handlers) return;
    if (typeof value === "string") {
      // Never re-enter wasm from inside a wasm callback: `performHello` sends
      // synchronously, and a recursive borrow of the wasm object panics.
      queueMicrotask(() => {
        if (!this.closedByUs) handlers.onMessage(value);
      });
      return;
    }
    const bytes = value instanceof Uint8Array ? value : new Uint8Array(value);
    const buffer = bytes.buffer.slice(
      bytes.byteOffset,
      bytes.byteOffset + bytes.byteLength,
    ) as ArrayBuffer;
    queueMicrotask(() => {
      if (!this.closedByUs) handlers.onBinary?.(buffer);
    });
  }

  private onStatus(json: string): void {
    let status: TransportStatus;
    try {
      status = JSON.parse(json) as TransportStatus;
    } catch {
      return;
    }
    const handlers = this.handlers;
    const offline = status.state === "offline";
    queueMicrotask(() => {
      if (!this.notifiedClose) handlers?.onTransportStatus?.(status);
    });
    if (offline && !this.notifiedClose) {
      queueMicrotask(() => {
        this.finishClose(1011, status.detail ?? "No secure path is available.", false);
      });
    }
  }

  private onTicket(token: string): void {
    if (this.notifiedClose) return;
    this.established = true;
    if (this.sessionId) {
      this.persistSession({ sessionId: this.sessionId, resumeToken: token });
    }
    // The PSK is single-use. Once the handshake succeeded the daemon has
    // allowlisted this device, so drop the one-time secret and keep the pin.
    if (this.pinned) {
      writeJson(this.storage, PIN_KEY, this.pinned);
      remove(this.storage, PENDING_KEY);
    }
    if (!this.openedOnce) {
      this.openedOnce = true;
      const handlers = this.handlers;
      queueMicrotask(() => {
        if (!this.notifiedClose && !this.closedByUs) handlers?.onOpen();
      });
    }
  }

  private onEngineClosed(reason: string): void {
    if (this.notifiedClose) return;
    const code = closeCodeFor(reason, this.established, this.resumeAttempted, hasPendingPairing());
    // `finishClose` frees the wasm object; defer so the current wasm callback
    // has returned its borrow.
    queueMicrotask(() => this.finishClose(code, reason, false));
  }

  // ---------------------------------------------------------------------
  // Teardown
  // ---------------------------------------------------------------------

  private finishClose(code: number, reason: string, wasClean: boolean, error?: string): void {
    if (this.notifiedClose || this.closedByUs) return;
    this.notifiedClose = true;
    this.stopTimer();

    // A failed resume must not poison every later attempt: drop the ticket so
    // the next transport starts a fresh logical session. A first handshake
    // that never authenticated keeps the pending PSK for a retry or re-pair.
    if (!this.established && this.resumeAttempted) {
      remove(this.storage, SESSION_KEY);
    }

    for (const socket of this.sockets) closeSocket(socket);
    this.sockets = [];
    try {
      this.wasm?.free();
    } catch {
      // Already freed.
    }
    this.wasm = null;

    if (error) this.handlers?.onError(error);
    this.handlers?.onClose({ code, reason, wasClean });
  }

  private reportFailure(error: unknown): void {
    const message = error instanceof Error ? error.message : String(error);
    this.handlers?.onError(message);
    this.finishClose(1011, message, false);
  }

  private stopTimer(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  private persistSession(session: StoredSession): void {
    writeJson(this.storage, SESSION_KEY, session);
  }
}

/**
 * Maps an engine close reason to a WebSocket close code.
 *
 * `H4rnClient` treats 4401 and 4403 as fatal, so a stale resume token or a
 * stale one-time PSK must not immediately become fatal while a fresh session
 * can still be attempted.
 */
function closeCodeFor(
  reason: string,
  established: boolean,
  resumeAttempted: boolean,
  pendingPairing: boolean,
): number {
  const text = reason.toLowerCase();
  if (text.includes("revoked")) return 4403;
  if (text.includes("unsupported")) return 4402;
  if (text.includes("not paired") || text.includes("handshake failed")) {
    return established || (!resumeAttempted && !pendingPairing) ? 4401 : 1011;
  }
  if (text.includes("already in use")) return 1011;
  return 1011;
}

function hasPendingPairing(): boolean {
  return Boolean(readJson<PendingPairing>(defaultStorage(), PENDING_KEY));
}

function sendSocket(socket: WebSocket, bytes: Uint8Array): void {
  try {
    socket.send(bytes);
  } catch {
    // A racing close is handled by `onclose`; the session retransmits if a
    // standby path is still alive.
  }
}

function closeSocket(socket: WebSocket | null): void {
  if (!socket) return;
  if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
    try {
      socket.close(1000, "client closed");
    } catch {
      // Already gone.
    }
  }
}

function defaultStorage(): Pick<Storage, "getItem" | "setItem" | "removeItem"> | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

function readIdentity(storage: SecureTransportOptions["storage"]): string {
  const existing = storage?.getItem(IDENTITY_KEY);
  if (existing) return existing;
  const identity = generate_identity();
  storage?.setItem(IDENTITY_KEY, identity);
  return identity;
}

function readJson<T>(storage: SecureTransportOptions["storage"], key: string): T | null {
  const value = storage?.getItem(key);
  if (!value) return null;
  try {
    return JSON.parse(value) as T;
  } catch {
    return null;
  }
}

function writeJson(storage: SecureTransportOptions["storage"], key: string, value: unknown): void {
  try {
    storage?.setItem(key, JSON.stringify(value));
  } catch {
    // Private-mode storage can reject writes; the transport still works for
    // this page load.
  }
}

function remove(storage: SecureTransportOptions["storage"], key: string): void {
  try {
    storage?.removeItem(key);
  } catch {
    // Ignore.
  }
}

/**
 * Accepts the pairing payload's `host:port`, a `ws(s)://` URL or an
 * `http(s)://` URL and returns the `/secure` WebSocket endpoint.
 */
export function normalizeSecureUrl(endpoint: string): string {
  const trimmed = endpoint.trim();
  const withScheme = /^(wss?|https?):\/\//i.test(trimmed) ? trimmed : `ws://${trimmed}`;
  const url = new URL(withScheme);
  if (url.protocol === "http:") url.protocol = "ws:";
  if (url.protocol === "https:") url.protocol = "wss:";
  if (url.pathname === "/" || url.pathname === "") {
    url.pathname = "/secure";
  }
  return url.toString();
}

/** Tailnet addresses and MagicDNS names get the tailscale preference rank. */
export function kindForEndpoint(endpoint: string): "direct" | "tailscale" {
  try {
    const host = new URL(endpoint).hostname;
    if (host.endsWith(".ts.net")) return "tailscale";
    const octets = host.split(".").map(Number);
    const [first, second] = octets;
    if (
      octets.length === 4 &&
      first === 100 &&
      second !== undefined &&
      second >= 64 &&
      second <= 127
    ) {
      return "tailscale";
    }
  } catch {
    // Fall through to direct.
  }
  return "direct";
}

function nowMs(): number {
  return typeof performance === "undefined" ? Date.now() : performance.now();
}
