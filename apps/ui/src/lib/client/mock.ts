import type {
  AgentState,
  ClientFrame,
  Event,
  HarnessKind,
  HostSummary,
  Item,
  ProtoError,
  ResponseBody,
  ServerFrame,
  ThreadSummary,
  ToolCall,
  TurnStatus,
  Usage,
} from "@/gen";

import type { Transport, TransportHandlers } from "./types";

type MockThread = {
  summary: ThreadSummary;
  events: Event[];
};

type MockApproval = {
  threadId: string;
  approvalId: string;
  turnId: string;
};

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

const iso = (offsetMs = 0) => new Date(Date.now() + offsetMs).toISOString();

function uid(prefix: string): string {
  return `${prefix}_${Math.random().toString(36).slice(2, 10)}`;
}

function messageItem(itemId: string, role: "user" | "assistant" | "system", text: string): Item {
  return { kind: "message", item_id: itemId, role, text };
}

function userMessage(itemId: string, text: string): Event {
  return { type: "item_completed", ...messageItem(itemId, "user", text) };
}

function assistantMessage(itemId: string, text: string): Event {
  return { type: "item_completed", ...messageItem(itemId, "assistant", text) };
}

function toolCall(
  overrides: Partial<ToolCall> & Pick<ToolCall, "tool_call_id" | "name" | "summary">,
): ToolCall {
  return {
    status: "completed",
    input: null,
    output: null,
    started_at: iso(-8_000),
    completed_at: iso(-7_400),
    ...overrides,
  };
}

function usage(input: number, output: number, durationMs: number, costMicros: number): Usage {
  return {
    input_tokens: input,
    output_tokens: output,
    cached_input_tokens: Math.round(input * 0.42),
    cost_micros: costMicros,
    duration_ms: durationMs,
  };
}

const TOTAL_USAGE: Usage = usage(182_400, 41_800, 214_000, 420_000);

const continuedText = [
  "The failure only reproduces under the CI runner because the fake clock is installed after the database pool opens, so the first `find` races the timer advance.",
  "\n\nI traced it to `tests/setup.ts`: `vi.useFakeTimers()` runs in `beforeEach`, but `db.connect()` is module-scoped and settles on a real timer from the previous test file.",
  "\n\nI can pin the clock in the setup hook and make the pool lazy, then run the suite three times with `CI=1` to confirm the race is gone.",
  "\n\nOne more check: `session.test.ts` asserts the expiry boundary with `Date.now()` directly. I'd switch that to the injected clock so the test cannot drift when the suite runs near midnight.",
].join("");

