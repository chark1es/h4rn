import type {
  ClientFrame,
  ClientRequest,
  DeviceInfo,
  Event,
  ResponseBody,
  ServerFrame,
  TerminalExitNotice,
  TransportStatus,
} from "@/gen";

import { H4rnRequestError, protoError } from "./errors";
import { emitProxyFrame, emitUsageFrame } from "./frames";
import type {
  ClientOptions,
  ConnectionSnapshot,
  EventEnvelope,
  ReadyInfo,
  Transport,
  TransportHandlers,
} from "./types";
import { WebSocketTransport } from "./ws";

export type ResponseOf<T extends ClientRequest["type"]> = T extends "hello"
  ? Extract<ResponseBody, { type: "hello" }>
  : T extends "list_hosts"
    ? Extract<ResponseBody, { type: "hosts" }>
    : T extends "list_threads"
      ? Extract<ResponseBody, { type: "threads" }>
      : T extends "get_thread"
        ? Extract<ResponseBody, { type: "thread" }>
        : T extends "create_thread"
          ? Extract<ResponseBody, { type: "thread_created" }>
          : T extends "send_message"
            ? Extract<ResponseBody, { type: "message_accepted" }>
            : T extends "terminal_open"
              ? Extract<ResponseBody, { type: "terminal_opened" }>
              : T extends "terminal_attach"
                ? Extract<ResponseBody, { type: "terminal_attached" }>
                : T extends "terminal_list"
                  ? Extract<ResponseBody, { type: "terminals" }>
                  : T extends "fs_list"
                    ? Extract<ResponseBody, { type: "fs_listing" }>
                    : T extends "fs_read"
                      ? Extract<ResponseBody, { type: "fs_file" }>
                      : T extends "fs_write"
                        ? Extract<ResponseBody, { type: "fs_written" }>
                        : T extends "fs_create" | "fs_rename"
                          ? Extract<ResponseBody, { type: "fs_entry_created" }>
                          : T extends "fs_delete"
                            ? Extract<ResponseBody, { type: "fs_deleted" }>
                            : T extends "fs_restore"
                              ? Extract<ResponseBody, { type: "fs_restored" }>
                              : T extends "fs_search"
                                ? Extract<ResponseBody, { type: "fs_search_started" }>
                                : T extends "fs_watch"
                                  ? Extract<ResponseBody, { type: "fs_watching" }>
                                  : T extends "fs_diff"
                                    ? Extract<ResponseBody, { type: "fs_diff" }>
                                    : T extends "fs_turn_changes"
                                      ? Extract<ResponseBody, { type: "fs_turn_changes" }>
                                      : T extends "begin_upload"
                                        ? Extract<ResponseBody, { type: "upload_started" }>
                                        : T extends "commit_upload"
                                          ? Extract<ResponseBody, { type: "upload_committed" }>
                                          : T extends "query_upload"
                                            ? Extract<ResponseBody, { type: "upload_status" }>
                                            : T extends "get_blob"
                                              ? Extract<ResponseBody, { type: "blob" }>
                                              : T extends "mcp"
                                                ? ResponseBody
                                                : T extends "mcp_list"
                                                  ? Extract<ResponseBody, { type: "mcp_servers" }>
                                                  : T extends "mcp_upsert"
                                                    ? Extract<ResponseBody, { type: "mcp_server" }>
                                                    : T extends "skills_list"
                                                      ? Extract<ResponseBody, { type: "skills" }>
                                                      : T extends
                                                            | "skills_add"
                                                            | "skills_update"
                                                            | "skills_set_enabled"
                                                        ? Extract<
                                                            ResponseBody,
                                                            { type: "skill_added" }
                                                          >
                                                        : T extends
                                                              | "skills_import_preview"
                                                              | "skills_conflicts"
                                                              | "skills_diff"
                                                              | "skills_sync_now"
                                                          ? ResponseBody
                                                          : T extends "skills_import_commit"
                                                            ? Extract<ResponseBody, { type: "ack" }>
                                                            : T extends "get_usage"
                                                  ? Extract<ResponseBody, { type: "usage_report" }>
                                                  : T extends "get_limits"
                                                    ? Extract<ResponseBody, { type: "limits" }>
                                                    : T extends "list_resets"
                                                      ? Extract<ResponseBody, { type: "resets" }>
                                                      : T extends "cancel_resume" | "resume_now"
                                                        ? Extract<
                                                            ResponseBody,
                                                            { type: "resume_changed" }
                                                          >
                                                        : T extends
                                                              | "list_proxy_profiles"
                                                              | "remove_proxy_profile"
                                                          ? Extract<
                                                              ResponseBody,
                                                              { type: "proxy_profiles" }
                                                            >
                                                          : T extends
                                                                | "add_proxy_profile"
                                                                | "update_proxy_profile"
                                                                | "set_proxy_harness_override"
                                                            ? Extract<
                                                                ResponseBody,
                                                                { type: "proxy_profile" }
                                                              >
                                                            : T extends "test_proxy_profile"
                                                              ? Extract<
                                                                  ResponseBody,
                                                                  { type: "proxy_tested" }
                                                                >
                                                              : T extends "list_proxy_models"
                                                                ? Extract<
                                                                    ResponseBody,
                                                                    { type: "proxy_models" }
                                                                  >
                                                                : T extends "list_proxy_limits"
                                                                  ? Extract<
                                                                      ResponseBody,
                                                                      { type: "proxy_limits" }
                                                                    >
                                                                  : T extends "pick_proxy_failover"
                                                                    ? Extract<
                                                                        ResponseBody,
                                                                        { type: "proxy_failover" }
                                                                      >
                                                                  : Extract<
                                                                      ResponseBody,
                                                                      { type: "ack" }
                                                                    >;

