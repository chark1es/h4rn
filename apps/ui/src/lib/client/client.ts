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
import {
  emitHarnessFrame,
  emitImportFrame,
  emitProxyFrame,
  emitScheduleFrame,
  emitUsageFrame,
} from "./frames";
import type {
  ClientOptions,
  ConnectionSnapshot,
  EventEnvelope,
  ReadyInfo,
  Transport,
  TransportHandlers,
} from "./types";
import { WebSocketTransport } from "./ws";

type ResponseOfRest<T extends ClientRequest["type"]> = T extends "hello"
  ? Extract<ResponseBody, { type: "hello" }>
  : T extends "subscribe"
    ? Extract<ResponseBody, { type: "subscribed" }>
    : T extends "list_harness_versions"
      ? Extract<ResponseBody, { type: "harness_versions" }>
      : T extends "pairing_confirmations"
        ? Extract<ResponseBody, { type: "pairing_confirmations" }>
        : T extends "list_hosts"
          ? Extract<ResponseBody, { type: "hosts" }>
          : T extends "list_threads"
            ? Extract<ResponseBody, { type: "threads" }>
            : T extends "get_thread"
              ? Extract<ResponseBody, { type: "thread" }>
              : T extends "create_thread"
                ? Extract<ResponseBody, { type: "thread_created" }>
                : T extends "fork_thread"
                  ? Extract<ResponseBody, { type: "thread_forked" }>
                  : T extends "send_message"
                    ? Extract<ResponseBody, { type: "message_accepted" }>
                    : T extends "rewind_thread"
                      ? Extract<ResponseBody, { type: "rewound" }>
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
                                                : T extends "fs_diff_files"
                                                  ? Extract<ResponseBody, { type: "fs_diff_files" }>
                                                  : T extends "fs_diff_commits"
                                                    ? Extract<
                                                        ResponseBody,
                                                        { type: "fs_diff_commits" }
                                                      >
                                                    : T extends "fs_branches"
                                                      ? Extract<
                                                          ResponseBody,
                                                          { type: "fs_branches" }
                                                        >
                                                      : T extends "begin_upload"
                                                        ? Extract<
                                                            ResponseBody,
                                                            { type: "upload_started" }
                                                          >
                                                        : T extends "commit_upload"
                                                          ? Extract<
                                                              ResponseBody,
                                                              { type: "upload_committed" }
                                                            >
                                                          : T extends "query_upload"
                                                            ? Extract<
                                                                ResponseBody,
                                                                { type: "upload_status" }
                                                              >
                                                            : T extends "get_blob"
                                                              ? Extract<
                                                                  ResponseBody,
                                                                  { type: "blob" }
                                                                >
                                                              : T extends "subagent"
                                                                ? Extract<
                                                                    ResponseBody,
                                                                    | { type: "subagents" }
                                                                    | { type: "subagent" }
                                                                  >
                                                                : T extends "mcp"
                                                                  ? ResponseBody
                                                                  : T extends "mcp_list"
                                                                    ? Extract<
                                                                        ResponseBody,
                                                                        { type: "mcp_servers" }
                                                                      >
                                                                    : T extends "mcp_upsert"
                                                                      ? Extract<
                                                                          ResponseBody,
                                                                          { type: "mcp_server" }
                                                                        >
                                                                      : T extends "skills_list"
                                                                        ? Extract<
                                                                            ResponseBody,
                                                                            { type: "skills" }
                                                                          >
                                                                        : T extends
                                                                              | "skills_add"
                                                                              | "skills_update"
                                                                              | "skills_set_enabled"
                                                                          ? Extract<
                                                                              ResponseBody,
                                                                              {
                                                                                type: "skill_added";
                                                                              }
                                                                            >
                                                                          : T extends
                                                                                | "skills_import_preview"
                                                                                | "skills_conflicts"
                                                                                | "skills_diff"
                                                                                | "skills_sync_now"
                                                                            ? ResponseBody
                                                                            : T extends "skills_import_commit"
                                                                              ? Extract<
                                                                                  ResponseBody,
                                                                                  { type: "ack" }
                                                                                >
                                                                              : T extends "get_usage"
                                                                                ? Extract<
                                                                                    ResponseBody,
                                                                                    {
                                                                                      type: "usage_report";
                                                                                    }
                                                                                  >
                                                                                : T extends "get_limits"
                                                                                  ? Extract<
                                                                                      ResponseBody,
                                                                                      {
                                                                                        type: "limits";
                                                                                      }
                                                                                    >
                                                                                  : T extends "list_failover_options"
                                                                                    ? Extract<
                                                                                        ResponseBody,
                                                                                        {
                                                                                          type: "failover_options";
                                                                                        }
                                                                                      >
                                                                                    : T extends "list_resets"
                                                                                      ? Extract<
                                                                                          ResponseBody,
                                                                                          {
                                                                                            type: "resets";
                                                                                          }
                                                                                        >
                                                                                      : T extends
                                                                                            | "cancel_resume"
                                                                                            | "fail_over"
                                                                                            | "resume_now"
                                                                                        ? Extract<
                                                                                            ResponseBody,
                                                                                            {
                                                                                              type: "resume_changed";
                                                                                            }
                                                                                          >
                                                                                        : T extends
                                                                                              | "list_proxy_profiles"
                                                                                              | "remove_proxy_profile"
                                                                                          ? Extract<
                                                                                              ResponseBody,
                                                                                              {
                                                                                                type: "proxy_profiles";
                                                                                              }
                                                                                            >
                                                                                          : T extends
                                                                                                | "add_proxy_profile"
                                                                                                | "update_proxy_profile"
                                                                                                | "set_proxy_harness_override"
                                                                                            ? Extract<
                                                                                                ResponseBody,
                                                                                                {
                                                                                                  type: "proxy_profile";
                                                                                                }
                                                                                              >
                                                                                            : T extends "test_proxy_profile"
                                                                                              ? Extract<
                                                                                                  ResponseBody,
                                                                                                  {
                                                                                                    type: "proxy_tested";
                                                                                                  }
                                                                                                >
                                                                                              : T extends "list_proxy_models"
                                                                                                ? Extract<
                                                                                                    ResponseBody,
                                                                                                    {
                                                                                                      type: "proxy_models";
                                                                                                    }
                                                                                                  >
                                                                                                : T extends "apply_proxy_overrides"
                                                                                                  ? Extract<
                                                                                                      ResponseBody,
                                                                                                      {
                                                                                                        type: "proxy_applied";
                                                                                                      }
                                                                                                    >
                                                                                                  : T extends "list_proxy_limits"
                                                                                                    ? Extract<
                                                                                                        ResponseBody,
                                                                                                        {
                                                                                                          type: "proxy_limits";
                                                                                                        }
                                                                                                      >
                                                                                                    : T extends "pick_proxy_failover"
                                                                                                      ? Extract<
                                                                                                          ResponseBody,
                                                                                                          {
                                                                                                            type: "proxy_failover";
                                                                                                          }
                                                                                                        >
                                                                                                      : T extends "list_text_generators"
                                                                                                        ? Extract<
                                                                                                            ResponseBody,
                                                                                                            {
                                                                                                              type: "text_generators";
                                                                                                            }
                                                                                                          >
                                                                                                        : T extends
                                                                                                              | "add_text_generator"
                                                                                                              | "update_text_generator"
                                                                                                          ? Extract<
                                                                                                              ResponseBody,
                                                                                                              {
                                                                                                                type: "text_generator";
                                                                                                              }
                                                                                                            >
                                                                                                          : T extends "test_text_generator"
                                                                                                            ? Extract<
                                                                                                                ResponseBody,
                                                                                                                {
                                                                                                                  type: "text_generator_tested";
                                                                                                                }
                                                                                                              >
                                                                                                            : T extends
                                                                                                                  | "get_text_generation_settings"
                                                                                                                  | "set_text_generation_settings"
                                                                                                              ? Extract<
                                                                                                                  ResponseBody,
                                                                                                                  {
                                                                                                                    type: "text_generation_settings";
                                                                                                                  }
                                                                                                                >
                                                                                                              : T extends
                                                                                                                    | "get_guest_sharing"
                                                                                                                    | "share_thread_with_guest"
                                                                                                                ? Extract<
                                                                                                                    ResponseBody,
                                                                                                                    {
                                                                                                                      type: "guest_sharing";
                                                                                                                    }
                                                                                                                  >
                                                                                                                : T extends
                                                                                                                      | "get_guest_settings"
                                                                                                                      | "set_guest_settings"
                                                                                                                  ? Extract<
                                                                                                                      ResponseBody,
                                                                                                                      {
                                                                                                                        type: "guest_settings";
                                                                                                                      }
                                                                                                                    >
                                                                                                                  : T extends "rename_thread"
                                                                                                                    ? Extract<
                                                                                                                        ResponseBody,
                                                                                                                        {
                                                                                                                          type: "thread_renamed";
                                                                                                                        }
                                                                                                                      >
                                                                                                                    : T extends
                                                                                                                          | "get_network_access"
                                                                                                                          | "set_network_access"
                                                                                                                      ? Extract<
                                                                                                                          ResponseBody,
                                                                                                                          {
                                                                                                                            type: "network_access";
                                                                                                                          }
                                                                                                                        >
                                                                                                                      : T extends
                                                                                                                            | "list_direct_devices"
                                                                                                                            | "revoke_direct_device"
                                                                                                                        ? Extract<
                                                                                                                            ResponseBody,
                                                                                                                            {
                                                                                                                              type: "direct_devices";
                                                                                                                            }
                                                                                                                          >
                                                                                                                        : T extends "create_direct_pairing"
                                                                                                                          ? Extract<
                                                                                                                              ResponseBody,
                                                                                                                              {
                                                                                                                                type: "direct_pairing_offer";
                                                                                                                              }
                                                                                                                            >
                                                                                                                          : T extends "get_acp_agent"
                                                                                                                            ? Extract<
                                                                                                                                ResponseBody,
                                                                                                                                {
                                                                                                                                  type: "acp_agent_spec";
                                                                                                                                }
                                                                                                                              >
                                                                                                                            : T extends "list_acp_agents"
                                                                                                                              ? Extract<
                                                                                                                                  ResponseBody,
                                                                                                                                  {
                                                                                                                                    type: "acp_agents";
                                                                                                                                  }
                                                                                                                                >
                                                                                                                              : T extends
                                                                                                                                    | "add_acp_agent"
                                                                                                                                    | "update_acp_agent"
                                                                                                                                ? Extract<
                                                                                                                                    ResponseBody,
                                                                                                                                    {
                                                                                                                                      type: "acp_agent";
                                                                                                                                    }
                                                                                                                                  >
                                                                                                                                : T extends "probe_acp_agent"
                                                                                                                                  ? Extract<
                                                                                                                                      ResponseBody,
                                                                                                                                      {
                                                                                                                                        type: "acp_agent_probe";
                                                                                                                                      }
                                                                                                                                    >
                                                                                                                                  : T extends
                                                                                                                                        | "queue_message"
                                                                                                                                        | "get_thread_queue"
                                                                                                                                    ? T extends "queue_message"
                                                                                                                                      ? Extract<
                                                                                                                                          ResponseBody,
                                                                                                                                          {
                                                                                                                                            type: "queue_updated";
                                                                                                                                          }
                                                                                                                                        >
                                                                                                                                      : Extract<
                                                                                                                                          ResponseBody,
                                                                                                                                          {
                                                                                                                                            type: "thread_queue";
                                                                                                                                          }
                                                                                                                                        >
                                                                                                                                    : Extract<
                                                                                                                                        ResponseBody,
                                                                                                                                        {
                                                                                                                                          type: "ack";
                                                                                                                                        }
                                                                                                                                      >;

