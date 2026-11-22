import { describe, expect, it, vi } from "vitest";
import { draftForEnvelope } from "@/features/notifications/logic";
import { NOTIFICATION_DEFAULTS } from "@/features/notifications/settings";
import type {
  ClientFrame,
  ClientRequest,
  Event,
  ResponseBody,
  ServerFrame,
  ThreadFilter,
} from "@/gen";

import { H4rnClient } from "./client";
import type { EventEnvelope, Transport, TransportHandlers } from "./types";
import { isProtocolToken } from "./ws";

type Reply = (body: ResponseBody) => void;

class FakeTransport implements Transport {
  readonly kind = "mock" as const;
  readonly sent: ClientFrame[] = [];
  handlers: TransportHandlers | null = null;
  closed = false;
  /** When false, frames other than hello are ignored. */
  answerAll = true;
  replyHello = true;

  constructor(readonly onSend?: (frame: ClientFrame, reply: Reply) => boolean) {}

  connect(handlers: TransportHandlers): void {
    this.handlers = handlers;
    handlers.onOpen();
  }

  send(data: string | ArrayBuffer | Uint8Array): void {
    if (typeof data !== "string") return;
    const frame = JSON.parse(data) as ClientFrame;
    this.sent.push(frame);
    const reply: Reply = (body) => this.reply(frame.id, body);
    if (this.onSend?.(frame, reply)) return;
    if (frame.body.type === "hello") {
      if (!this.replyHello) return;
      reply({
        type: "hello",
        protocol_version: 1,
        server_version: "test",
        device_id: "dev_test",
        local_host: "host_test",
        hosts: [],
      });
      return;
    }
    if (!this.answerAll) return;
    if (frame.body.type === "subscribe") reply({ type: "ack" });
    if (frame.body.type === "list_hosts") reply({ type: "hosts", hosts: [] });
    if (frame.body.type === "send_message" || frame.body.type === "create_thread") {
      reply({ type: "ack" });
    }
  }

  close(): void {
    this.closed = true;
  }

  reply(id: number, body: ResponseBody): void {
    this.frame({ kind: "response", id, result: { Ok: body } });
  }

  fail(id: number, code: "internal" | "unsupported" = "internal"): void {
    this.frame({
      kind: "response",
      id,
      result: { Err: { code, message: "nope", hint: null } },
    });
  }

  emit(seq: number, event: Event): void {
    this.frame({ kind: "event", seq, event } as ServerFrame);
  }

  frame(serverFrame: ServerFrame): void {
    this.handlers?.onMessage(JSON.stringify(serverFrame));
  }

  drop(code = 1006, reason = ""): void {
    this.handlers?.onClose({ code, reason, wasClean: false });
  }
}

/** A socket that opens then immediately drops without answering hello. */
class DeadTransport extends FakeTransport {
  override connect(handlers: TransportHandlers): void {
    this.handlers = handlers;
    this.answerAll = false;
    this.replyHello = false;
    handlers.onOpen();
    handlers.onClose({ code: 1006, reason: "refused", wasClean: false });
  }
}

type Harness = {
  client: H4rnClient;
  transports: FakeTransport[];
  events: EventEnvelope[];
};

type HarnessOptions = {
  requestTimeoutMs?: number;
  reconnect?: { baseMs: number; jitter: number; maxAttempts?: number };
};

function harness(
  factory?: (context: { attempt: number }) => FakeTransport,
  options: HarnessOptions = {},
): Harness {
  const transports: FakeTransport[] = [];
  const events: EventEnvelope[] = [];
  const client = new H4rnClient({
    transportFactory: (context) => {
      const transport = factory?.(context) ?? new FakeTransport();
      transports.push(transport);
      return transport;
    },
    ...(options.requestTimeoutMs === undefined
      ? {}
      : { requestTimeoutMs: options.requestTimeoutMs }),
    reconnect: options.reconnect ?? { baseMs: 1, jitter: 0, maxAttempts: 4 },
    transportLabel: "mock",
  });
  client.onEvent((envelope) => events.push(envelope));
  return { client, transports, events };
}

async function settle(): Promise<void> {
  for (let index = 0; index < 6; index += 1) await Promise.resolve();
}

const EMPTY_FILTER: ThreadFilter = {
  tabs: [],
  host: null,
  harness: null,
  search: null,
  include_snoozed: true,
  include_children: false,
};

function createBody(): Omit<Extract<ClientRequest, { type: "create_thread" }>, "type"> {
  return {
    host: "host_a",
    harness: "claude_code",
    workspace_path: "/work/repo",
    model: "opus",
    prompt: "hello",
    attachments: [],
    runtime_mode: "approval_required",
    worktree: null,
  };
}