function historyFor(thread: ThreadSummary): Event[] {
  const events: Event[] = [];
  const user = userMessage(
    `msg_${thread.thread_id}_u0`,
    thread.thread_id === "thr_a"
      ? "The auth test fails on CI only. Fix it and add a regression test that pins the clock. Do not change the public API."
      : thread.thread_id === "thr_b"
        ? "Add exponential backoff with jitter to the relay reconnect, and make sure a resumed stream does not replay events the client already applied."
        : `Work on ${thread.title.toLowerCase()} in ${thread.workspace_path}. Report what you find before changing files.`,
  );
  events.push(user);

  const turnId = `turn_${thread.thread_id}_1`;
  events.push({
    type: "turn_started",
    thread_id: thread.thread_id,
    turn_id: turnId,
    started_at: iso(-8_000),
  });

  if (thread.thread_id === "thr_a") {
    events.push(
      assistantMessage(
        `msg_${thread.thread_id}_a0`,
        "I'll reproduce the CI-only failure first, then pin the clock in the setup hook.",
      ),
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_read",
          name: "Read",
          summary: "auth/session.test.ts · 42 lines",
          started_at: iso(-7_900),
          completed_at: iso(-7_500),
        }),
      },
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_grep",
          name: "Grep",
          summary: '"fakeTimers|Date.now" · 3 matches in tests',
          started_at: iso(-7_400),
          completed_at: iso(-7_100),
        }),
      },
      assistantMessage(
        `msg_${thread.thread_id}_a1`,
        "The assertion is not the problem. The pool opens on a real timer before the setup hook installs the fake clock, so the first lookup races the timer advance on slower CI hardware.",
      ),
      {
        type: "approval_requested",
        thread_id: thread.thread_id,
        turn_id: turnId,
        approval_id: "ap_a_1",
        kind: "command",
        prompt:
          "Run `CI=1 pnpm test --filter auth --repeat 3` on atlas to confirm the race before editing tests/setup.ts.",
        options: [],
      },
      { type: "agent_state_changed", thread_id: thread.thread_id, state: "awaiting_approval" },
    );
    return events;
  }

  if (thread.thread_id === "thr_b") {
    events.push(
      assistantMessage(
        `msg_${thread.thread_id}_a0`,
        "Starting with the session layer: the socket owns the transport, so backoff belongs one level up.",
      ),
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_b_grep",
          name: "Grep",
          summary: '"reconnect|since_seq" · 11 matches',
        }),
      },
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_b_read",
          name: "Read",
          summary: "crates/h4rn-transport/src/session.rs · 318 lines",
          status: "running",
          completed_at: null,
          started_at: iso(-2_000),
        }),
      },
      {
        type: "message_delta",
        thread_id: thread.thread_id,
        turn_id: turnId,
        item_id: `msg_${thread.thread_id}_live`,
        text: "The backoff belongs in the client, not the socket: ",
      },
      { type: "agent_state_changed", thread_id: thread.thread_id, state: "running_tool" },
    );
    return events;
  }

  if (thread.thread_id === "thr_c") {
    events.push({
      type: "tool_call",
      thread_id: thread.thread_id,
      turn_id: turnId,
      tool_call: toolCall({
        tool_call_id: "tool_c_read",
        name: "Read",
        summary: "pnpm-lock.yaml · 2,104 lines",
        status: "running",
        completed_at: null,
        started_at: iso(-4_000),
      }),
    });
    events.push({ type: "agent_state_changed", thread_id: thread.thread_id, state: "reading" });
    return events;
  }

  events.push(
    assistantMessage(
      `msg_${thread.thread_id}_a0`,
      thread.tab === "working"
        ? "I'm reading the relevant files before proposing a change. The workspace is clean and no tool has written anything yet."
        : "The change is in place and the checks pass. I left the branch clean and did not commit.",
    ),
  );

  if (thread.tab !== "working") {
    events.push({
      type: "tool_call",
      thread_id: thread.thread_id,
      turn_id: turnId,
      tool_call: toolCall({
        tool_call_id: `tool_${thread.thread_id}_test`,
        name: "Bash",
        summary:
          thread.tab === "done"
            ? "pnpm test --filter session · 48 passed"
            : "pnpm lint · no findings",
      }),
    });
    events.push({
      type: "turn_completed",
      thread_id: thread.thread_id,
      turn_id: turnId,
      usage: thread.usage,
      status: "completed",
    });
    events.push({
      type: "agent_state_changed",
      thread_id: thread.thread_id,
      state: "done",
    });
  }

  return events;
}

function summary(input: {
  id: string;
  host: string;
  harness: HarnessKind;
  title: string;
  model: string;
  tab: ThreadSummary["tab"];
  state: AgentState;
  workspace?: string;
  pinned?: boolean;
  snoozedMs?: number;
  lastActivityMs: number;
  unread?: boolean;
  needsAttention?: boolean;
  usage?: Usage;
}): ThreadSummary {
  return {
    thread_id: input.id,
    host: input.host,
    harness: input.harness,
    title: input.title,
    workspace_path: input.workspace ?? "/home/you/code/api",
    model: input.model,
    tab: input.tab,
    pinned: input.pinned ?? false,
    snoozed_until: input.snoozedMs ? iso(input.snoozedMs) : null,
    last_activity: iso(-input.lastActivityMs),
    agent_state: input.state,
    usage: input.usage ?? usage(12_000, 3_400, 42_000, 31_000),
    needs_attention: input.needsAttention ?? false,
    unread: input.unread ?? false,
  };
}