export type ResponseOf<T extends ClientRequest["type"]> = T extends "list_worktrees"
  ? Extract<ResponseBody, { type: "worktrees" }>
  : T extends "clear_worktrees"
    ? Extract<ResponseBody, { type: "worktrees_cleared" }>
    : T extends "switch_thread_harness"
      ? Extract<ResponseBody, { type: "thread_switched" }>
      : T extends "get_worktree_settings" | "set_worktree_settings"
        ? Extract<ResponseBody, { type: "worktree_settings" }>
        : T extends "rewind_plan"
          ? Extract<ResponseBody, { type: "rewind_plan" }>
          : T extends "get_rewind_settings" | "set_rewind_settings_enabled"
            ? Extract<ResponseBody, { type: "rewind_settings" }>
            : T extends "get_agent_alert_settings" | "set_agent_alert_settings"
              ? Extract<ResponseBody, { type: "agent_alert_settings" }>
              : ResponseOfRest<T>;

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

/** Errors produced by a dropped connection, as opposed to a daemon answer. */
const DISCONNECTED = new WeakSet<H4rnRequestError>();

const READ_PREFIXES = ["list_", "get_", "read_", "search_"];

/** Read-only requests that are safe to send a second time. */
function isIdempotent(body: ClientRequest): boolean {
  if (body.type === "mcp_list") return true;
  if (body.type === "mcp") {
    const inner = (body as { request?: { type?: string } }).request?.type ?? "";
    return READ_PREFIXES.some((prefix) => inner.startsWith(prefix));
  }
  return READ_PREFIXES.some((prefix) => body.type.startsWith(prefix));
}

