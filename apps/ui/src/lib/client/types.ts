import type {
  DeviceInfo,
  Event,
  HostId,
  HostSummary,
  ProtoError,
  ServerFrame,
  TransportStatus,
} from "@/gen";

/**
 * Connection state shown by the status bar and the offline banner.
 *
 * `connecting` — first handshake, no projection yet.
 * `online`     — hello completed; events are flowing.
 * `degraded`   — the socket dropped and the client is reconnecting while it
 *                renders the cached projection.
 * `offline`    — retries are exhausted, the device is offline, or auth failed.
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
  serverVersion: string;
  deviceId: string;
};

export type EventEnvelope = {
  seq: number;
  event: Event;
  /**
   * True when the client delivered this envelope from the subscribe replay
   * after a reconnect. Replayed events are history: the projection still
   * applies them, but anything that responds to "news" (OS notifications)
   * must stay quiet.
   */
  replayed?: boolean;
};

export type TransportHandlers = {
  onOpen: () => void;
  onMessage: (data: string) => void;
  /** One binary WebSocket message; the header is decoded by the feature using it. */
  onBinary?: (data: ArrayBuffer) => void;
  onClose: (frame: { code: number; reason: string; wasClean: boolean }) => void;
  onError: (message: string) => void;
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
    baseMs?: number;
    maxMs?: number;
    jitter?: number;
    maxAttempts?: number;
  };
};

export type RequestBody<_T> = ServerFrame extends never ? never : never;

export type { DeviceInfo, Event, HostSummary, ProtoError, ServerFrame };