function buildThreads(scenario: string): Map<string, MockThread> {
  const map = new Map<string, MockThread>();
  if (scenario === "empty" || scenario === "nohosts") return map;

  const seeds: ThreadSummary[] = [
    summary({
      id: "thr_a",
      host: "atlas",
      harness: "claude_code",
      title: "Fix the flaky auth test",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "awaiting_approval",
      lastActivityMs: 42_000,
      needsAttention: true,
      usage: TOTAL_USAGE,
    }),
    summary({
      id: "thr_b",
      host: "orion",
      harness: "codex",
      title: "Ship relay retry with backoff",
      model: "gpt-5.1-codex",
      tab: "working",
      state: "running_tool",
      workspace: "/home/you/code/h4rn-connect",
      lastActivityMs: 3 * 60_000,
      usage: usage(98_000, 22_600, 184_000, 214_000),
    }),
    summary({
      id: "thr_c",
      host: "atlas-wsl",
      harness: "opencode",
      title: "Audit the dependency tree",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "reading",
      workspace: "/home/you/code/web",
      lastActivityMs: 2 * 60_000,
      usage: usage(31_000, 6_200, 74_000, 58_000),
    }),
    summary({
      id: "thr_k",
      host: "atlas",
      harness: "claude_code",
      title: "Draft the pairing copy",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "thinking",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 12 * 60_000,
      usage: usage(44_000, 9_800, 96_000, 81_000),
    }),
    summary({
      id: "thr_l",
      host: "atlas-wsl",
      harness: "codex",
      title: "Parse WSL distro hosts",
      model: "gpt-5.1-codex-mini",
      tab: "working",
      state: "thinking",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 20 * 60_000,
      usage: usage(18_400, 3_100, 38_000, 22_000),
    }),
    summary({
      id: "thr_d",
      host: "atlas",
      harness: "claude_code",
      title: "Migrate the session store",
      model: "claude-sonnet-5-5",
      tab: "done",
      state: "done",
      lastActivityMs: HOUR,
      unread: true,
      usage: usage(64_200, 18_900, 126_000, 122_000),
    }),
    summary({
      id: "thr_e",
      host: "orion",
      harness: "opencode",
      title: "Update the PR template",
      model: "claude-sonnet-5-5",
      tab: "done",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 3 * HOUR,
      unread: true,
      usage: usage(9_800, 2_200, 31_000, 18_000),
    }),
    summary({
      id: "thr_f",
      host: "buildbox",
      harness: "claude_code",
      title: "Bump the Tauri plugin set",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/desktop",
      lastActivityMs: DAY,
      usage: usage(52_000, 11_200, 98_000, 74_000),
    }),
    summary({
      id: "thr_g",
      host: "atlas",
      harness: "claude_code",
      title: "Review ADR-004 edge cases",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 2 * DAY,
      usage: usage(38_000, 7_400, 82_000, 55_000),
    }),
    summary({
      id: "thr_h",
      host: "orion",
      harness: "codex",
      title: "Trim unused MCP servers",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 3 * DAY,
      usage: usage(21_000, 5_600, 61_000, 39_000),
    }),
    summary({
      id: "thr_i",
      host: "atlas",
      harness: "codex",
      title: "Pin the Tauri sidecar build",
      model: "gpt-5.1-codex",
      tab: "working",
      state: "idle",
      workspace: "/home/you/code/desktop",
      snoozedMs: 16 * HOUR,
      lastActivityMs: 4 * HOUR,
      usage: usage(15_800, 2_400, 44_000, 24_000),
    }),
    summary({
      id: "thr_j",
      host: "buildbox",
      harness: "claude_code",
      title: "Re-run the Windows installer check",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "idle",
      workspace: "/home/you/code/desktop",
      snoozedMs: 3 * HOUR,
      lastActivityMs: 6 * HOUR,
      usage: usage(28_400, 4_900, 71_000, 41_000),
    }),
  ];

  for (const thread of seeds) {
    map.set(thread.thread_id, { summary: thread, events: historyFor(thread) });
  }
  return map;
}

function buildHosts(scenario: string): HostSummary[] {
  if (scenario === "nohosts") return [];
  return [
    {
      host_id: "atlas",
      name: "atlas",
      platform: "linux",
      online: true,
      last_seen: iso(-2_000),
      harnesses: ["claude_code", "codex", "opencode"],
    },
    {
      host_id: "orion",
      name: "orion",
      platform: "macos",
      online: true,
      last_seen: iso(-9_000),
      harnesses: ["codex", "claude_code"],
    },
    {
      host_id: "atlas-wsl",
      name: "atlas / Ubuntu",
      platform: "wsl:Ubuntu 24.04",
      online: true,
      last_seen: iso(-1_000),
      harnesses: ["codex", "opencode"],
    },
    {
      host_id: "buildbox",
      name: "buildbox",
      platform: "linux",
      online: false,
      last_seen: iso(-4 * HOUR),
      harnesses: ["claude_code", "antigravity"],
    },
  ];
}

