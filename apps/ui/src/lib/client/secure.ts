import type { TransportStatus } from "@/gen";
import type { SecureTransport as WasmSecureTransport } from "@/gen/transport-wasm/h4rn_transport_wasm";
import {
  connect as connectTransport,
  generate_identity,
  parse_pairing,
} from "@/gen/transport-wasm/h4rn_transport_wasm";
import { forgetSecret, getSecret, peekSecret, setSecret } from "@/lib/platform/secrets";

import type { Transport, TransportContext, TransportFactory, TransportHandlers } from "./types";
import { loadTransportWasm } from "./wasm";

/**
 * Browser adapter for the Rust Noise/multipath transport.
 *
 * Every cryptographic operation, the Noise handshake, the session layer and
 * path selection live in `h4rn-transport-wasm`. This file only owns the two
 * things a browser must own: WebSockets and timers. No key material is ever
 * read or produced by TypeScript beyond moving opaque strings between the
 * platform secret store and wasm (ADR-001, ADR-009, ADR-010, ADR-038).
 *
 * The daemon issues a resume token once per logical session. `H4rnClient`
 * throws away and recreates a `Transport` on every reconnect, so the token and
 * the session id are persisted here, outside the transport instance.
 */

const IDENTITY_KEY = "h4rn.secure.identity";
const PIN_KEY = "h4rn.secure.pin";
const PENDING_KEY = "h4rn.secure.pending";
const SESSION_KEY = "h4rn.secure.session";
const PAIRING_URI_KEY = "h4rn.secure.pairing-uri";

/** How often the wasm core runs probes, retransmits and path selection. */
const TICK_MS = 50;

/**
 * How long a socket may stay `CONNECTING` before its path is failed.
 *
 * Chromium can leave a WebSocket to an unroutable address connecting for
 * minutes. Without this bound the engine keeps a path in `Probing` and the
 * session never reports offline, so `H4rnClient` never reconnects.
 */
const OPEN_TIMEOUT_MS = 8_000;

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

export type PathConfig = {
  kind: "direct" | "tailscale" | "relay";
  endpoint: string;
};

/**
 * A machine selected from h4rn-connect.
 *
 * Unlike a QR pairing this carries no one-time PSK: the device's Noise key was
 * already allowlisted by the daemon when the pairing was claimed. The relay
 * URL is minted fresh for every attempt because its access token and DPoP proof
 * are short-lived.
 */
export type RemoteTransportConfig = {
  /** Daemon X25519 static public key, hex. */
  daemonPublic: string;
  /** Candidate paths; a `relay` path's endpoint is replaced per attempt. */
  paths: PathConfig[];
  /** Machine id sent in the relay's `open` frame. */
  machineId: string;
  /** Storage key for the resume ticket, so machines do not share tickets. */
  sessionKey?: string;
};

export type SecureTransportOptions = {
  /** Raw `h4rn://pair?...` URI from the URL or a QR scan. */
  pairing?: string | null;
  /** Optional relay standby endpoint (`host:port` or `wss://...`). */
  relay?: string | null;
  /** Explicit remote machine, bypassing the local pairing pin. */
  remote?: RemoteTransportConfig | null;
  /**
   * Mints an authenticated `wss://.../relay` URL. Called before every attempt
   * so an expired access token never becomes a dead transport.
   */
  mintRelayUrl?: (() => Promise<string | null>) | null;
};

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
export function hasStoredSecurity(): boolean {
  return Boolean(peekSecret(PIN_KEY) ?? peekSecret(PENDING_KEY));
}

export class SecureTransport implements Transport {
  readonly kind = "websocket" as const;

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
  /** Relay session id for the single relay path, chosen once per transport. */
  private relayConnId: string | null = null;
  /** Per-machine resume-ticket key; the local pairing uses the default. */
  private sessionKey: string = SESSION_KEY;

  constructor(
    private readonly context: TransportContext,
    private readonly options: SecureTransportOptions = {},
  ) {}

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
      await loadTransportWasm();
      if (this.closedByUs) return;

