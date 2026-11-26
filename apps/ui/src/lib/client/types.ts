import type {
  DeviceInfo,
  Event,
  HostId,
  HostSummary,
  ProtoError,
  ServerFrame,
  TransportStatus,
} from "@/gen";
import type { ClientErrorSource } from "./errors";

/**
 * Connection state shown by the status bar and the offline banner.
 *
 * `connecting` — first handshake, no projection yet.
 * `online`     — hello completed; events are flowing.
 * `degraded`   — the socket dropped and the client is reconnecting while it
 *                renders the cached projection. It keeps retrying at the
 *                capped interval until the socket returns.
 * `offline`    — the connection was stopped, retries were explicitly disabled,
 *                or auth failed.
 */
export type ConnectionStatus = "connecting" | "online" | "degraded" | "offline";

export type ConnectionSnapshot = {
  status: ConnectionStatus;
  /** Consecutive failed connection attempts; 0 while online. */
  attempt: number;
  /** Transport label for the status bar ("direct", "relay", "mock"). */
  transport: string;
  /** Wall-clock milliseconds of the last successful hello. */
  lastConnectedAt: number | null;
  /** Wall-clock milliseconds of the last frame of any kind. */
  lastEventAt: number | null;
  /** Round-trip time from the most recent liveness probe, if measured. */
  latencyMs: number | null;
  /** Last transport or protocol failure; cleared by a successful hello. */
  error: ProtoError | null;
};

export type ReadyInfo = {
  reconnected: boolean;
  hosts: HostSummary[];
  /** Host id of the daemon's own machine, for host-scoped requests. */
  localHostId: HostId;
  /**
   * The daemon machine's IANA time zone, from its `hello`.
   *
   * A scheduled task defaults to this zone so a floating time means the
   * daemon's wall clock, not this device's.
   */
  daemonTimezone: string;
  serverVersion: string;
  deviceId: string;
  /**
   * The connect machine the daemon owns, when it is signed in.
   *
   * `hello` reports it from the daemon's own session, so a direct socket can
   * name the machine it reached without consulting this device's account row.
   * `null` for a signed-out daemon or an older daemon that omits the field.
   */
  daemonMachineId: string | null;
};

export type EventEnvelope = {
  seq: number;
  event: Event;
  /**
   * True when this envelope is at or below the replay boundary the daemon
   * named in its subscribe answer. Replayed events are history: the projection
   * still applies them, but anything that responds to "news" (OS
   * notifications) must stay quiet. A live event that arrived while the
   * subscribe was in flight is above the boundary and is not tagged.
   */
  replayed?: boolean;
};

export type TransportHandlers = {
  onOpen: () => void;
  onMessage: (data: string) => void;
  /** One binary WebSocket message; the header is decoded by the feature using it. */
  onBinary?: (data: ArrayBuffer) => void;
  /**
   * The transport closed. `error` is set when the transport knows why in
   * words the client owns (`shell:errors.*`), so requests that were waiting
   * on it fail with that kind rather than the raw `reason`.
   */
  onClose: (frame: {
    code: number;
    reason: string;
    wasClean: boolean;
    error?: ClientErrorSource;
  }) => void;
  /** A transport failure, as a kind the client words (`shell:errors.*`). */
  onError: (failure: ClientErrorSource) => void;
  /**
   * Multipath state from the secure transport. Optional so the plain
   * WebSocket and mock transports stay unchanged.
   */
  onTransportStatus?: (status: TransportStatus) => void;
};

/** One socket attempt. Reconnection is owned by `H4rnClient`, not the transport. */
export interface Transport {
  readonly kind: "websocket" | "mock";
  connect: (handlers: TransportHandlers) => void;
  send: (data: string | ArrayBuffer | Uint8Array) => void;
  close: () => void;
}

export type TransportContext = {
  url: string;
  token: string | null;
  attempt: number;
};

export type TransportFactory = (context: TransportContext) => Transport;

export type ClientOptions = {
  url?: string;
  token?: string | null;
  device?: DeviceInfo;
  transportFactory?: TransportFactory;
  /** Human label for the status bar. */
  transportLabel?: string;
  requestTimeoutMs?: number;
  reconnect?: {
    /** First retry delay; doubles on each consecutive failure. */
    baseMs?: number;
    /** Capped retry interval, kept for as long as the socket stays down. */
    maxMs?: number;
    /** Fraction of the delay added or removed at random. */
    jitter?: number;
    /**
     * Fast exponential attempts before settling at `maxMs`; `0` disables
     * retries entirely for deterministic offline scenarios.
     */
    maxAttempts?: number;
  };
};

export type RequestBody<_T> = ServerFrame extends never ? never : never;

export type { DeviceInfo, Event, HostSummary, ProtoError, ServerFrame };