/** An in-browser daemon used by `?mock=1` for development and screenshots. */
class MockDaemon {
  private readonly threads: Map<string, MockThread>;
  private readonly hosts: HostSummary[];
  private seq = 0;
  private disposed = false;
  private readonly timers = new Set<ReturnType<typeof setTimeout>>();
  private readonly approvals = new Map<string, MockApproval>();
  private readonly startedStreams = new Set<string>();
  private created = 0;

  constructor(
    private readonly handlers: TransportHandlers,
    scenario: string,
  ) {
    this.hosts = buildHosts(scenario);
    this.threads = buildThreads(scenario);
  }

  dispose(): void {
    this.disposed = true;
    for (const timer of this.timers) clearTimeout(timer);
    this.timers.clear();
  }

  receive(data: string): void {
    let frame: ClientFrame;
    try {
      frame = JSON.parse(data) as ClientFrame;
    } catch {
      return;
    }

    this.later(90 + Math.random() * 160, () => {
      if (this.disposed) return;
      this.handle(frame);
    });
  }

  private handle(frame: ClientFrame): void {
    const { body } = frame;
    switch (body.type) {
      case "hello":
        this.ok(frame.id, {
          type: "hello",
          protocol_version: 1,
          server_version: "0.1.0-mock",
          device_id: "mock-device",
          hosts: this.hosts,
        });
        return;
      case "list_hosts":
        this.ok(frame.id, { type: "hosts", hosts: this.hosts });
        return;
      case "list_threads": {
        const filter = body.filter;
        const threads = [...this.threads.values()]
          .map((entry) => entry.summary)
          .filter((thread) => {
            if (!filter.include_snoozed && thread.snoozed_until) return false;
            if (filter.tabs.length > 0 && !filter.tabs.includes(thread.tab)) return false;
            if (filter.host && thread.host !== filter.host) return false;
            if (filter.harness && thread.harness !== filter.harness) return false;
            if (
              filter.search &&
              !thread.title.toLowerCase().includes(filter.search.toLowerCase())
            ) {
              return false;
            }
            return true;
          });
        this.ok(frame.id, { type: "threads", threads });
        return;
      }
      case "get_thread": {
        const entry = this.threads.get(body.thread_id);
        if (!entry) {
          this.err(
            frame.id,
            "not_found",
            `Thread ${body.thread_id} is not on this daemon.`,
            "Refresh the list.",
          );
          return;
        }
        this.ok(frame.id, {
          type: "thread",
          summary: entry.summary,
          events: body.since_seq > 0 ? [] : entry.events,
          next_seq: this.seq + 1,
        });
        if (body.thread_id === "thr_b") this.startLiveStream(body.thread_id);
        return;
      }
      case "subscribe":
        this.ok(frame.id, { type: "ack" });
        return;
      case "create_thread": {
        const id = `thr_new_${++this.created}`;
        const now = Date.now();
        const thread: ThreadSummary = {
          thread_id: id,
          host: body.host,
          harness: body.harness,
          title: body.prompt.split("\n")[0]?.slice(0, 56) || "New thread",
          workspace_path: body.workspace_path,
          model: body.model,
          tab: "working",
          pinned: false,
          snoozed_until: null,
          last_activity: new Date(now).toISOString(),
          agent_state: "thinking",
          usage: usage(0, 0, 0, 0),
          needs_attention: false,
          unread: false,
        };
        this.threads.set(id, { summary: thread, events: [] });
        this.ok(frame.id, { type: "ack" });
        this.later(260, () => this.emit(id, { type: "thread_upserted", ...thread }));
        this.later(520, () => this.runTurn(id, body.prompt));
        return;
      }
      case "send_message": {
        const entry = this.threads.get(body.thread_id);
        if (!entry) {
          this.err(
            frame.id,
            "not_found",
            "That thread is no longer open.",
            "Reload the thread list.",
          );
          return;
        }
        this.ok(frame.id, { type: "ack" });
        entry.summary = {
          ...entry.summary,
          tab: "working",
          agent_state: "thinking",
          unread: false,
          last_activity: iso(),
        };
        this.later(120, () =>
          this.emit(body.thread_id, { type: "thread_upserted", ...entry.summary }),
        );
        this.later(380, () => this.runTurn(body.thread_id, body.text));
        return;
      }
      case "respond_approval": {
        const approval = this.approvals.get(body.approval_id);
        this.ok(frame.id, { type: "ack" });
        if (!approval) return;
        this.approvals.delete(body.approval_id);
        if (body.decision !== "approve") {
          this.later(180, () => this.finishApproval(approval, false));
          return;
        }
        this.later(180, () => this.finishApproval(approval, true));
        return;
      }
      case "interrupt": {
        const entry = this.threads.get(body.thread_id);
        this.ok(frame.id, { type: "ack" });
        if (!entry) return;
        const turn = [...entry.events]
          .reverse()
          .find(
            (event): event is Extract<Event, { type: "turn_started" }> =>
              event.type === "turn_started",
          );
        if (turn) {
          this.emit(body.thread_id, {
            type: "turn_completed",
            thread_id: body.thread_id,
            turn_id: turn.turn_id,
            usage: entry.summary.usage,
            status: "interrupted",
          });
        }
        entry.summary = {
          ...entry.summary,
          tab: "done",
          agent_state: "done",
          unread: true,
          needs_attention: false,
          last_activity: iso(),
        };
        this.emit(body.thread_id, { type: "thread_upserted", ...entry.summary });
        return;
      }
      case "set_thread_tab": {
        const entry = this.threads.get(body.thread_id);
        this.ok(frame.id, { type: "ack" });
        if (!entry) return;
        entry.summary = {
          ...entry.summary,
          tab: body.tab,
          pinned: body.tab === "working" ? body.pinned : false,
          unread: body.tab === "settled" ? false : entry.summary.unread,
          last_activity: iso(),
        };
        this.emit(body.thread_id, { type: "thread_upserted", ...entry.summary });
        return;
      }
      case "snooze": {
        const entry = this.threads.get(body.thread_id);
        this.ok(frame.id, { type: "ack" });
        if (!entry) return;
        entry.summary = {
          ...entry.summary,
          snoozed_until: body.until,
          last_activity: iso(),
        };
        this.emit(body.thread_id, { type: "thread_upserted", ...entry.summary });
        return;
      }
      default:
        this.err(
          frame.id,
          "unsupported",
          `The mock daemon does not implement \`${body.type}\`.`,
          "Phase 1b only exercises threads and approvals.",
        );
    }
  }

