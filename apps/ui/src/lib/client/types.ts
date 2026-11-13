import type { DeviceInfo, Event, HostSummary, ProtoError, ServerFrame } from "@/gen";

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
  serverVersion: string;
  deviceId: string;
};

export type EventEnvelope = {
  seq: number;
  event: Event;
};

export type TransportHandlers = {
  onOpen: () => void;
  onMessage: (data: string) => void;
  onClose: (frame: { code: number; reason: string; wasClean: boolean }) => void;
  onError: (message: string) => void;
};

/** One socket attempt. Reconnection is owned by `H4rnClient`, not the transport. */
export interface Transport {
  readonly kind: "websocket" | "mock";
  connect: (handlers: TransportHandlers) => void;
  send: (data: string) => void;
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