/**
 * Events that carry no durable position: `seq` is the daemon's current
 * watermark, not a new record, so they must not advance the client cursor or
 * be dropped by sequence deduplication.
 */
export function isTransientEvent(event: Event): boolean {
  return event.type === "fs_changed" || event.type === "fs_search_batch";
}

type Pending = {
  id: number;
  body: ClientRequest;
  resolve: (body: ResponseBody) => void;
  reject: (error: H4rnRequestError) => void;
  timer: ReturnType<typeof setTimeout>;
};

const DEFAULT_DEVICE: DeviceInfo = {
  device_id: "web",
  name: "h4rn web",
  platform: "web",
  app_version: "0.1.0",
};

const FATAL_CLOSE_CODES = new Map<number, { message: string; hint: string }>([
  [4401, { message: "The daemon rejected this device token.", hint: "Pair this device again." }],
  [
    4403,
    { message: "This device is not allowed to do that.", hint: "Ask the host owner for access." },
  ],
]);

/**
 * The protocol client: request/response correlation, ordered event delivery,
 * and reconnection with exponential backoff and resume metadata.
 *
 * All protocol knowledge lives here and in `projection.ts`; components only
 * consume the store.
 */
export class H4rnClient {
  private readonly options: Required<
    Pick<ClientOptions, "url" | "transportLabel" | "requestTimeoutMs">
  > &
    ClientOptions;
  private readonly device: DeviceInfo;
  private readonly statusListeners = new Set<(snapshot: ConnectionSnapshot) => void>();
  private readonly eventListeners = new Set<(event: EventEnvelope) => void>();
  private readonly binaryListeners = new Set<(data: ArrayBuffer) => void>();
  private readonly terminalListeners = new Set<(notice: TerminalExitNotice) => void>();
  private readonly readyListeners = new Set<(info: ReadyInfo) => void>();
  private readonly pending = new Map<number, Pending>();

  private transport: Transport | null = null;
  private frameId = 0;
  private lastSeq = 0;
  /** Events between socket open and the subscribe reply, sorted on flush. */
  private replaying = false;
  private replayBuffer: EventEnvelope[] = [];
  private attempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private stopped = false;
  private fatal = false;
  private socketOpen = false;
  private everReady = false;
  private snapshot: ConnectionSnapshot;
  private readonly transportFactory: (context: {
    url: string;
    token: string | null;
    attempt: number;
  }) => Transport;

  constructor(options: ClientOptions = {}) {
    this.options = {
      ...options,
      url: options.url ?? "ws://127.0.0.1:47800/ws",
      transportLabel: options.transportLabel ?? "direct",
      requestTimeoutMs: options.requestTimeoutMs ?? 10_000,
    };
    this.device = options.device ?? DEFAULT_DEVICE;
    this.transportFactory =
      options.transportFactory ?? ((context) => new WebSocketTransport(context.url, context.token));
    this.snapshot = {
      status: "connecting",
      attempt: 0,
      transport: this.options.transportLabel,
      lastConnectedAt: null,
      lastEventAt: null,
      latencyMs: null,
      error: null,
    };
  }