/**
 * A wake inside this window of a fresh connect attempt is ignored.
 *
 * Browser wakes arrive in bursts (online, then focus, then visibility), and
 * `retryImmediately` would otherwise tear down a socket that is still
 * opening and start the same attempt again.
 */
const WAKE_MIN_ATTEMPT_MS = 1_000;

/**
 * Characters of one outgoing frame sent per chunk.
 *
 * 96K characters stay below a framed transport's 512 KiB even when every one
 * is three UTF-8 bytes; JSON escaping in the envelope adds at most a factor of
 * two on ASCII text.
 */
const OUTBOUND_CHUNK_CHARS = 96 * 1024;

const DEFAULT_DEVICE: DeviceInfo = {
  device_id: "web",
  name: "h4rn web",
  platform: "web",
  app_version: "1.0.1",
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
/**
 * Splits `text` into pieces of at most `size` UTF-16 code units, never
 * between the two halves of a surrogate pair: a lone half would serialise as
 * an unpaired `\ud83d` escape that the daemon's JSON parser rejects.
 */
export function chunkText(text: string, size: number): Array<[number, string, boolean]> {
  const pieces: string[] = [];
  let start = 0;
  while (start < text.length) {
    let end = Math.min(text.length, start + size);
    if (end < text.length && end - start > 1) {
      const code = text.charCodeAt(end - 1);
      if (code >= 0xd800 && code <= 0xdbff) end -= 1;
    }
    pieces.push(text.slice(start, end));
    start = end;
  }
  return pieces.map((piece, index) => [index, piece, index === pieces.length - 1]);
}

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
  /** The server frame being joined from chunks, if any. */
  private receivedChunk: { index: number; text: string } | null = null;

  private transport: Transport | null = null;
  private frameId = 0;
  /**
   * Highest sequence actually delivered to listeners.
   *
   * Deduplication uses this, not the audited boundary: a frame can arrive
   * after the subscribe reply yet still be one the daemon queued for this
   * socket, and it must not be dropped just because the reply named a head
   * that covers it.
   */
  private lastSeq = 0;
  /**
   * Highest journal position the daemon has acknowledged serving (or skipping)
   * for this client.
   *
   * A first connect asks from the live edge, so the answer's head is history
   * this client deliberately did not want; remembering it separately keeps a
   * later reconnect from replaying that range, without using it to discard
   * frames that arrive live after the reply.
   */
  private ackSeq = 0;
  /** Events between socket open and the subscribe reply, sorted on flush. */
  private replaying = false;
  private replayBuffer: EventEnvelope[] = [];
  /** The id of the subscribe answer this socket is still owed, if any. */
  private subscribeId: number | null = null;
  /** `since_seq` of that subscribe; null means it started at the live edge. */
  private subscribeSince: number | null = null;
  private attempt = 0;
  /** Mockable wall-clock start of the in-flight connect attempt. */
  private attemptStartedAt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  /** Retry as soon as the browser says the network or the app is back. */
  private wakeListenersInstalled = false;
  private readonly onWake = (): void => this.retryImmediately();
  private readonly onVisibilityChange = (): void => {
    if (typeof document !== "undefined" && document.visibilityState === "visible") {
      this.retryImmediately();
    }
  };
  private stopped = false;
  private fatal = false;
  private socketOpen = false;
  /** True once hello and the subscribe replay finished on this socket. */
  private handshaken = false;
  private readonly handshakeWaiters = new Set<{
    resolve: () => void;
    reject: (error: H4rnRequestError) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();
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
    this.installWakeListeners();
    this.setSnapshot({ status: "connecting", attempt: 0, error: null });
    this.openSocket();
  }

  stop(): void {
    this.stopped = true;
    this.handshaken = false;
    this.removeWakeListeners();
    this.clearReconnect();
    this.stopPing();
    const stoppedError = new H4rnRequestError(
      protoError("internal", "The client stopped.", "Start it again to reconnect."),
    );
    this.settleHandshakeWaiters(stoppedError);
    this.rejectPending(stoppedError);
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
    // `stop()` removed the wake listeners; a manual Reconnect starts a live
    // client again, so put them back (installing twice is a no-op).
    this.installWakeListeners();
    this.attempt = 0;
    this.setSnapshot({ status: "connecting", attempt: 0, error: null });
    if (this.socketOpen) {
      void this.performHello();
      return;
    }
    this.transport?.close();
    this.transport = null;
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

  /**
   * Sends one JSON frame, split into `chunk` envelopes when it is too large
   * for one transport frame.
   *
   * A long prompt is the reason this exists: the paired transport refuses a
   * frame over 512 KiB and closes the session. The daemon joins the pieces
   * back into the exact original text and handles it once, so nothing is
   * shortened or sent twice.
   */
  private sendFrame(text: string, id: number): void {
    const transport = this.transport;
    if (!transport) return;
    if (text.length <= OUTBOUND_CHUNK_CHARS) {
      transport.send(text);
      return;
    }
    for (const [index, piece, last] of chunkText(text, OUTBOUND_CHUNK_CHARS)) {
      transport.send(JSON.stringify({ id, chunk: { index, last, data: piece } }));
    }
  }

  onReady(listener: (info: ReadyInfo) => void): () => void {
    this.readyListeners.add(listener);
    return () => this.readyListeners.delete(listener);
  }

  /**
   * Sends one request. Requests wait for the hello handshake (and its
   * subscribe replay) to finish instead of racing it, and an idempotent read
   * that is cut off by a dropped connection is sent once more after the
   * reconnect. Writes are never replayed: they may already have been applied.
   */
  async request<T extends ClientRequest["type"]>(
    body: Extract<ClientRequest, { type: T }>,
    options: { timeoutMs?: number } = {},
  ): Promise<ResponseOf<T>> {
    if (body.type === "hello" || body.type === "subscribe") {
      return this.dispatch(body, options);
    }
    const timeoutMs = options.timeoutMs ?? this.options.requestTimeoutMs;
    const deadline = Date.now() + timeoutMs;
    await this.untilHandshaken(timeoutMs);
    try {
      return await this.dispatch(body, { timeoutMs: Math.max(1, deadline - Date.now()) });
    } catch (error) {
      if (
        error instanceof H4rnRequestError &&
        DISCONNECTED.has(error) &&
        isIdempotent(body) &&
        deadline - Date.now() > 0
      ) {
        await this.untilHandshaken(Math.max(1, deadline - Date.now()));
        return this.dispatch(body, { timeoutMs: Math.max(1, deadline - Date.now()) });
      }
      throw error;
    }
  }

  private notConnected(): H4rnRequestError {
    return new H4rnRequestError(
      protoError(
        "internal",
        "The daemon is not connected.",
        "Wait until the status bar says online, then try again.",
      ),
    );
  }

  /** Resolves once the handshake is done; waits through connect and reconnect. */
  private untilHandshaken(timeoutMs: number): Promise<void> {
    if (this.handshaken && this.socketOpen && this.transport) return Promise.resolve();
    const status = this.snapshot.status;
    const recovering = this.socketOpen || status === "connecting" || status === "degraded";
    if (this.stopped || this.fatal || !recovering) return Promise.reject(this.notConnected());
    return new Promise<void>((resolve, reject) => {
      const waiter = {
        resolve: () => {
          clearTimeout(waiter.timer);
          this.handshakeWaiters.delete(waiter);
          resolve();
        },
        reject: (error: H4rnRequestError) => {
          clearTimeout(waiter.timer);
          this.handshakeWaiters.delete(waiter);
          reject(error);
        },
        timer: setTimeout(() => {
          waiter.reject(
            new H4rnRequestError(
              protoError(
                "internal",
                "The daemon connection did not finish its handshake in time.",
                "The daemon may be busy. Try again, or check the connection in the status bar.",
              ),
            ),
          );
        }, timeoutMs),
      };
      this.handshakeWaiters.add(waiter);
    });
  }

  private settleHandshakeWaiters(error: H4rnRequestError | null): void {
    for (const waiter of [...this.handshakeWaiters]) {
      if (error) waiter.reject(error);
      else waiter.resolve();
    }
  }

  private async dispatch<T extends ClientRequest["type"]>(
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
    if (body.type === "subscribe") {
      // Remember the request so a late answer (after the request's own
      // timeout) can still close the replay at the boundary it names.
      const subscribe = body as Extract<ClientRequest, { type: "subscribe" }>;
      this.subscribeId = id;
      this.subscribeSince = subscribe.since_seq ?? null;
    }
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
      this.sendFrame(JSON.stringify(frame), id);
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
    this.receivedChunk = null;
    this.attemptStartedAt = Date.now();
    const handlers: TransportHandlers = {
      onOpen: () => {
        this.socketOpen = true;
        // Keep the delivered and acknowledged cursors across reconnects so
        // the subscribe replay resumes exactly where the previous socket
        // stopped.
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
        // On the first connection nothing was delivered, so start at the live
        // edge; after a reconnect ask for the gap and let the daemon replay it
        // before going live (ADR-003). The answer names the journal head it
        // served, which is the only honest replay boundary: a live event can
        // arrive before this answer and must not be mistaken for history.
        const resumeFrom = Math.max(this.lastSeq, this.ackSeq);
        await this.request(
          {
            type: "subscribe",
            topics: ["all"],
            since_seq: resumeFrom > 0 ? resumeFrom : null,
          },
          { timeoutMs: 6_000 },
        );
      } catch {
        // The request timed out, but its answer may still arrive. Keep the
        // held frames buffered: `handleMessage` flushes them with the answer's
        // boundary, and a dropped socket discards them so the next subscribe
        // replays the same range. Reads and lists still work meanwhile.
      } finally {
        this.handshaken = true;
        this.settleHandshakeWaiters(null);
      }

      const ready: ReadyInfo = {
        reconnected,
        hosts: hello.hosts,
        localHostId: hello.local_host,
        daemonTimezone: hello.daemon_timezone ?? "",
        serverVersion: hello.server_version,
        deviceId: hello.device_id,
        // A daemon older than this field answers without it, and a signed-out
        // daemon has no machine to name.
        daemonMachineId: hello.daemon_machine_id ?? null,
      };
      for (const listener of this.readyListeners) listener(ready);
    } catch (error) {
      const proto = error instanceof H4rnRequestError ? error.proto : null;
      if (proto?.code === "unauthorized" || proto?.code === "forbidden") {
        this.fatal = true;
        this.settleHandshakeWaiters(new H4rnRequestError(proto));
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

  /**
   * Joins a frame the daemon split for the paired transport.
   *
   * A frame over one transport frame arrives as `chunk` envelopes; they carry
   * the exact text of the original frame, so joining them loses nothing. Any
   * other frame returns false and is parsed normally.
   */
  private receiveChunk(data: string): boolean {
    if (!data.includes('"chunk"')) return false;
    let frame: { kind?: unknown; index?: unknown; last?: unknown; data?: unknown };
    try {
      frame = JSON.parse(data) as typeof frame;
    } catch {
      return false;
    }
    if (
      frame.kind !== "chunk" ||
      typeof frame.index !== "number" ||
      typeof frame.last !== "boolean" ||
      typeof frame.data !== "string"
    ) {
      return false;
    }
    if (frame.index === 0) {
      this.receivedChunk = { index: 0, text: frame.data };
    } else if (this.receivedChunk === null || frame.index !== this.receivedChunk.index + 1) {
      this.receivedChunk = null;
      this.patchSnapshot({
        error: protoError(
          "internal",
          "The daemon's chunked frame arrived out of order.",
          "The connection will retry.",
        ),
      });
      return true;
    } else {
      this.receivedChunk.index = frame.index;
      this.receivedChunk.text += frame.data;
    }
    if (frame.last && this.receivedChunk !== null) {
      const joined = this.receivedChunk.text;
      this.receivedChunk = null;
      this.handleMessage(joined);
    }
    return true;
  }

  private handleMessage(data: string): void {
    if (this.receiveChunk(data)) return;
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
      // The subscribe answer closes the replay even when its request already
      // timed out: the timeout only stops the handshake from waiting, it does
      // not make the held frames older or newer than they are.
      if (frame.id === this.subscribeId) {
        this.subscribeId = null;
        if ("Ok" in frame.result) {
          const answer = frame.result.Ok;
          if (answer.type === "subscribed") {
            this.finishSubscribe(answer.replay_through ?? null, true);
          } else if (answer.type === "ack") {
            // An older daemon acknowledges the subscribe without naming a
            // boundary. A live-edge subscribe (`since_seq: null`) replayed
            // nothing, so the held frames are news; a resumed one is history.
            this.finishSubscribe(null, true);
          } else {
            // Any other successful answer is not a subscribe acceptance.
            this.finishSubscribe(null, false);
          }
        } else {
          // A refused subscribe has no boundary. Held frames cannot be the
          // gap replay the daemon declined to send, so treat them as history.
          this.finishSubscribe(null, false);
        }
      }
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

    // Harness update output is a live stream of the running command; a
    // reconnecting view re-reads the versions instead of replaying it.
    if (frame.kind === "harness_event") {
      emitHarnessFrame(frame.event);
      return;
    }

    // A scheduled task changed after a run, an edit or a rotation. A
    // reconnecting view re-reads the list instead of replaying this.
    if (frame.kind === "schedule_event") {
      emitScheduleFrame(frame.event);
      return;
    }

    // An import run's progress; a reconnecting client asks the daemon for the
    // current run instead of replaying this.
    if (frame.kind === "import_event") {
      emitImportFrame(frame.event);
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

  /**
   * Delivers the events held while the subscribe was open.
   *
   * `replayThrough` is the journal head the daemon served the subscribe at:
   * envelopes at or below it are the replay (history), and anything above it
   * arrived live after the daemon went live and must be treated as news. When
   * the subscribe failed there is no boundary, so everything held is history.
   *
   * `firstConnect` says the subscribe asked from the live edge
   * (`since_seq: null`). Nothing was replayed in that case, whatever boundary
   * the answer names: the daemon sends no history for a live-edge subscribe,
   * so every held frame reached this socket after it opened and is news. A
   * frame that arrived between `hello` and `subscribe` belongs here too, even
   * though the daemon's journal head (and therefore `replayThrough`) already
   * covers its sequence.
   */
  private flushReplay(replayThrough: number | null, firstConnect = false): void {
    if (!this.replaying) return;
    this.replaying = false;
    const pending = this.replayBuffer.splice(0).sort((left, right) => left.seq - right.seq);
    for (const envelope of pending) {
      if (envelope.seq <= this.lastSeq) continue;
      this.lastSeq = envelope.seq;
      // Held frames on a resumed subscribe are history from before this client
      // caught up. Tagging them is what lets the notifier tell a burst of old
      // approvals from a live one. A first connect has no history to tell.
      const isReplay =
        !firstConnect && (replayThrough !== null ? envelope.seq <= replayThrough : true);
      const replayed: EventEnvelope = {
        ...envelope,
        ...(isReplay ? { replayed: true } : {}),
      };
      for (const listener of this.eventListeners) {
        listener(replayed);
      }
    }
    // The acknowledged boundary covers the replay (or the live edge) the
    // daemon served, so a later reconnect asks from the right place. It is
    // kept apart from `lastSeq`: the reply may name a head that covers frames
    // still queued for this socket, and those are delivered, not dropped.
    if (replayThrough !== null) {
      this.ackSeq = Math.max(this.ackSeq, replayThrough);
    }
  }

  /** Closes the replay window with the subscribe answer's boundary. */
  private finishSubscribe(replayThrough: number | null, accepted: boolean): void {
    // A subscribe from the live edge replayed nothing, however its answer
    // names the boundary: the daemon sends no history for `since_seq: null`, so
    // every held frame is live. A resumed subscribe's held frames are history
    // unless the boundary says otherwise. A refused subscribe gets no such
    // benefit of the doubt.
    const firstConnect = accepted && this.subscribeSince === null;
    this.subscribeSince = null;
    this.flushReplay(replayThrough, firstConnect);
  }

  private handleClose(code: number, reason: string): void {
    this.socketOpen = false;
    this.transport = null;
    this.stopPing();
    this.handshaken = false;
    // Frames held for this socket's subscribe die with it: the next socket
    // asks from the delivered/acknowledged cursor again, so the daemon replays
    // the same range with a boundary instead of this client guessing at their
    // provenance.
    this.replayBuffer = [];
    this.subscribeId = null;
    this.subscribeSince = null;
    const dropped = new H4rnRequestError(
      protoError(
        "internal",
        reason || "The daemon closed the connection.",
        "The client reconnects automatically. Reads are retried; writes are not.",
      ),
    );
    DISCONNECTED.add(dropped);
    this.rejectPending(dropped);

    const fatal = FATAL_CLOSE_CODES.get(code);
    if (fatal) {
      this.fatal = true;
      this.settleHandshakeWaiters(
        new H4rnRequestError(
          protoError(code === 4401 ? "unauthorized" : "forbidden", fatal.message, fatal.hint),
        ),
      );
      this.setSnapshot({
        status: "offline",
        error: protoError(code === 4401 ? "unauthorized" : "forbidden", fatal.message, fatal.hint),
      });
      return;
    }

    if (this.stopped) return;
    this.scheduleReconnect();
  }

  /** Retries now and resets the backoff after a browser wake signal. */
  private retryImmediately(): void {
    if (this.stopped || this.fatal || this.socketOpen) return;
    // A wake during the very first connect is not a retry: nothing has failed
    // yet, and turning `connecting` into `degraded` would flash the reconnect
    // banner while the first socket is still opening. Once the client has
    // been ready, or a retry is already under way, wakes retry at once.
    if (!this.everReady && this.snapshot.status === "connecting") return;
    // A fresh attempt is still opening: wakes arrive in bursts (online, then
    // focus, then visibility) and each one must not restart the same attempt.
    if (this.transport && Date.now() - this.attemptStartedAt < WAKE_MIN_ATTEMPT_MS) return;
    // Deterministic offline scenarios opt out of retries entirely.
    if ((this.options.reconnect?.maxAttempts ?? 12) === 0) return;
    this.clearReconnect();
    // Never hold two sockets: this path can fire while an attempt is opening.
    if (this.transport) {
      this.transport.close();
      this.transport = null;
    }
    this.attempt = 0;
    // Stay in the cached `degraded` phase rather than flashing the skeleton.
    this.setSnapshot({ status: "degraded", attempt: 0, error: null });
    this.openSocket();
  }

  private installWakeListeners(): void {
    if (this.wakeListenersInstalled) return;
    const hasWindow = typeof window !== "undefined";
    const hasDocument = typeof document !== "undefined";
    if (!hasWindow && !hasDocument) return;
    this.wakeListenersInstalled = true;
    if (hasWindow) {
      window.addEventListener("online", this.onWake);
      window.addEventListener("focus", this.onWake);
    }
    if (hasDocument) {
      document.addEventListener("visibilitychange", this.onVisibilityChange);
    }
  }

  private removeWakeListeners(): void {
    if (!this.wakeListenersInstalled) return;
    this.wakeListenersInstalled = false;
    if (typeof window !== "undefined") {
      window.removeEventListener("online", this.onWake);
      window.removeEventListener("focus", this.onWake);
    }
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", this.onVisibilityChange);
    }
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.fatal || this.reconnectTimer) return;

    this.attempt += 1;
    const maxAttempts = this.options.reconnect?.maxAttempts ?? 12;
    // An explicit zero is the deterministic "give up" used by the offline
    // visual scenario; ordinary clients keep retrying at the capped interval.
    if (maxAttempts === 0) {
      this.setSnapshot({
        status: "offline",
        attempt: this.attempt,
        error:
          this.snapshot.error ??
          protoError(
            "internal",
            "The daemon connection is offline.",
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
    // Fast exponential attempts, then settle at the cap for as long as it
    // takes. The client only stops for a fatal close or an explicit stop.
    const fastAttempts = Math.min(this.attempt, maxAttempts);
    const nominal =
      this.attempt > maxAttempts ? max : Math.min(max, base * 2 ** (fastAttempts - 1));
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