describe("H4rnClient", () => {
  it("correlates responses by frame id regardless of arrival order", async () => {
    const pending: Array<{ reply: Reply }> = [];
    const { client, transports } = harness(
      () =>
        new FakeTransport((frame, reply) => {
          if (frame.body.type === "list_threads") {
            pending.push({ reply });
            return true;
          }
          return false;
        }),
    );

    client.start();
    await settle();
    expect(client.getSnapshot().status).toBe("online");

    const first = client.request({ type: "list_threads", filter: EMPTY_FILTER });
    const second = client.request({ type: "create_thread", ...createBody() });
    await settle();
    expect(pending).toHaveLength(1);

    const request = pending[0];
    if (!request) throw new Error("request was not captured");
    request.reply({ type: "threads", threads: [] });

    await expect(first).resolves.toMatchObject({ type: "threads" });
    await expect(second).resolves.toMatchObject({ type: "ack" });
    expect(transports[0]?.sent.length).toBeGreaterThanOrEqual(4);
  });

  it("rejects a request that outlives its timeout", async () => {
    const transport = new FakeTransport();
    transport.answerAll = false;
    const { client } = harness(() => transport, { requestTimeoutMs: 20 });

    client.start();
    await settle();

    await expect(
      client.request({ type: "send_message", thread_id: "t1", text: "hi", attachments: [] }),
    ).rejects.toMatchObject({ proto: { code: "internal" } });
  });

  it("rejects with the daemon's protocol error", async () => {
    const transport = new FakeTransport((frame) => {
      if (frame.body.type === "send_message") {
        transport.fail(frame.id, "unsupported");
        return true;
      }
      return false;
    });
    const { client } = harness(() => transport);

    client.start();
    await settle();

    await expect(
      client.request({ type: "send_message", thread_id: "t1", text: "hi", attachments: [] }),
    ).rejects.toMatchObject({ proto: { code: "unsupported" } });
  });

  it("drops duplicate and out-of-order events by sequence", async () => {
    const transport = new FakeTransport();
    const { client, events } = harness(() => transport);

    client.start();
    await settle();

    transport.emit(3, { type: "agent_state_changed", thread_id: "t1", state: "thinking" });
    transport.emit(2, { type: "agent_state_changed", thread_id: "t1", state: "idle" });
    transport.emit(3, { type: "agent_state_changed", thread_id: "t1", state: "thinking" });
    transport.emit(4, { type: "agent_state_changed", thread_id: "t1", state: "done" });

    expect(events.map((envelope) => envelope.seq)).toEqual([3, 4]);
  });

  it("tags frames held for a slow subscribe as replayed history", async () => {
    vi.useFakeTimers();
    try {
      let answerSubscribe: Reply | null = null;
      const { client, events, transports } = harness(
        () =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            answerSubscribe = reply;
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      expect(client.getSnapshot().status).toBe("online");

      // The person's device was away while this approval happened. The
      // subscribe has not answered, so the frame belongs to the replay.
      const approval: Event = {
        type: "approval_requested",
        thread_id: "t1",
        turn_id: "turn_1",
        approval_id: "ap_1",
        subagent_id: null,
        kind: "command",
        prompt: "npm test",
        options: [],
      };
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");
      transport.emit(7, approval);

      // A slow replay is exactly the case the old wall-clock grace guessed at.
      await vi.advanceTimersByTimeAsync(3_000);
      expect(events).toHaveLength(0);
      (answerSubscribe as Reply | null)?.({
        type: "ack",
      });
      await settle();

      expect(events).toHaveLength(1);
      expect(events[0]).toMatchObject({ seq: 7, replayed: true });

      const notificationOptions = {
        threads: {},
        settings: NOTIFICATION_DEFAULTS,
        focus: { windowFocused: false, openThreadId: null, connection: "online" as const },
      };
      const replayed = events[0];
      if (!replayed) throw new Error("replay was not delivered");
      expect(draftForEnvelope(replayed, notificationOptions)).toBeNull();

      // One second later, the same kind of event is live news.
      await vi.advanceTimersByTimeAsync(1_000);
      transport.emit(8, { ...approval, approval_id: "ap_2" } as Event);
      expect(events).toHaveLength(2);
      expect(events[1]?.replayed).toBeUndefined();
      const live = events[1];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions)).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("reports degraded while reconnecting and returns online", async () => {
    vi.useFakeTimers();
    try {
      const contexts: number[] = [];
      const { client, transports } = harness((context) => {
        contexts.push(context.attempt);
        return new FakeTransport();
      });

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      expect(client.getSnapshot().status).toBe("online");

      transports[0]?.drop();
      expect(client.getSnapshot()).toMatchObject({ status: "degraded", attempt: 1 });

      await vi.advanceTimersByTimeAsync(10);
      expect(contexts).toEqual([0, 1]);
      expect(client.getSnapshot().status).toBe("online");
      expect(client.getSnapshot().attempt).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("holds requests until the hello handshake completes", async () => {
    let answerHello: Reply | null = null;
    const { client, transports } = harness(
      () =>
        new FakeTransport((frame, reply) => {
          if (frame.body.type !== "hello") return false;
          answerHello = reply;
          return true;
        }),
    );

    client.start();
    await settle();
    const listing = client.request({ type: "list_threads", filter: EMPTY_FILTER });
    await settle();
    expect(transports[0]?.sent.map((frame) => frame.body.type)).toEqual(["hello"]);

    (answerHello as Reply | null)?.({
      type: "hello",
      protocol_version: 1,
      server_version: "test",
      device_id: "dev_test",
      local_host: "host_test",
      hosts: [],
    });
    await settle();
    const sent = transports[0]?.sent ?? [];
    expect(sent.map((frame) => frame.body.type)).toEqual(["hello", "subscribe", "list_threads"]);
    const frame = sent[2];
    if (!frame) throw new Error("list was not sent");
    transports[0]?.reply(frame.id, { type: "threads", threads: [] });
    await expect(listing).resolves.toMatchObject({ type: "threads" });
  });

  it("resends an idempotent read after a reconnect but never a write", async () => {
    vi.useFakeTimers();
    try {
      const { client, transports } = harness((context) =>
        context.attempt === 0
          ? new FakeTransport(
              (frame) => frame.body.type === "list_threads" || frame.body.type === "send_message",
            )
          : new FakeTransport((frame, reply) => {
              if (frame.body.type !== "list_threads") return false;
              reply({ type: "threads", threads: [] });
              return true;
            }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      const read = client.request({ type: "list_threads", filter: EMPTY_FILTER });
      const write = client.request({
        type: "send_message",
        thread_id: "t1",
        text: "hi",
        attachments: [],
      });
      const writeOutcome = write.then(
        () => "resolved",
        () => "rejected",
      );
      await vi.advanceTimersByTimeAsync(1);

      transports[0]?.drop();
      await vi.advanceTimersByTimeAsync(20);

      await expect(read).resolves.toMatchObject({ type: "threads" });
      await expect(writeOutcome).resolves.toBe("rejected");
      expect(transports[1]?.sent.map((frame) => frame.body.type)).not.toContain("send_message");
    } finally {
      vi.useRealTimers();
    }
  });

  it("resumes the subscription from the last applied sequence", async () => {
    const { client, transports } = harness();

    client.start();
    await settle();

    transports[0]?.emit(5, { type: "agent_state_changed", thread_id: "t1", state: "thinking" });
    transports[0]?.drop();

    await new Promise((resolve) => setTimeout(resolve, 20));
    await settle();

    const subscribe = transports[1]?.sent.find((frame) => frame.body.type === "subscribe");
    expect(subscribe?.body).toMatchObject({ type: "subscribe", since_seq: 5 });
  });

  it("goes offline after exhausting reconnect attempts", async () => {
    vi.useFakeTimers();
    try {
      const { client } = harness(() => new DeadTransport(), {
        reconnect: { baseMs: 1, jitter: 0, maxAttempts: 4 },
      });

      client.start();
      await vi.advanceTimersByTimeAsync(1);

      for (let index = 0; index < 6; index += 1) {
        await vi.advanceTimersByTimeAsync(100);
      }

      expect(client.getSnapshot().status).toBe("offline");
    } finally {
      vi.useRealTimers();
    }
  });

  it("treats auth close codes as fatal", async () => {
    const { client, transports } = harness();

    client.start();
    await settle();
    transports[0]?.drop(4401, "bad token");

    expect(client.getSnapshot()).toMatchObject({
      status: "offline",
      error: { code: "unauthorized" },
    });
  });
});

describe("isProtocolToken", () => {
  it("accepts RFC 6455 token characters", () => {
    expect(isProtocolToken("abc.DEF_123-~")).toBe(true);
  });

  it("rejects values that need the query fallback", () => {
    expect(isProtocolToken("")).toBe(false);
    expect(isProtocolToken("has space")).toBe(false);
    expect(isProtocolToken("café")).toBe(false);
    expect(isProtocolToken("a".repeat(124))).toBe(false);
  });
});