  start(): void {
    this.stopped = false;
    this.fatal = false;
    this.setSnapshot({ status: "connecting", attempt: 0, error: null });
    this.openSocket();
  }

  stop(): void {
    this.stopped = true;
    this.clearReconnect();
    this.stopPing();
    this.rejectPending(
      new H4rnRequestError(
        protoError("internal", "The client stopped.", "Start it again to reconnect."),
      ),
    );
    this.transport?.close();
    this.transport = null;
    this.socketOpen = false;
    this.setSnapshot({ status: "offline" });
  }

  /** Reset the backoff budget after the user asks to retry. */
  reconnectNow(): void {
    this.clearReconnect();
    this.stopped = false;
    this.fatal = false;
    this.attempt = 0;
    this.setSnapshot({ status: "connecting", attempt: 0, error: null });
    if (this.socketOpen) {
      void this.performHello();
      return;
    }
    this.openSocket();
  }

  getSnapshot(): ConnectionSnapshot {
    return this.snapshot;
  }

  onStatus(listener: (snapshot: ConnectionSnapshot) => void): () => void {
    this.statusListeners.add(listener);
    return () => this.statusListeners.delete(listener);
  }

  onEvent(listener: (event: EventEnvelope) => void): () => void {
    this.eventListeners.add(listener);
    return () => this.eventListeners.delete(listener);
  }

  /**
   * Delivers raw binary frames (PTY output, blob chunks, acks).
   *
   * The protocol header is feature-owned so the client stays a byte pipe; see
   * `lib/client/binary.ts` and `features/attachments/frames.ts` for decoders.
   */
  onBinary(listener: (data: ArrayBuffer) => void): () => void {
    this.binaryListeners.add(listener);
    return () => this.binaryListeners.delete(listener);
  }

  /** Subscribes to ephemeral terminal exit notices. */
  onTerminalEvent(listener: (notice: TerminalExitNotice) => void): () => void {
    this.terminalListeners.add(listener);
    return () => this.terminalListeners.delete(listener);
  }

  /** Sends one binary frame, such as PTY input or a blob chunk. */
  sendBinary(data: Uint8Array | ArrayBuffer): void {
    if (!this.socketOpen || !this.transport) {
      throw new H4rnRequestError(
        protoError(
          "internal",
          "The daemon is not connected.",
          "Wait until the status bar says online, then try again.",
        ),
      );
    }
    this.transport.send(data);
  }

  onReady(listener: (info: ReadyInfo) => void): () => void {
    this.readyListeners.add(listener);
    return () => this.readyListeners.delete(listener);
  }

