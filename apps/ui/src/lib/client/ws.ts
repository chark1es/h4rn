import type { Transport, TransportHandlers } from "./types";

/**
 * One browser WebSocket attempt.
 *
 * Auth is offered as subprotocols (`h4rn.v1`, `bearer`, `<token>`) because a
 * custom header is impossible from a browser WebSocket. Tokens that cannot be
 * a WebSocket protocol token (spaces, separators, non-ASCII) fall back to the
 * `?token=` query parameter.
 */
export class WebSocketTransport implements Transport {
  readonly kind = "websocket" as const;
  private socket: WebSocket | null = null;
  private closedByUs = false;

  constructor(
    private readonly url: string,
    private readonly token: string | null,
  ) {}

  connect(handlers: TransportHandlers): void {
    this.closedByUs = false;

    const protocols = ["h4rn.v1"];
    let url = this.url;

    if (this.token) {
      if (isProtocolToken(this.token)) {
        protocols.push("bearer", this.token);
      } else {
        const separator = url.includes("?") ? "&" : "?";
        url = `${url}${separator}token=${encodeURIComponent(this.token)}`;
      }
    }

    let socket: WebSocket;
    try {
      socket = new WebSocket(url, protocols);
    } catch (error) {
      handlers.onError(error instanceof Error ? error.message : "The WebSocket URL was rejected.");
      handlers.onClose({
        code: 1006,
        reason: "The WebSocket could not be created.",
        wasClean: false,
      });
      return;
    }

    this.socket = socket;
    socket.binaryType = "arraybuffer";

    socket.onopen = () => {
      handlers.onOpen();
    };
    socket.onmessage = (message) => {
      if (typeof message.data === "string") {
        handlers.onMessage(message.data);
      } else if (message.data instanceof ArrayBuffer) {
        handlers.onBinary(message.data);
      }
    };
    socket.onerror = () => {
      handlers.onError("The daemon did not answer. It may be stopped or unreachable.");
    };
    socket.onclose = (event) => {
      if (this.closedByUs) return;
      handlers.onClose({
        code: event.code,
        reason: event.reason || "The connection closed.",
        wasClean: event.wasClean,
      });
    };
  }

  send(data: string | ArrayBuffer | Uint8Array): void {
    if (this.socket?.readyState !== WebSocket.OPEN) {
      throw new Error("The socket is not open.");
    }
    this.socket.send(data);
  }

  close(): void {
    this.closedByUs = true;
    if (this.socket && this.socket.readyState <= WebSocket.OPEN) {
      this.socket.close(1000, "client closed");
    }
    this.socket = null;
  }
}

/** RFC 6455 token grammar, bounded so a header stays sane. */
export function isProtocolToken(value: string): boolean {
  return value.length > 0 && value.length <= 123 && /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(value);
}