  private runTurn(threadId: string, prompt: string): void {
    const entry = this.threads.get(threadId);
    if (!entry || this.disposed) return;

    const turnId = uid("turn");
    const itemId = uid("msg");
    entry.summary = {
      ...entry.summary,
      tab: "working",
      agent_state: "thinking",
      needs_attention: false,
      unread: false,
      last_activity: iso(),
    };
    this.emit(threadId, { type: "thread_upserted", ...entry.summary });
    this.emit(threadId, {
      type: "turn_started",
      thread_id: threadId,
      turn_id: turnId,
      started_at: iso(),
    });
    this.emit(threadId, userMessage(uid("msg"), prompt));

    const reply =
      "\n\nI'll start by mapping the relevant files, then make the smallest change that explains the failure. " +
      "I’ll keep the branch clean and report the command output before touching anything irreversible.";
    let offset = 0;
    const tick = () => {
      if (this.disposed) return;
      if (offset >= reply.length) {
        this.emit(threadId, assistantMessage(itemId, reply));
        this.emit(threadId, {
          type: "tool_call",
          thread_id: threadId,
          turn_id: turnId,
          tool_call: toolCall({
            tool_call_id: uid("tool"),
            name: "Bash",
            summary: "pnpm test --filter auth · 48 passed",
            status: "running",
            completed_at: null,
            started_at: iso(),
          }),
        });
        this.later(1_200, () => {
          if (this.disposed) return;
          const lastTool = [...(this.threads.get(threadId)?.events ?? [])]
            .reverse()
            .find((event) => event.type === "tool_call");
          if (lastTool?.type === "tool_call") {
            this.emit(threadId, {
              type: "tool_call",
              thread_id: threadId,
              turn_id: turnId,
              tool_call: { ...lastTool.tool_call, status: "completed", completed_at: iso() },
            });
          }
          this.emit(threadId, {
            type: "turn_completed",
            thread_id: threadId,
            turn_id: turnId,
            usage: usage(8_400, 1_900, 24_000, 14_000),
            status: "completed",
          });
          const thread = this.threads.get(threadId);
          if (thread) {
            thread.summary = {
              ...thread.summary,
              tab: "done",
              agent_state: "done",
              unread: true,
              last_activity: iso(),
              usage: usage(8_400, 1_900, 24_000, 14_000),
            };
            this.emit(threadId, { type: "thread_upserted", ...thread.summary });
          }
        });
        return;
      }
      const size = 8 + Math.floor(Math.random() * 14);
      const chunk = reply.slice(offset, offset + size);
      offset += size;
      this.emit(threadId, {
        type: "message_delta",
        thread_id: threadId,
        turn_id: turnId,
        item_id: itemId,
        text: chunk,
      });
      this.later(110, tick);
    };
    this.later(320, tick);
  }

