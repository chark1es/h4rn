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
import type { EventEnvelope, ReadyInfo, Transport, TransportHandlers } from "./types";
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
        daemon_machine_id: null,
        local_host: "host_test",
        hosts: [],
      });
      return;
    }
    if (!this.answerAll) return;
    if (frame.body.type === "subscribe") reply({ type: "subscribed", replay_through: 0 });
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

/** A socket whose connection never completes: `onOpen` does not fire. */
class PendingTransport extends FakeTransport {
  override connect(handlers: TransportHandlers): void {
    this.handlers = handlers;
  }
}

type Harness = {
  client: H4rnClient;
  transports: FakeTransport[];
  events: EventEnvelope[];
};

type HarnessOptions = {
  requestTimeoutMs?: number;
  reconnect?: { baseMs: number; maxMs?: number; jitter: number; maxAttempts?: number };
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

function approvalEvent(approvalId: string): Event {
  return {
    type: "approval_requested",
    thread_id: "t1",
    turn_id: "turn_1",
    approval_id: approvalId,
    subagent_id: null,
    kind: "command",
    prompt: "npm test",
    options: [],
  };
}

function notificationOptions() {
  return {
    threads: {},
    settings: NOTIFICATION_DEFAULTS,
    focus: {
      windowFocused: false,
      openThreadId: null,
      connection: "online" as const,
    },
  };
}

function createBody(): Omit<Extract<ClientRequest, { type: "create_thread" }>, "type"> {
  return {
    host: "host_a",
    harness: "claude_code",
    agent_id: null,
    workspace_path: "/work/repo",
    model: "opus",
    prompt: "hello",
    attachments: [],
    runtime_mode: "approval_required",
    worktree: null,
  };
}

describe("H4rnClient", () => {
  it("reports the daemon's connect machine from hello", async () => {
    type HelloBody = Extract<ResponseBody, { type: "hello" }>;
    const hello = (daemonMachineId: string | null): HelloBody => ({
      type: "hello",
      protocol_version: 1,
      server_version: "test",
      device_id: "dev_test",
      daemon_machine_id: daemonMachineId,
      local_host: "host_test",
      hosts: [],
    });
    const readyWith = async (body: ResponseBody): Promise<ReadyInfo> => {
      const { client } = harness(
        () =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "hello") return false;
            reply(body);
            return true;
          }),
      );
      const ready = new Promise<ReadyInfo>((resolve) => client.onReady(resolve));
      client.start();
      return ready;
    };

    expect((await readyWith(hello("m_atlas"))).daemonMachineId).toBe("m_atlas");
    // An older daemon omits the field; the client reports no machine rather
    // than an undefined value reaching the notifier.
    const { daemon_machine_id: _omitted, ...older } = hello("m_atlas");
    expect((await readyWith(older as ResponseBody)).daemonMachineId).toBeNull();
  });

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

  it("tags frames held for a slow resumed subscribe as replayed history", async () => {
    vi.useFakeTimers();
    try {
      let answerSubscribe: Reply | null = null;
      const { client, events, transports } = harness(
        (context) =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            if (context.attempt === 0) {
              reply({ type: "subscribed", replay_through: 0 });
              return true;
            }
            answerSubscribe = reply;
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");
      transport.emit(3, { type: "agent_state_changed", thread_id: "t1", state: "thinking" });
      await settle();
      expect(events).toHaveLength(1);
      transport.drop();
      await vi.advanceTimersByTimeAsync(20);
      expect(client.getSnapshot().status).toBe("online");
      expect(answerSubscribe).not.toBeNull();

      // The person's device was away while this approval happened. The resume
      // asks from the last seen sequence, so the frame belongs to the replay.
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
      const resumed = transports[1];
      if (!resumed) throw new Error("the reconnect transport was not created");
      resumed.emit(7, approval);

      // A slow replay is exactly the case the old wall-clock grace guessed at.
      await vi.advanceTimersByTimeAsync(3_000);
      expect(events).toHaveLength(1);
      (answerSubscribe as Reply | null)?.({
        type: "subscribed",
        replay_through: 7,
      });
      await settle();

      expect(events).toHaveLength(2);
      expect(events[1]).toMatchObject({ seq: 7, replayed: true });

      const notificationOptions = {
        threads: {},
        settings: NOTIFICATION_DEFAULTS,
        focus: { windowFocused: false, openThreadId: null, connection: "online" as const },
      };
      const replayed = events[1];
      if (!replayed) throw new Error("replay was not delivered");
      expect(draftForEnvelope(replayed, notificationOptions)).toBeNull();

      // One second later, the same kind of event is live news.
      await vi.advanceTimersByTimeAsync(1_000);
      resumed.emit(8, { ...approval, approval_id: "ap_2" } as Event);
      expect(events).toHaveLength(3);
      expect(events[2]?.replayed).toBeUndefined();
      const live = events[2];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions)).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("treats every frame held on a first connect as news", async () => {
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
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");

      // `since_seq: null` means the daemon sent no history at all. Both frames
      // arrived on this socket after it opened, including the one whose
      // sequence the answer's journal head already covers, so both are news.
      transport.emit(2, approvalEvent("ap_before_subscribe"));
      transport.emit(4, approvalEvent("ap_live"));
      (answerSubscribe as Reply | null)?.({
        type: "subscribed",
        replay_through: 3,
      });
      await settle();

      expect(events[0]).toMatchObject({ seq: 2 });
      expect(events[0]?.replayed).toBeUndefined();
      expect(events[1]?.replayed).toBeUndefined();
      const first = events[0];
      if (!first) throw new Error("held event was not delivered");
      expect(draftForEnvelope(first, notificationOptions())).not.toBeNull();
      const live = events[1];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("delivers a first-connect frame that arrives after the subscribe reply", async () => {
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
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");

      // The daemon serves the live edge at journal head 7 and queues an
      // approval behind its answer. The frame reaches this socket after the
      // reply, with a sequence the reply's head already covers; it is still
      // news, not history, and must not be deduplicated against that head.
      (answerSubscribe as Reply | null)?.({
        type: "subscribed",
        replay_through: 7,
      });
      await settle();
      transport.emit(7, approvalEvent("ap_after_reply"));
      await settle();

      expect(events.map((envelope) => envelope.seq)).toEqual([7]);
      expect(events[0]?.replayed).toBeUndefined();
      const live = events[0];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("treats a live event after the reconnect replay boundary as news", async () => {
    vi.useFakeTimers();
    try {
      let answerReconnect: Reply | null = null;
      const { client, events, transports } = harness(
        (context) =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            if (context.attempt === 0) {
              reply({ type: "subscribed", replay_through: 0 });
              return true;
            }
            answerReconnect = reply;
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      transports[0]?.emit(2, approvalEvent("ap_seen"));
      transports[0]?.drop();
      await vi.advanceTimersByTimeAsync(20);
      expect(answerReconnect).not.toBeNull();

      // The daemon served the subscribe at head 3, then a fourth event landed
      // while the answer was in flight. The first is history; the second is
      // what the person actually missed.
      transports[1]?.emit(3, approvalEvent("ap_replayed"));
      transports[1]?.emit(4, approvalEvent("ap_live"));
      (answerReconnect as Reply | null)?.({
        type: "subscribed",
        replay_through: 3,
      });
      await settle();

      const replayed = events.find((envelope) => envelope.seq === 3);
      const live = events.find((envelope) => envelope.seq === 4);
      expect(replayed?.replayed).toBe(true);
      expect(live?.replayed).toBeUndefined();
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps holding frames when the subscribe outlives its own timeout", async () => {
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
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");

      // Held while the subscribe is open, and still held after its request
      // times out: without the answer nothing can be delivered yet.
      transport.emit(7, approvalEvent("ap_held"));
      await vi.advanceTimersByTimeAsync(6_000);
      expect(client.getSnapshot().status).toBe("online");
      expect(events).toHaveLength(0);

      transport.emit(8, approvalEvent("ap_live"));
      await vi.advanceTimersByTimeAsync(1_000);
      expect(events).toHaveLength(0);

      (answerSubscribe as Reply | null)?.({
        type: "subscribed",
        replay_through: 7,
      });
      await settle();

      // A first connect replayed nothing, so the frame at the served head is
      // as live as the one after it.
      expect(events).toHaveLength(2);
      expect(events[0]).toMatchObject({ seq: 7 });
      expect(events[0]?.replayed).toBeUndefined();
      expect(events[1]?.replayed).toBeUndefined();
      const live = events[1];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops frames held for a socket that closes before the subscribe answer", async () => {
    vi.useFakeTimers();
    try {
      let answerSubscribe: Reply | null = null;
      const { client, events, transports } = harness(
        (context) =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            if (context.attempt === 0) {
              answerSubscribe = reply;
              return true;
            }
            reply({ type: "subscribed", replay_through: 0 });
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      transports[0]?.emit(7, approvalEvent("ap_held"));
      transports[0]?.drop();
      await vi.advanceTimersByTimeAsync(20);

      // The next socket re-subscribes from the same cursor, so the held copy
      // must not also be delivered live.
      expect(events).toHaveLength(0);
      expect(answerSubscribe).not.toBeNull();
      expect(
        transports[1]?.sent.find((frame) => frame.body.type === "subscribe")?.body,
      ).toMatchObject({ type: "subscribe", since_seq: null });
    } finally {
      vi.useRealTimers();
    }
  });

  it("delivers live-edge frames untagged when an older daemon names no boundary", async () => {
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
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");

      // A first connect asks from the live edge; an older daemon accepts it
      // without a boundary and sends nothing itself, so the frame is news.
      transport.emit(3, approvalEvent("ap_live"));
      (answerSubscribe as Reply | null)?.({ type: "subscribed" } as never);
      await settle();

      expect(events).toHaveLength(1);
      expect(events[0]?.replayed).toBeUndefined();
      const live = events[0];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("delivers live-edge frames untagged when an older daemon answers only with an ack", async () => {
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
      const transport = transports[0];
      if (!transport) throw new Error("transport was not created");

      // A first connect asks from the live edge and this daemon accepts the
      // subscription with the protocol's plain `{type:"ack"}` reply. Nothing
      // was replayed, so the held frame is news.
      transport.emit(3, approvalEvent("ap_live"));
      (answerSubscribe as Reply | null)?.({ type: "ack" });
      await settle();

      expect(events).toHaveLength(1);
      expect(events[0]?.replayed).toBeUndefined();
      const live = events[0];
      if (!live) throw new Error("live event was not delivered");
      expect(draftForEnvelope(live, notificationOptions())).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps a resumed subscribe's held frames tagged when the daemon only acks", async () => {
    vi.useFakeTimers();
    try {
      let answerSubscribe: Reply | null = null;
      const { client, events, transports } = harness(
        (context) =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            if (context.attempt === 0) {
              reply({ type: "subscribed", replay_through: 0 });
              return true;
            }
            answerSubscribe = reply;
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      transports[0]?.emit(2, approvalEvent("ap_seen"));
      transports[0]?.drop();
      await vi.advanceTimersByTimeAsync(20);

      // The reconnect asked for a replay (`since_seq` is no longer null). An
      // ack names no boundary, so guessing those frames were live would
      // announce history the client just missed.
      transports[1]?.emit(3, approvalEvent("ap_replayed"));
      (answerSubscribe as Reply | null)?.({ type: "ack" });
      await settle();

      expect(events.find((envelope) => envelope.seq === 3)?.replayed).toBe(true);
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
      daemon_machine_id: null,
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

  it("resumes a first connect from the head its reply acknowledged", async () => {
    vi.useFakeTimers();
    try {
      let answerSubscribe: Reply | null = null;
      const { client, transports } = harness(
        () =>
          new FakeTransport((frame, reply) => {
            if (frame.body.type !== "subscribe") return false;
            answerSubscribe = reply;
            return true;
          }),
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      // A live-edge subscribe replayed nothing but the daemon named head 9:
      // a reconnect must not ask to replay that skipped history.
      (answerSubscribe as Reply | null)?.({
        type: "subscribed",
        replay_through: 9,
      });
      await settle();
      transports[0]?.drop();
      await vi.advanceTimersByTimeAsync(20);
      await settle();

      const subscribe = transports[1]?.sent.find((frame) => frame.body.type === "subscribe");
      expect(subscribe?.body).toMatchObject({ type: "subscribe", since_seq: 9 });
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps retrying at the capped interval instead of giving up", async () => {
    vi.useFakeTimers();
    try {
      const { client, transports } = harness(() => new DeadTransport(), {
        reconnect: { baseMs: 1, maxMs: 4, jitter: 0, maxAttempts: 2 },
      });

      client.start();
      await vi.advanceTimersByTimeAsync(1);

      for (let index = 0; index < 10; index += 1) {
        await vi.advanceTimersByTimeAsync(10);
      }

      expect(client.getSnapshot().status).toBe("degraded");
      expect(client.getSnapshot().attempt).toBeGreaterThan(2);
      expect(transports.length).toBeGreaterThan(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("retries at once on online and focus without stacking sockets", async () => {
    vi.useFakeTimers();
    const windowStub = new EventTarget();
    Object.defineProperty(globalThis, "window", { value: windowStub, configurable: true });
    try {
      let opened = 0;
      const staying = new FakeTransport();
      const { client, transports } = harness(
        () => (opened++ === 0 ? new DeadTransport() : staying),
        { reconnect: { baseMs: 60_000, maxMs: 60_000, jitter: 0, maxAttempts: 4 } },
      );

      client.start();
      expect(transports.length).toBe(1);
      expect(client.getSnapshot().status).toBe("degraded");
      // The 60 s backoff must not hold the retry back.
      windowStub.dispatchEvent(new Event("online"));
      await settle();
      expect(transports.length).toBe(2);
      expect(client.getSnapshot().status).toBe("online");

      // An open socket is not replaced by another wake signal.
      windowStub.dispatchEvent(new Event("focus"));
      expect(transports.length).toBe(2);

      transports[1]?.drop();
      expect(client.getSnapshot()).toMatchObject({ status: "degraded", attempt: 1 });
      windowStub.dispatchEvent(new Event("focus"));
      await settle();
      expect(transports.length).toBe(3);
      expect(client.getSnapshot().attempt).toBe(0);
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "window");
      vi.useRealTimers();
    }
  });

  it("ignores wakes while a fresh connect attempt is still opening", async () => {
    vi.useFakeTimers();
    const windowStub = new EventTarget();
    Object.defineProperty(globalThis, "window", { value: windowStub, configurable: true });
    try {
      let opened = 0;
      const { client, transports } = harness(
        () => (opened++ === 0 ? new FakeTransport() : new PendingTransport()),
        { reconnect: { baseMs: 60_000, maxMs: 60_000, jitter: 0, maxAttempts: 4 } },
      );

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      expect(client.getSnapshot().status).toBe("online");

      // A wake skips the 60 s backoff; the new socket never finishes opening.
      transports[0]?.drop();
      windowStub.dispatchEvent(new Event("online"));
      expect(transports.length).toBe(2);
      expect(client.getSnapshot().status).toBe("degraded");

      // Wakes in the first second of that attempt must not restart it.
      windowStub.dispatchEvent(new Event("focus"));
      windowStub.dispatchEvent(new Event("online"));
      await settle();
      expect(transports.length).toBe(2);
      expect(transports[1]?.closed).toBe(false);

      // Once the attempt is a second old, a wake may retry again.
      await vi.advanceTimersByTimeAsync(1_100);
      windowStub.dispatchEvent(new Event("focus"));
      expect(transports.length).toBe(3);
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "window");
      vi.useRealTimers();
    }
  });

  it("does not turn the first connect into a reconnect on wake signals", async () => {
    const windowStub = new EventTarget();
    const documentStub = new EventTarget() as EventTarget & {
      visibilityState: DocumentVisibilityState;
    };
    documentStub.visibilityState = "visible";
    Object.defineProperty(globalThis, "window", { value: windowStub, configurable: true });
    Object.defineProperty(globalThis, "document", {
      value: documentStub,
      configurable: true,
    });
    try {
      // Hello never answers, so the first socket is still connecting.
      const { client, transports } = harness(() => new FakeTransport(() => true));
      client.start();
      expect(client.getSnapshot().status).toBe("connecting");

      windowStub.dispatchEvent(new Event("online"));
      windowStub.dispatchEvent(new Event("focus"));
      documentStub.dispatchEvent(new Event("visibilitychange"));
      await settle();

      expect(transports.length).toBe(1);
      expect(client.getSnapshot().status).toBe("connecting");
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "window");
      Reflect.deleteProperty(globalThis, "document");
    }
  });

  it("retries at once when the document becomes visible", async () => {
    vi.useFakeTimers();
    const documentStub = new EventTarget() as EventTarget & {
      visibilityState: DocumentVisibilityState;
    };
    documentStub.visibilityState = "hidden";
    Object.defineProperty(globalThis, "document", {
      value: documentStub,
      configurable: true,
    });
    try {
      const { client, transports } = harness(() => new DeadTransport(), {
        reconnect: { baseMs: 60_000, maxMs: 60_000, jitter: 0, maxAttempts: 4 },
      });

      client.start();
      expect(transports.length).toBe(1);

      documentStub.dispatchEvent(new Event("visibilitychange"));
      expect(transports.length).toBe(1);

      documentStub.visibilityState = "visible";
      documentStub.dispatchEvent(new Event("visibilitychange"));
      expect(transports.length).toBe(2);
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "document");
      vi.useRealTimers();
    }
  });

  it("reinstalls wake listeners when the user presses Reconnect", async () => {
    vi.useFakeTimers();
    const windowStub = new EventTarget();
    Object.defineProperty(globalThis, "window", { value: windowStub, configurable: true });
    try {
      const { client, transports } = harness(() => new FakeTransport(), {
        reconnect: { baseMs: 60_000, maxMs: 60_000, jitter: 0, maxAttempts: 4 },
      });

      client.start();
      await vi.advanceTimersByTimeAsync(1);
      expect(client.getSnapshot().status).toBe("online");

      // Stopping removes the listeners; Reconnect must bring them back.
      client.stop();
      client.reconnectNow();
      await vi.advanceTimersByTimeAsync(1);
      expect(client.getSnapshot().status).toBe("online");
      expect(transports.length).toBe(2);

      transports[1]?.drop();
      expect(client.getSnapshot()).toMatchObject({ status: "degraded", attempt: 1 });
      // The 60 s backoff must not hold this retry back.
      windowStub.dispatchEvent(new Event("online"));
      await settle();
      expect(transports.length).toBe(3);
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "window");
      vi.useRealTimers();
    }
  });

  it("ignores wake signals after a fatal close", async () => {
    const windowStub = new EventTarget();
    Object.defineProperty(globalThis, "window", { value: windowStub, configurable: true });
    try {
      const { client, transports } = harness();
      client.start();
      await settle();
      transports[0]?.drop(4401, "bad token");
      windowStub.dispatchEvent(new Event("online"));
      expect(transports.length).toBe(1);
      expect(client.getSnapshot().status).toBe("offline");
      client.stop();
    } finally {
      Reflect.deleteProperty(globalThis, "window");
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