  async request<T extends ClientRequest["type"]>(
    body: Extract<ClientRequest, { type: T }>,
    options: { timeoutMs?: number } = {},
  ): Promise<ResponseOf<T>> {
    if (!this.socketOpen || !this.transport) {
      throw new H4rnRequestError(
        protoError(
          "internal",
          "The daemon is not connected.",
          "Wait until the status bar says online, then try again.",
        ),
      );
    }

    const id = ++this.frameId;
    const frame: ClientFrame = { id, body };
    const timeoutMs = options.timeoutMs ?? this.options.requestTimeoutMs;
    const pending: Pending = {
      id,
      body,
      resolve: () => undefined,
      reject: () => undefined,
      timer: setTimeout(() => undefined, 0),
    };
    clearTimeout(pending.timer);

    const promise = new Promise<ResponseBody>((resolve, reject) => {
      pending.resolve = resolve;
      pending.reject = reject;
      pending.timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new H4rnRequestError(
            protoError(
              "internal",
              `The daemon did not answer \`${body.type}\` within ${Math.round(timeoutMs / 1000)} s.`,
              "The daemon may be busy. Try again, or check the connection in the status bar.",
            ),
            id,
          ),
        );
      }, timeoutMs);
    });

    this.pending.set(id, pending);

    try {
      this.transport.send(JSON.stringify(frame));
    } catch (error) {
      clearTimeout(pending.timer);
      this.pending.delete(id);
      throw new H4rnRequestError(
        protoError(
          "internal",
          error instanceof Error ? error.message : "The frame could not be sent.",
          "Wait for the connection to recover, then try again.",
        ),
        id,
      );
    }

    return (await promise) as ResponseOf<T>;
  }

  // ---------------------------------------------------------------------
  // Socket lifecycle
  // ---------------------------------------------------------------------

  private openSocket(): void {
    if (this.stopped || this.fatal) return;

    this.socketOpen = false;
    const handlers: TransportHandlers = {
      onOpen: () => {
        this.socketOpen = true;
        // Keep `lastSeq` across reconnects so the subscribe replay resumes
        // exactly where the previous socket stopped.
        this.replaying = true;
        this.replayBuffer = [];
        void this.performHello();
      },
      onMessage: (data) => this.handleMessage(data),
      onBinary: (data) => this.handleBinary(data),
      onClose: (frame) => this.handleClose(frame.code, frame.reason),
      onTransportStatus: (status) => this.handleTransportStatus(status),
      onError: (message) => {
        this.patchSnapshot({
          error: protoError(
            "internal",
            message,
            "Check that the daemon is running, then reconnect.",
          ),
        });
      },
    };

    try {
      this.transport = this.transportFactory({
        url: this.options.url,
        token: this.options.token ?? null,
        attempt: this.attempt,
      });
      this.transport.connect(handlers);
    } catch (error) {
      this.patchSnapshot({
        error: protoError(
          "internal",
          error instanceof Error ? error.message : "The transport could not be created.",
          "Check the daemon URL and try again.",
        ),
      });
      this.scheduleReconnect();
    }
  }

  private async performHello(): Promise<void> {
    try {
      const hello = await this.request(
        {
          type: "hello",
          protocol_version: 1,
          device: this.device,
        },
        { timeoutMs: 6_000 },
      );

      if (hello.type !== "hello") {
        throw new H4rnRequestError(
          protoError(
            "internal",
            "The daemon answered hello with an unexpected response.",
            "Update h4rn on both ends.",
          ),
        );
      }

      const reconnected = this.everReady;
      this.everReady = true;
      this.attempt = 0;
      this.stopPing();
      this.setSnapshot({
        status: "online",
        attempt: 0,
        lastConnectedAt: Date.now(),
        error: null,
      });
      this.startPing();

      try {
        // On the first connection `lastSeq` is 0, so start at the live edge;
        // after a reconnect ask for the gap and let the daemon replay it
        // before going live (ADR-003).
        await this.request(
          {
            type: "subscribe",
            topics: ["all"],
            since_seq: this.lastSeq > 0 ? this.lastSeq : null,
          },
          { timeoutMs: 6_000 },
        );
      } catch {
        // Subscriptions are best-effort; list and detail requests still work.
      } finally {
        this.flushReplay();
      }

      const ready: ReadyInfo = {
        reconnected,
        hosts: hello.hosts,
        localHostId: hello.local_host,
        serverVersion: hello.server_version,
        deviceId: hello.device_id,
      };
      for (const listener of this.readyListeners) listener(ready);
    } catch (error) {
      const proto = error instanceof H4rnRequestError ? error.proto : null;
      if (proto?.code === "unauthorized" || proto?.code === "forbidden") {
        this.fatal = true;
        this.setSnapshot({ status: "offline", error: proto });
        this.transport?.close();
        return;
      }
      this.patchSnapshot({
        error:
          proto ??
          protoError(
            "internal",
            "The hello handshake did not complete.",
            "Check the daemon version, then reconnect.",
          ),
      });
      this.transport?.close();
    }
  }

  /** Surfaces the active secure path (and its RTT) in the status bar. */
  private handleTransportStatus(status: TransportStatus): void {
    const active = status.active;
    this.patchSnapshot({
      transport: active?.kind ?? "encrypted",
      latencyMs: active?.rtt_ms ?? null,
    });
  }

  private handleBinary(data: ArrayBuffer): void {
    // PTY chunks and blob chunks can arrive thousands of times a second; they
    // never change connection state, so only touch the activity timestamp.
    this.patchSnapshot({ lastEventAt: Date.now() });
    for (const listener of this.binaryListeners) listener(data);
  }

  private handleMessage(data: string): void {
    this.patchSnapshot({ lastEventAt: Date.now() });

    let frame: ServerFrame;
    try {
      frame = JSON.parse(data) as ServerFrame;
    } catch {
      this.patchSnapshot({
        error: protoError(
          "internal",
          "The daemon sent a frame that was not JSON.",
          "Update h4rn on both ends; the connection will retry.",
        ),
      });
      return;
    }

    if (frame.kind === "response") {
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      clearTimeout(pending.timer);
      this.pending.delete(frame.id);
      if ("Ok" in frame.result) {
        pending.resolve(frame.result.Ok);
      } else {
        pending.reject(new H4rnRequestError(frame.result.Err, frame.id));
      }
      return;
    }

    if (frame.kind === "terminal_event") {
      for (const listener of this.terminalListeners) listener(frame.event);
      return;
    }

    // Usage ticks and proxy changes are ephemeral by design: they have no
    // sequence number, so they never advance `lastSeq` or enter the replay.
    if (frame.kind === "usage_event") {
      emitUsageFrame(frame.event);
      return;
    }

    if (frame.kind === "proxy_event") {
      emitProxyFrame(frame.event);
      return;
    }

    if (frame.kind === "event") {
      if (isTransientEvent(frame.event)) {
        // File-watch hints and streamed search batches reuse the current
        // watermark; deliver them live without moving `lastSeq`.
        for (const listener of this.eventListeners) {
          listener({ seq: frame.seq, event: frame.event });
        }
        return;
      }
      if (frame.seq <= this.lastSeq) return;
      if (this.replaying) {
        // Hold replay frames until the subscribe reply marks the gap closed,
        // then deliver them in sequence order.
        this.replayBuffer.push({ seq: frame.seq, event: frame.event });
        return;
      }
      this.lastSeq = frame.seq;
      for (const listener of this.eventListeners) {
        listener({ seq: frame.seq, event: frame.event });
      }
    }
  }

  private flushReplay(): void {
    if (!this.replaying) return;
    this.replaying = false;
    const pending = this.replayBuffer.splice(0).sort((left, right) => left.seq - right.seq);
    for (const envelope of pending) {
      if (envelope.seq <= this.lastSeq) continue;
      this.lastSeq = envelope.seq;
      for (const listener of this.eventListeners) {
        listener(envelope);
      }
    }
  }

  private handleClose(code: number, reason: string): void {
    this.socketOpen = false;
    this.transport = null;
    this.stopPing();
    this.rejectPending(
      new H4rnRequestError(
        protoError(
          "internal",
          reason || "The daemon closed the connection.",
          "The client reconnects automatically. Requests sent during the gap are not retried.",
        ),
      ),
    );

    const fatal = FATAL_CLOSE_CODES.get(code);
    if (fatal) {
      this.fatal = true;
      this.setSnapshot({
        status: "offline",
        error: protoError(code === 4401 ? "unauthorized" : "forbidden", fatal.message, fatal.hint),
      });
      return;
    }

    if (this.stopped) return;
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.fatal || this.reconnectTimer) return;

    this.attempt += 1;
    const maxAttempts = this.options.reconnect?.maxAttempts ?? 12;
    if (this.attempt > maxAttempts) {
      this.setSnapshot({
        status: "offline",
        attempt: this.attempt,
        error:
          this.snapshot.error ??
          protoError(
            "internal",
            "The daemon has not answered after repeated attempts.",
            "Start the daemon or check the network, then press Reconnect.",
          ),
      });
      return;
    }

    this.setSnapshot({
      status: "degraded",
      attempt: this.attempt,
      latencyMs: null,
    });

    const base = this.options.reconnect?.baseMs ?? 500;
    const max = this.options.reconnect?.maxMs ?? 15_000;
    const jitter = this.options.reconnect?.jitter ?? 0.3;
    const nominal = Math.min(max, base * 2 ** (this.attempt - 1));
    const delay = Math.round(nominal * (1 + (Math.random() * 2 - 1) * jitter));

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.openSocket();
    }, delay);
  }

  private clearReconnect(): void {
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private startPing(): void {
    this.stopPing();
    this.pingTimer = setInterval(() => {
      if (!this.socketOpen) return;
      const started = Date.now();
      void this.request({ type: "list_hosts" }, { timeoutMs: 5_000 })
        .then(() => {
          this.patchSnapshot({ latencyMs: Date.now() - started });
        })
        .catch(() => {
          this.patchSnapshot({ latencyMs: null });
        });
    }, 15_000);
  }

  private stopPing(): void {
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.pingTimer = null;
  }

  private rejectPending(error: H4rnRequestError): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }

  private setSnapshot(next: Partial<ConnectionSnapshot>): void {
    this.snapshot = { ...this.snapshot, ...next };
    for (const listener of this.statusListeners) listener(this.snapshot);
  }

  private patchSnapshot(next: Partial<ConnectionSnapshot>): void {
    const changed = (Object.keys(next) as Array<keyof ConnectionSnapshot>).some(
      (key) => this.snapshot[key] !== next[key],
    );
    if (!changed) return;
    this.setSnapshot(next);
  }
}

export function createH4rnClient(options: ClientOptions): H4rnClient {
  return new H4rnClient(options);
}