  private startLiveStream(threadId: string): void {
    if (this.startedStreams.has(threadId)) return;
    this.startedStreams.add(threadId);

    const entry = this.threads.get(threadId);
    if (!entry) return;
    const turn = [...entry.events]
      .reverse()
      .find(
        (event): event is Extract<Event, { type: "turn_started" }> => event.type === "turn_started",
      );
    if (!turn) return;

    const itemId = `msg_${threadId}_live`;
    const text = continuedText;
    let offset = 0;
    let ticks = 0;

    const tick = () => {
      if (this.disposed) return;
      ticks += 1;
      if (offset >= text.length) {
        this.emit(threadId, assistantMessage(itemId, text));
        this.emit(threadId, {
          type: "agent_state_changed",
          thread_id: threadId,
          state: "thinking",
        });
        const thread = this.threads.get(threadId);
        if (thread) {
          thread.summary = { ...thread.summary, agent_state: "thinking", last_activity: iso() };
          this.emit(threadId, { type: "thread_upserted", ...thread.summary });
        }
        this.startedStreams.delete(threadId);
        return;
      }
      const size = 6 + Math.floor(Math.random() * 12);
      const chunk = text.slice(offset, offset + size);
      offset += size;
      this.emit(threadId, {
        type: "message_delta",
        thread_id: threadId,
        turn_id: turn.turn_id,
        item_id: itemId,
        text: chunk,
      });

      if (ticks % 9 === 0) {
        const tool = toolCall({
          tool_call_id: uid("tool"),
          name: ticks % 18 === 0 ? "Grep" : "Read",
          summary:
            ticks % 18 === 0
              ? '"retry|jitter" · 7 matches'
              : "crates/h4rn-transport/src/session.rs · 318 lines",
          status: "running",
          completed_at: null,
          started_at: iso(),
        });
        this.emit(threadId, {
          type: "tool_call",
          thread_id: threadId,
          turn_id: turn.turn_id,
          tool_call: tool,
        });
        this.emit(threadId, {
          type: "agent_state_changed",
          thread_id: threadId,
          state: "running_tool",
        });
      } else if (ticks % 9 === 4) {
        this.emit(threadId, { type: "agent_state_changed", thread_id: threadId, state: "reading" });
      }

      if (ticks % 6 === 0) {
        const thread = this.threads.get(threadId);
        if (thread) {
          thread.summary = { ...thread.summary, last_activity: iso() };
          this.emit(threadId, { type: "thread_upserted", ...thread.summary });
        }
      }

      this.later(420, tick);
    };

    this.later(420, tick);
  }