      const resolved = await this.resolveConfig();
      // A wake may have replaced this transport while its config was being
      // resolved. A cancelled attempt must not create wasm state, sockets or
      // timers behind the replacement's back.
      if (this.closedByUs) return;
      if (!resolved) {
        if (this.options.remote) {
          this.finishClose(
            1011,
            "No route to this machine is available.",
            false,
            "The machine's daemon may be offline. Check the hosts list, then reconnect.",
          );
        } else {
          this.finishClose(
            4401,
            "This browser is not paired with a daemon.",
            true,
            "Scan the daemon's pairing code, then reopen the app.",
          );
        }
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

  private async resolveConfig(): Promise<{
    identity: string;
    daemonPublic: string;
    psk: string | undefined;
    sessionId: string;
    resumeToken: string | undefined;
    paths: PathConfig[];
    pin: PairingPin | null;
  } | null> {
    if (this.options.remote) {
      return this.resolveRemote(this.options.remote);
    }

    const storedPin = await readJson<PairingPin>(PIN_KEY);
    const pending = await readJson<PendingPairing>(PENDING_KEY);
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
        await writeJson(PENDING_KEY, fresh);
        // The pairing URI carries the one-time PSK; consume it so a reload
        // does not replay a secret that the daemon has already burned.
        forgetSecret(PAIRING_URI_KEY);
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

    const session = await readJson<StoredSession>(SESSION_KEY);
    return {
      identity: await ensureDeviceIdentity(),
      daemonPublic: pin.daemonPublic,
      psk,
      sessionId: session?.sessionId ?? "00".repeat(16),
      resumeToken: session?.resumeToken ?? undefined,
      paths,
      pin,
    };
  }

  /**
   * Resolves a connect-selected machine.
   *
   * Relay endpoints are minted per attempt; a failed mint drops that path
   * rather than poisoning the whole transport, so a reachable direct address
   * still works when the relay is down.
   */
  private async resolveRemote(remote: RemoteTransportConfig): Promise<{
    identity: string;
    daemonPublic: string;
    psk: string | undefined;
    sessionId: string;
    resumeToken: string | undefined;
    paths: PathConfig[];
    pin: PairingPin | null;
  } | null> {
    const paths: PathConfig[] = [];
    for (const path of remote.paths) {
      if (path.kind !== "relay" || !this.options.mintRelayUrl) {
        paths.push(path);
        continue;
      }
      try {
        const endpoint = await this.options.mintRelayUrl();
        if (endpoint) paths.push({ kind: "relay", endpoint });
      } catch {
        // No relay this attempt; a direct or tailnet path may still connect.
      }
    }
    if (paths.length === 0) return null;

    this.sessionKey = remote.sessionKey ?? SESSION_KEY;
    const session = await readJson<StoredSession>(this.sessionKey);
    if (paths.some((path) => path.kind === "relay")) {
      this.relayConnId = randomHex(16);
    }
    return {
      identity: await ensureDeviceIdentity(),
      daemonPublic: remote.daemonPublic,
      psk: undefined,
      sessionId: session?.sessionId ?? "00".repeat(16),
      resumeToken: session?.resumeToken ?? undefined,
      paths,
      pin: null,
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
      const openTimer = setTimeout(() => {
        if (this.closedByUs) return;
        // Failing the path wakes the engine, which either selects another path
        // or reports the session offline so the client can reconnect.
        closeSocket(socket);
      }, OPEN_TIMEOUT_MS);

      socket.onopen = () => {
        clearTimeout(openTimer);
        if (this.closedByUs) return;
        // The relay routes by conn_id, so the open frame must precede the
        // first Noise packet on the same socket.
        if (this.isRelay(index)) this.sendRelayOpen(socket);
        try {
          this.wasm?.path_open(index, nowMs());
        } catch (error) {
          this.reportFailure(error);
          return;
        }
        const queued = this.outbound[index]?.splice(0) ?? [];
        for (const packet of queued) this.sendPathPacket(index, socket, packet);
      };
      socket.onmessage = (message) => {
        if (this.closedByUs) return;
        if (this.isRelay(index)) {
          if (typeof message.data !== "string") {
            this.handlers?.onError("The relay sent a binary frame; only JSON is expected.");
            return;
          }
          this.handleRelayEvent(index, socket, message.data);
          return;
        }
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
        clearTimeout(openTimer);
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
      this.sendPathPacket(index, socket, bytes);
    } else {
      this.outbound[index]?.push(bytes);
    }
  }

  private isRelay(index: number): boolean {
    return this.paths[index]?.kind === "relay";
  }

  /** Writes one Noise packet directly or inside a relay `data` frame. */
  private sendPathPacket(index: number, socket: WebSocket, bytes: Uint8Array): void {
    if (!this.isRelay(index)) {
      sendSocket(socket, bytes);
      return;
    }
    try {
      socket.send(
        JSON.stringify({
          conn_id: this.relayConnId ?? "",
          kind: "data",
          payload: encodeBase64Url(bytes),
        }),
      );
    } catch {
      // A racing close is handled by `onclose`.
    }
  }

  private sendRelayOpen(socket: WebSocket): void {
    const machineId = this.options.remote?.machineId ?? "";
    const payload = encodeBase64Url(
      new TextEncoder().encode(JSON.stringify({ machine_id: machineId })),
    );
    try {
      socket.send(JSON.stringify({ conn_id: this.relayConnId ?? "", kind: "open", payload }));
    } catch {
      // The socket may have closed immediately; `onclose` will report it.
    }
  }

  private handleRelayEvent(index: number, socket: WebSocket, text: string): void {
    type RelayEvent = {
      type?: string;
      frame?: { conn_id?: string; kind?: string; payload?: string };
      error?: { code?: string; message?: string; hint?: string | null };
    };
    let event: RelayEvent;
    try {
      event = JSON.parse(text) as RelayEvent;
    } catch {
      this.handlers?.onError("The relay sent a frame that was not JSON.");
      return;
    }
    if (event.type === "error") {
      this.handlers?.onError(
        event.error?.hint ?? event.error?.message ?? "The relay reported an error.",
      );
      return;
    }
    if (event.type !== "frame" || !event.frame) return;
    if (event.frame.conn_id && event.frame.conn_id !== this.relayConnId) return;
    switch (event.frame.kind) {
      case "data": {
        const bytes = decodeBase64Url(event.frame.payload ?? "");
        if (!bytes) {
          this.handlers?.onError("The relay sent an invalid data payload.");
          return;
        }
        const buffer = bytes.buffer.slice(
          bytes.byteOffset,
          bytes.byteOffset + bytes.byteLength,
        ) as ArrayBuffer;
        this.receive(index, buffer);
        break;
      }
      case "close":
        try {
          socket.close(1000, "relay closed");
        } catch {
          // Already gone.
        }
        break;
      default:
        // `ping` is a relay keepalive, not a transport event.
        break;
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
    // `degraded` means the session was established but no path is usable any
    // more. Nothing in this adapter reopens a socket, so hand the session back
    // to `H4rnClient` to reconnect with fresh paths instead of hanging.
    const lost = status.state === "offline" || status.state === "degraded";
    queueMicrotask(() => {
      if (!this.notifiedClose) handlers?.onTransportStatus?.(status);
    });
    if (lost && !this.notifiedClose) {
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
      void writeJson(PIN_KEY, this.pinned);
      forgetSecret(PENDING_KEY);
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
      forgetSecret(this.sessionKey);
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
    void writeJson(this.sessionKey, session);
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
  const raw = peekSecret(PENDING_KEY);
  if (!raw) return false;
  try {
    return Boolean(JSON.parse(raw));
  } catch {
    return false;
  }
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

/**
 * The stable device identity used for Noise and DPoP.
 *
 * It is generated once and kept in the platform secret store. Only the wasm
 * module ever sees the secret halves.
 */
export async function ensureDeviceIdentity(): Promise<string> {
  const existing = await getSecret(IDENTITY_KEY);
  if (existing) return existing;
  const identity = generate_identity();
  await setSecret(IDENTITY_KEY, identity);
  return identity;
}

/** URL-safe base64 without padding, matching the relay's wire format. */
export function encodeBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Decodes URL-safe base64 without padding; `null` means malformed. */
export function decodeBase64Url(value: string): Uint8Array | null {
  try {
    const padded = value.replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(padded.padEnd(Math.ceil(padded.length / 4) * 4, "="));
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  } catch {
    return null;
  }
}

function randomHex(bytes: number): string {
  const buffer = new Uint8Array(bytes);
  if (typeof crypto !== "undefined" && crypto.getRandomValues) {
    crypto.getRandomValues(buffer);
  } else {
    for (let index = 0; index < bytes; index += 1) {
      buffer[index] = Math.floor(Math.random() * 256);
    }
  }
  return [...buffer].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function readJson<T>(key: string): Promise<T | null> {
  const value = await getSecret(key);
  if (!value) return null;
  try {
    return JSON.parse(value) as T;
  } catch {
    return null;
  }
}

async function writeJson(key: string, value: unknown): Promise<void> {
  try {
    await setSecret(key, JSON.stringify(value));
  } catch {
    // A rejecting store leaves the in-memory cache serving this page load.
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