  private finishApproval(approval: MockApproval, approved: boolean): void {
    const entry = this.threads.get(approval.threadId);
    if (!entry) return;

    this.emit(approval.threadId, {
      type: "agent_state_changed",
      thread_id: approval.threadId,
      state: approved ? "running_tool" : "done",
    });

    if (!approved) {
      this.emit(approval.threadId, {
        type: "turn_completed",
        thread_id: approval.threadId,
        turn_id: approval.turnId,
        usage: entry.summary.usage,
        status: "interrupted",
      });
      entry.summary = {
        ...entry.summary,
        tab: "done",
        agent_state: "done",
        unread: true,
        needs_attention: false,
        last_activity: iso(),
      };
      this.emit(approval.threadId, { type: "thread_upserted", ...entry.summary });
      return;
    }

    this.emit(approval.threadId, {
      type: "tool_call",
      thread_id: approval.threadId,
      turn_id: approval.turnId,
      tool_call: toolCall({
        tool_call_id: uid("tool"),
        name: "Bash",
        summary: "CI=1 pnpm test --filter auth --repeat 3 · 3 passed",
        status: "completed",
        output: "48 tests passed in 12.4 s (3 runs)",
        started_at: iso(),
        completed_at: iso(1_000),
      }),
    });

    const itemId = uid("msg");
    const text =
      "\n\nThe race is confirmed: all three CI runs failed on the first lookup, then passed once the pool was lazy. I pinned the clock in tests/setup.ts and added a regression test that advances timers before the pool resolves.";
    let offset = 0;
    const tick = () => {
      if (this.disposed) return;
      if (offset >= text.length) {
        this.emit(approval.threadId, assistantMessage(itemId, text));
        this.emit(approval.threadId, {
          type: "turn_completed",
          thread_id: approval.threadId,
          turn_id: approval.turnId,
          usage: usage(24_600, 6_100, 68_000, 58_000),
          status: "completed",
        });
        const thread = this.threads.get(approval.threadId);
        if (thread) {
          thread.summary = {
            ...thread.summary,
            tab: "done",
            agent_state: "done",
            unread: true,
            needs_attention: false,
            last_activity: iso(),
          };
          this.emit(approval.threadId, { type: "thread_upserted", ...thread.summary });
        }
        return;
      }
      const size = 8 + Math.floor(Math.random() * 10);
      const chunk = text.slice(offset, offset + size);
      offset += size;
      this.emit(approval.threadId, {
        type: "message_delta",
        thread_id: approval.threadId,
        turn_id: approval.turnId,
        item_id: itemId,
        text: chunk,
      });
      this.later(110, tick);
    };
    this.later(350, tick);
  }

  private emit(threadId: string, event: Event): void {
    if (this.disposed) return;
    const entry = this.threads.get(threadId);
    if (entry) entry.events.push(event);
    if (event.type === "approval_requested") {
      this.approvals.set(event.approval_id, {
        threadId,
        approvalId: event.approval_id,
        turnId: event.turn_id,
      });
    }
    this.sendEvent(event);
  }

  private sendEvent(event: Event): void {
    this.seq += 1;
    const frame: ServerFrame = { kind: "event", seq: this.seq, event };
    this.handlers.onMessage(JSON.stringify(frame));
  }

  private ok(id: number, result: ResponseBody): void {
    if (this.disposed) return;
    const frame: ServerFrame = { kind: "response", id, result: { Ok: result } };
    this.handlers.onMessage(JSON.stringify(frame));
  }

  private err(id: number, code: ProtoError["code"], message: string, hint: string | null): void {
    if (this.disposed) return;
    const frame: ServerFrame = { kind: "response", id, result: { Err: { code, message, hint } } };
    this.handlers.onMessage(JSON.stringify(frame));
  }

  private later(ms: number, fn: () => void): void {
    if (this.disposed) return;
    const timer = setTimeout(() => {
      this.timers.delete(timer);
      if (!this.disposed) fn();
    }, ms);
    this.timers.add(timer);
  }
}

export function createMockTransport(scenario: string): Transport {
  let daemon: MockDaemon | null = null;
  let openTimer: ReturnType<typeof setTimeout> | null = null;
  let closed = false;

  return {
    kind: "mock",
    connect(handlers) {
      daemon = new MockDaemon(handlers, scenario);
      openTimer = setTimeout(() => {
        openTimer = null;
        if (!closed) handlers.onOpen();
      }, 220);
    },
    send(data) {
      daemon?.receive(data);
    },
    close() {
      closed = true;
      if (openTimer) clearTimeout(openTimer);
      daemon?.dispose();
      daemon = null;
    },
  };
}

export type { TurnStatus };
