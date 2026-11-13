import {
  DEMO_IMAGE_BYTES,
  DEMO_IMAGE_ID,
  DEMO_IMAGE_MIME,
  DEMO_IMAGE_NAME,
  DEMO_LOG_BYTES,
  DEMO_LOG_ID,
  DEMO_LOG_NAME,
} from "@/features/attachments/demo";
import { BinaryKind, decodeFrame, encodeFrame } from "@/features/attachments/frames";
import { createMockFs } from "@/features/ide/mockFs";
import type {
  AgentState,
  Attachment,
  BlobInfo,
  ClientFrame,
  Event,
  HarnessKind,
  HostSummary,
  Item,
  ProtoError,
  ResponseBody,
  SeqEvent,
  ServerFrame,
  TerminalExit,
  TerminalInfo,
  ThreadSummary,
  ToolCall,
  TurnStatus,
  Usage,
} from "@/gen";

import { decodeBinaryFrame, encodeBinaryFrame, BinaryKind as TerminalBinaryKind } from "./binary";
import type { Transport, TransportHandlers } from "./types";

type MockThread = {
  summary: ThreadSummary;
  events: Event[];
  wire: SeqEvent[];
};

type MockApproval = {
  threadId: string;
  approvalId: string;
  turnId: string;
};

type MockTerminal = {
  info: TerminalInfo;
  seq: number;
  scrollback: string;
  line: string;
};

type MockBlob = {
  bytes: Uint8Array;
  name: string;
  mime: string;
  size: number;
  created_at: string;
};

type MockUpload = {
  uploadId: string;
  streamId: number;
  name: string;
  size: number;
  mimeHint: string | null;
  chunks: Uint8Array[];
  received: number;
  nextSeq: number;
  committed: string | null;
};

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

const iso = (offsetMs = 0) => new Date(Date.now() + offsetMs).toISOString();

function uid(prefix: string): string {
  return `${prefix}_${Math.random().toString(36).slice(2, 10)}`;
}

function messageItem(
  itemId: string,
  role: "user" | "assistant" | "system",
  text: string,
  attachments: Attachment[] = [],
): Item {
  return { kind: "message", item_id: itemId, role, text, attachments };
}

function userMessage(
  threadId: string,
  turnId: string,
  itemId: string,
  text: string,
  attachments: Attachment[] = [],
): Event {
  return {
    type: "item_completed",
    thread_id: threadId,
    turn_id: turnId,
    item: messageItem(itemId, "user", text, attachments),
  };
}

function assistantMessage(threadId: string, turnId: string, itemId: string, text: string): Event {
  return {
    type: "item_completed",
    thread_id: threadId,
    turn_id: turnId,
    item: messageItem(itemId, "assistant", text),
  };
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

const TOTAL_USAGE: Usage = usage(3_000, 15_400, 252_000, 420_000);

const continuedText = [
  "The failure only reproduces under the CI runner because the fake clock is installed after the database pool opens, so the first `find` races the timer advance.",
  "\n\nI traced it to `tests/setup.ts`: `vi.useFakeTimers()` runs in `beforeEach`, but `db.connect()` is module-scoped and settles on a real timer from the previous test file.",
  "\n\nI can pin the clock in the setup hook and make the pool lazy, then run the suite three times with `CI=1` to confirm the race is gone.",
  "\n\nOne more check: `session.test.ts` asserts the expiry boundary with `Date.now()` directly. I'd switch that to the injected clock so the test cannot drift when the suite runs near midnight.",
].join("");

function historyFor(thread: ThreadSummary): Event[] {
  const events: Event[] = [];
  const turnId = `turn_${thread.thread_id}_1`;
  const user = userMessage(
    thread.thread_id,
    turnId,
    `msg_${thread.thread_id}_u0`,
    thread.thread_id === "thr_a"
      ? "The auth test fails on CI only. Fix it and add a regression test that pins the clock. Don’t change the public API."
      : thread.thread_id === "thr_b"
        ? "Add exponential backoff with jitter to the relay reconnect, and make sure a resumed stream does not replay events the client already applied."
        : `Work on ${thread.title.toLowerCase()} in ${thread.workspace_path}. Report what you find before changing files.`,
  );
  events.push(user);

  events.push({
    type: "turn_started",
    thread_id: thread.thread_id,
    turn_id: turnId,
    started_at: iso(-8_000),
  });

  if (thread.thread_id === "thr_a") {
    events.push(
      assistantMessage(
        thread.thread_id,
        turnId,
        `msg_${thread.thread_id}_a0`,
        "I’ll reproduce the CI-only failure first, then check how the test setup installs the fake clock.",
      ),
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_read",
          name: "Read",
          summary: "src/auth/session.test.ts",
          output:
            '  1  import { describe, it, expect, beforeAll, vi } from "vitest";\n  2  import { createPool } from "../db/pool";\n  3  import { verifySession } from "./session";\n  …\n 41    it("expires at the boundary", async () => {\n 42      vi.advanceTimersByTime(30_000);\n (142 lines, showing 4)',
          started_at: iso(-7_900),
          completed_at: iso(-7_600),
        }),
      },
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_grep",
          name: "Search",
          summary: "useFakeTimers|Date.now  in src/ tests/",
          output:
            "src/auth/session.test.ts:12   vi.useFakeTimers();\nsrc/db/pool.ts:28              const openedAt = Date.now();\ntests/setup.ts:9               beforeAll(() => vi.useFakeTimers());",
          started_at: iso(-7_500),
          completed_at: iso(-7_300),
        }),
      },
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_setup",
          name: "Read",
          summary: "tests/setup.ts",
          output: "  9  beforeAll(() => vi.useFakeTimers());",
          started_at: iso(-7_200),
          completed_at: iso(-7_100),
        }),
      },
      assistantMessage(
        thread.thread_id,
        turnId,
        `msg_${thread.thread_id}_a1`,
        "The assertion isn’t the problem. `createPool()` reads `Date.now()` at import time, before `beforeAll` installs the fake clock, so the first session lookup races the timer advance on slower CI runners.",
      ),
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_edit_setup",
          name: "Edit",
          summary: "tests/setup.ts",
          started_at: iso(-7_000),
          completed_at: iso(-6_500),
        }),
      },
      {
        type: "tool_call",
        thread_id: thread.thread_id,
        turn_id: turnId,
        tool_call: toolCall({
          tool_call_id: "tool_a_edit_test",
          name: "Edit",
          summary: "src/auth/session.test.ts",
          started_at: iso(-6_400),
          completed_at: iso(-6_000),
        }),
      },
      {
        type: "approval_requested",
        thread_id: thread.thread_id,
        turn_id: turnId,
        approval_id: "ap_a_1",
        kind: "command",
        prompt: "CI=1 pnpm vitest run --project auth --repeat 20",
        options: [],
      },
      { type: "agent_state_changed", thread_id: thread.thread_id, state: "awaiting_approval" },
    );
    return events;
  }

  if (thread.thread_id === "thr_b") {
    events.push(
      assistantMessage(
        thread.thread_id,
        turnId,
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
    events.push({ type: "agent_state_changed", thread_id: thread.thread_id, state: "queued" });
    return events;
  }

  events.push(
    assistantMessage(
      thread.thread_id,
      turnId,
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
    // A small buffer keeps `relativeAge` rounding to the seeded minute even
    // when the store snapshots `now` a few milliseconds before the seed.
    last_activity: iso(-input.lastActivityMs - 5_000),
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
      title: "Fix flaky auth test",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "awaiting_approval",
      lastActivityMs: 2 * 60_000,
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
      lastActivityMs: 18 * 60_000,
      usage: usage(98_000, 11_200, 184_000, 214_000),
    }),
    summary({
      id: "thr_c",
      host: "atlas-wsl",
      harness: "opencode",
      title: "Audit dependency tree",
      model: "claude-sonnet-5-5",
      tab: "working",
      state: "queued",
      workspace: "/home/you/code/web",
      lastActivityMs: 4 * 60_000,
      usage: usage(31_000, 6_200, 74_000, 58_000),
    }),
    summary({
      id: "thr_d",
      host: "orion",
      harness: "codex",
      title: "Migrate session store to Redis",
      model: "gpt-5.1-codex",
      tab: "working",
      state: "rate_limited",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: HOUR,
      usage: usage(44_000, 9_800, 96_000, 81_000),
    }),
    summary({
      id: "thr_e",
      host: "buildbox",
      harness: "codex",
      title: "Parse WSL distro hosts",
      model: "gpt-5.1-codex-mini",
      tab: "working",
      state: "unreachable",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 3 * HOUR,
      usage: usage(18_400, 3_100, 38_000, 22_000),
    }),
    summary({
      id: "thr_f",
      host: "orion",
      harness: "antigravity",
      title: "Draft pairing screen copy",
      model: "gemini-3-pro",
      tab: "working",
      state: "thinking",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 6 * 60_000,
      usage: usage(12_400, 2_800, 31_000, 26_000),
    }),
    summary({
      id: "thr_g",
      host: "orion",
      harness: "opencode",
      title: "Update PR template",
      model: "claude-sonnet-5-5",
      tab: "done",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: HOUR,
      unread: true,
      usage: usage(9_800, 2_200, 31_000, 18_000),
    }),
    summary({
      id: "thr_h",
      host: "atlas",
      harness: "claude_code",
      title: "Bump xterm to 5.5 and fix resize",
      model: "claude-sonnet-5-5",
      tab: "done",
      state: "done",
      workspace: "/home/you/code/desktop",
      lastActivityMs: 3 * HOUR,
      unread: true,
      usage: usage(64_200, 18_900, 126_000, 122_000),
    }),
    summary({
      id: "thr_i",
      host: "atlas",
      harness: "claude_code",
      title: "Index usage events by harness",
      model: "claude-sonnet-5-5",
      tab: "done",
      state: "rate_limited",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 5 * HOUR,
      needsAttention: true,
      usage: usage(52_000, 11_200, 98_000, 74_000),
    }),
    summary({
      id: "thr_j",
      host: "atlas",
      harness: "claude_code",
      title: "Add DPoP proof to device pairing",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 2 * DAY,
      usage: usage(38_000, 7_400, 82_000, 55_000),
    }),
    summary({
      id: "thr_k",
      host: "orion",
      harness: "codex",
      title: "Rename harness instance ids",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 2 * DAY + 3 * HOUR,
      usage: usage(21_000, 5_600, 61_000, 39_000),
    }),
    summary({
      id: "thr_l",
      host: "atlas",
      harness: "claude_code",
      title: "Write CLIProxyAPI probe tests",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 3 * DAY,
      usage: usage(28_400, 4_900, 71_000, 41_000),
    }),
    summary({
      id: "thr_m",
      host: "orion",
      harness: "antigravity",
      title: "Fix terminal paste on Android",
      model: "gemini-3-pro",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 3 * DAY + 5 * HOUR,
      usage: usage(15_800, 2_400, 44_000, 24_000),
    }),
    summary({
      id: "thr_n",
      host: "atlas-wsl",
      harness: "opencode",
      title: "Document OrbitID device grant",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 4 * DAY,
      usage: usage(19_600, 3_900, 52_000, 33_000),
    }),
    summary({
      id: "thr_o",
      host: "atlas",
      harness: "codex",
      title: "Stream backpressure for relay",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn-connect",
      lastActivityMs: 5 * DAY,
      usage: usage(33_200, 8_100, 88_000, 69_000),
    }),
    summary({
      id: "thr_p",
      host: "atlas",
      harness: "claude_code",
      title: "Golden tests for blobatar seeds",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 6 * DAY,
      usage: usage(24_800, 5_100, 66_000, 47_000),
    }),
    summary({
      id: "thr_q",
      host: "orion",
      harness: "codex",
      title: "Retry pairing on expired QR",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 7 * DAY,
      usage: usage(17_200, 3_300, 49_000, 28_000),
    }),
    summary({
      id: "thr_r",
      host: "atlas",
      harness: "claude_code",
      title: "Split connection store from UI state",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 8 * DAY,
      usage: usage(41_600, 9_200, 99_000, 83_000),
    }),
    summary({
      id: "thr_s",
      host: "atlas",
      harness: "claude_code",
      title: "Guard tool output against ANSI",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 9 * DAY,
      usage: usage(26_400, 4_700, 62_000, 44_000),
    }),
    summary({
      id: "thr_t",
      host: "atlas-wsl",
      harness: "opencode",
      title: "Normalize WSL distro labels",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 10 * DAY,
      usage: usage(14_200, 2_600, 37_000, 21_000),
    }),
    summary({
      id: "thr_u",
      host: "orion",
      harness: "codex",
      title: "Cache device keys per host",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 11 * DAY,
      usage: usage(23_800, 4_200, 58_000, 36_000),
    }),
    summary({
      id: "thr_v",
      host: "atlas",
      harness: "claude_code",
      title: "Preserve scroll on tab switch",
      model: "claude-sonnet-5-5",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 12 * DAY,
      usage: usage(22_600, 4_000, 54_000, 37_000),
    }),
    summary({
      id: "thr_y",
      host: "atlas-wsl",
      harness: "codex",
      title: "Trim pairing QR payload",
      model: "gpt-5.1-codex",
      tab: "settled",
      state: "done",
      workspace: "/home/you/code/h4rn",
      lastActivityMs: 13 * DAY,
      usage: usage(18_200, 3_600, 47_000, 29_000),
    }),
    summary({
      id: "thr_w",
      host: "atlas",
      harness: "claude_code",
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
      id: "thr_x",
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
    map.set(thread.thread_id, { summary: thread, events: historyFor(thread), wire: [] });
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
      name: "Ubuntu-24.04",
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
      last_seen: iso(-3 * HOUR - 60_000),
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
  private readonly blobs = new Map<string, MockBlob>();
  private readonly uploads = new Map<string, MockUpload>();
  private readonly uploadsByStream = new Map<number, string>();
  private nextStream = 7_000;
  private readonly startedStreams = new Set<string>();
  private readonly terminals = new Map<string, MockTerminal>();
  private readonly fs = createMockFs();
  private created = 0;
  private nextTerminal = 0;
  private readonly failLists: boolean;
  private readonly failThread: boolean;
  /** Keep the skeleton screens up while still publishing cached hosts. */
  private readonly holdLists: boolean;

  constructor(
    private readonly handlers: TransportHandlers,
    scenario: string,
    private readonly attempt = 0,
  ) {
    this.hosts = buildHosts(scenario);
    this.threads = buildThreads(scenario);
    // The error scenario renders the cached inbox, then fails the reload so
    // the partial-host banner and the thread error state are both visible.
    this.failLists = scenario === "error" && this.attempt > 0;
    this.failThread = scenario === "error";
    this.holdLists = scenario === "loading";
    this.blobs.set(DEMO_IMAGE_ID, {
      bytes: DEMO_IMAGE_BYTES,
      name: DEMO_IMAGE_NAME,
      mime: DEMO_IMAGE_MIME,
      size: DEMO_IMAGE_BYTES.length,
      created_at: iso(-12 * 60_000),
    });
    this.blobs.set(DEMO_LOG_ID, {
      bytes: DEMO_LOG_BYTES,
      name: DEMO_LOG_NAME,
      mime: "text/plain",
      size: DEMO_LOG_BYTES.length,
      created_at: iso(-12 * 60_000),
    });
    for (const entry of this.threads.values()) {
      entry.wire = entry.events.map((event) => {
        this.seq += 1;
        return { seq: this.seq, event };
      });
    }
  }

  dispose(): void {
    this.disposed = true;
    for (const timer of this.timers) clearTimeout(timer);
    this.timers.clear();
  }

  receive(data: string | ArrayBuffer | Uint8Array): void {
    if (typeof data !== "string") {
      const frame = decodeBinaryFrame(data);
      if (frame?.kind === TerminalBinaryKind.PtyInput) {
        const payload = new TextDecoder().decode(frame.payload);
        this.later(8, () => {
          if (!this.disposed) this.handleInput(frame.streamId, payload);
        });
        return;
      }
      this.handleBinary(data);
      return;
    }

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

  /** Echoes keystrokes and answers the handful of commands the mock knows. */
  private handleInput(streamId: number, input: string): void {
    const terminal = [...this.terminals.values()].find(
      (entry) => entry.info.stream_id === streamId,
    );
    if (!terminal) return;
    for (const char of input) {
      if (char === "\r" || char === "\n") {
        this.emitOutput(terminal, "\r\n");
        this.emitOutput(terminal, `${this.runCommand(terminal, terminal.line)}\r\n`);
        terminal.line = "";
        this.emitOutput(terminal, this.prompt(terminal));
      } else if (char === "\u0003") {
        terminal.line = "";
        this.emitOutput(terminal, "^C\r\n");
        this.emitOutput(terminal, this.prompt(terminal));
      } else if (char === "\u007f") {
        if (terminal.line.length > 0) {
          terminal.line = terminal.line.slice(0, -1);
          this.emitOutput(terminal, "\b \b");
        }
      } else if (char === "\t") {
        terminal.line += "  ";
        this.emitOutput(terminal, "  ");
      } else if (char === "\u001b") {
        // Ignore escape sequences (arrow keys) in the mock.
      } else {
        terminal.line += char;
        this.emitOutput(terminal, char);
      }
    }
  }

  private prompt(terminal: MockTerminal): string {
    const cwd = terminal.info.cwd.replace(/^\/home\/[^/]+/, "~");
    return (
      `\u001b[38;5;110m${cwd}\u001b[0m \u001b[38;5;180mh4rn/fix-flaky-auth\u001b[0m\r\n` +
      `\u001b[38;5;114m$\u001b[0m `
    );
  }

  private runCommand(terminal: MockTerminal, line: string): string {
    const command = line.trim();
    if (!command) return "";
    if (command === "clear") {
      terminal.scrollback = "";
      return "\u001b[2J\u001b[H";
    }
    if (command === "pwd") return terminal.info.cwd;
    if (command === "ls" || command === "ls -a") {
      return "AGENTS.md  README.md  apps  crates  docs  package.json  scripts";
    }
    if (command.startsWith("echo ")) return command.slice(5);
    if (command === "git status" || command === "git status -sb") {
      return "On branch dev\nnothing to commit, working tree clean";
    }
    if (command === "git status --short") {
      return " M src/auth/session.test.ts\r\n M tests/setup.ts";
    }
    if (command.includes("vitest")) {
      return [
        "",
        " \u001b[2mRUN  v3.2.4 /home/dev/code/api\u001b[0m",
        "",
        " \u001b[32mok\u001b[0m src/auth/token.test.ts \u001b[2m(12 tests) 84ms\u001b[0m",
        " \u001b[32mok\u001b[0m src/auth/refresh.test.ts \u001b[2m(6 tests) 96ms\u001b[0m",
        " \u001b[32mok\u001b[0m src/auth/session.test.ts \u001b[2m(9 tests) 212ms\u001b[0m   \u001b[2mrun 1 of 3\u001b[0m",
        " \u001b[32mok\u001b[0m src/auth/session.test.ts \u001b[2m(9 tests) 207ms\u001b[0m   \u001b[2mrun 2 of 3\u001b[0m",
        " \u001b[32mok\u001b[0m src/auth/session.test.ts \u001b[2m(9 tests) 219ms\u001b[0m   \u001b[2mrun 3 of 3\u001b[0m",
        "",
        " \u001b[2mTest Files\u001b[0m  \u001b[32m5 passed\u001b[0m (5)",
        "      \u001b[2mTests\u001b[0m  \u001b[32m45 passed\u001b[0m (45)",
        "   \u001b[2mDuration\u001b[0m  2.84s",
      ].join("\r\n");
    }
    if (command === "help") {
      return "mock commands: ls, pwd, echo, git status, clear, help";
    }
    return `sh: command not found: ${command.split(" ")[0]}`;
  }

  private emitOutput(terminal: MockTerminal, text: string): void {
    if (this.disposed) return;
    terminal.seq += 1;
    terminal.scrollback += text;
    if (terminal.scrollback.length > 120_000) {
      terminal.scrollback = terminal.scrollback.slice(-80_000);
    }
    const bytes = encodeBinaryFrame({
      kind: TerminalBinaryKind.PtyOutput,
      streamId: terminal.info.stream_id,
      seq: terminal.seq,
      payload: new TextEncoder().encode(text),
    });
    this.handlers.onBinary?.(bytes.buffer as ArrayBuffer);
  }

  private handle(frame: ClientFrame): void {
    const { body } = frame;
    if (
      this.holdLists &&
      body.type !== "hello" &&
      body.type !== "list_hosts" &&
      body.type !== "subscribe"
    ) {
      return;
    }
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
        if (this.holdLists) return;
        if (this.failLists) {
          this.err(frame.id, "unsupported", "Couldn’t reach buildbox.", "Showing 3 of 4 hosts.");
          return;
        }
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
        if (this.failThread && body.thread_id === "thr_e") {
          this.err(
            frame.id,
            "unsupported",
            "buildbox stopped answering at 09:14 while this turn was running.",
            "The run may still be active there; nothing on this device was lost.",
          );
          return;
        }
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
          events: entry.wire.filter((record) => record.seq > body.since_seq),
          next_seq: entry.wire.at(-1)?.seq ?? body.since_seq,
        });
        if (body.thread_id === "thr_b") this.startLiveStream(body.thread_id);
        return;
      }
      case "subscribe":
        this.ok(frame.id, { type: "ack" });
        return;
      case "create_thread": {
        const id = `thr_new_${++this.created}`;
        const turnId = uid("turn");
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
          agent_state: "queued",
          usage: usage(0, 0, 0, 0),
          needs_attention: false,
          unread: false,
        };
        this.threads.set(id, { summary: thread, events: [], wire: [] });
        this.ok(frame.id, {
          type: "thread_created",
          thread_id: id,
          turn_id: turnId,
          summary: thread,
        });
        this.later(260, () => this.emit(id, { type: "thread_upserted", ...thread }));
        this.later(520, () =>
          this.runTurn(id, body.prompt, turnId, this.attachmentsFor(body.attachments)),
        );
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
        const turnId = uid("turn");
        this.ok(frame.id, {
          type: "message_accepted",
          thread_id: body.thread_id,
          turn_id: turnId,
          seq: this.seq + 1,
        });
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
        this.later(380, () =>
          this.runTurn(body.thread_id, body.text, turnId, this.attachmentsFor(body.attachments)),
        );
        return;
      }
      case "respond_approval": {
        const approval = this.approvals.get(body.approval_id);
        this.ok(frame.id, { type: "ack" });
        if (!approval) return;
        this.approvals.delete(body.approval_id);
        this.later(60, () =>
          this.emit(body.thread_id, {
            type: "approval_resolved",
            thread_id: body.thread_id,
            approval_id: body.approval_id,
            option_id: body.option_id ?? null,
            by_device: "mock-device",
          }),
        );
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
      case "terminal_open": {
        this.nextTerminal += 1;
        this.nextStream += 1;
        const info: TerminalInfo = {
          terminal_id: `term_mock_${this.nextTerminal}`,
          stream_id: this.nextStream,
          thread_id: body.thread_id,
          host: body.host,
          cwd: body.cwd,
          shell: body.shell ?? "/bin/zsh",
          cols: body.cols,
          rows: body.rows,
          alive: true,
          viewers: 1,
          exit: null,
        };
        const terminal: MockTerminal = { info, seq: 0, scrollback: "", line: "" };
        terminal.scrollback = this.prompt(terminal);
        this.terminals.set(info.terminal_id, terminal);
        this.ok(frame.id, { type: "terminal_opened", terminal: info });
        return;
      }
      case "terminal_attach": {
        const terminal = this.terminals.get(body.terminal_id);
        if (!terminal) {
          this.err(
            frame.id,
            "not_found",
            "That terminal is no longer open.",
            "Open a new terminal from the panel.",
          );
          return;
        }
        terminal.info = {
          ...terminal.info,
          cols: body.cols ?? terminal.info.cols,
          rows: body.rows ?? terminal.info.rows,
          viewers: 1,
        };
        this.ok(frame.id, {
          type: "terminal_attached",
          terminal: terminal.info,
          snapshot: terminal.scrollback,
          start_seq: terminal.seq,
        });
        return;
      }
      case "terminal_list": {
        const terminals = [...this.terminals.values()]
          .map((entry) => entry.info)
          .filter((info) => !body.thread_id || info.thread_id === body.thread_id);
        this.ok(frame.id, { type: "terminals", terminals });
        return;
      }
      case "terminal_resize": {
        const terminal = this.terminals.get(body.terminal_id);
        if (terminal) {
          terminal.info = { ...terminal.info, cols: body.cols, rows: body.rows };
        }
        this.ok(frame.id, { type: "ack" });
        return;
      }
      case "terminal_ack": {
        this.ok(frame.id, { type: "ack" });
        return;
      }
      case "terminal_close": {
        const terminal = this.terminals.get(body.terminal_id);
        this.ok(frame.id, { type: "ack" });
        if (!terminal) return;
        this.terminals.delete(body.terminal_id);
        const exit: TerminalExit = { status: 0, signal: null, at: iso() };
        const notice = {
          terminal_id: body.terminal_id,
          thread_id: terminal.info.thread_id,
          exit,
        };
        this.handlers.onMessage(JSON.stringify({ kind: "terminal_event", event: notice }));
        return;
      }
      case "begin_upload": {
        if (body.size > 50 * 1024 * 1024) {
          this.err(
            frame.id,
            "invalid_request",
            `${body.name} is larger than the 50 MiB attachment limit.`,
            "Compress it or split it into smaller files.",
          );
          return;
        }
        const upload: MockUpload = {
          uploadId: uid("upload"),
          streamId: this.nextStream++,
          name: body.name,
          size: body.size,
          mimeHint: body.mime_hint,
          chunks: [],
          received: 0,
          nextSeq: 0,
          committed: null,
        };
        this.uploads.set(upload.uploadId, upload);
        this.uploadsByStream.set(upload.streamId, upload.uploadId);
        this.ok(frame.id, {
          type: "upload_started",
          upload: {
            upload_id: upload.uploadId,
            stream_id: upload.streamId,
            chunk_size: 1024 * 1024,
          },
        });
        return;
      }
      case "commit_upload": {
        const upload = this.uploads.get(body.upload_id);
        if (!upload) {
          this.err(
            frame.id,
            "not_found",
            "That upload is no longer open.",
            "Attach the file again.",
          );
          return;
        }
        const id = body.blake3;
        if (!upload.committed) {
          const bytes = new Uint8Array(upload.received);
          let offset = 0;
          for (const chunk of upload.chunks) {
            bytes.set(chunk, offset);
            offset += chunk.length;
          }
          const name = upload.name.split(/[\\/]/).pop()?.trim() || "attachment";
          this.blobs.set(id, {
            bytes,
            name,
            mime: upload.mimeHint ?? "application/octet-stream",
            size: bytes.length,
            created_at: iso(),
          });
          upload.committed = id;
        }
        const blob = this.blobs.get(id);
        if (!blob) {
          this.err(frame.id, "internal", "The mock blob was not stored.", "Try again.");
          return;
        }
        this.ok(frame.id, { type: "upload_committed", blob: this.blobInfo(id, blob) });
        return;
      }
      case "query_upload": {
        const upload = this.uploads.get(body.upload_id);
        if (!upload) {
          this.err(frame.id, "not_found", "That upload is no longer open.", "Attach it again.");
          return;
        }
        this.ok(frame.id, {
          type: "upload_status",
          upload: {
            upload_id: upload.uploadId,
            stream_id: upload.streamId,
            received_bytes: upload.received,
            size: upload.size,
            committed: upload.committed,
          },
        });
        return;
      }
      case "abort_upload": {
        const upload = this.uploads.get(body.upload_id);
        if (upload) {
          this.uploads.delete(body.upload_id);
          this.uploadsByStream.delete(upload.streamId);
        }
        this.ok(frame.id, { type: "ack" });
        return;
      }
      case "get_blob": {
        const blob = this.blobs.get(body.id);
        if (!blob) {
          this.err(
            frame.id,
            "not_found",
            "That attachment is no longer stored.",
            "Ask for it to be sent again.",
          );
          return;
        }
        const streamId = this.nextStream++;
        this.ok(frame.id, {
          type: "blob",
          download: {
            id: body.id,
            name: blob.name,
            mime: blob.mime,
            size: blob.size,
            offset: 0,
            stream_id: streamId,
            thumbnail: body.thumbnail,
          },
        });
        this.streamBlob(streamId, blob.bytes);
        return;
      }
      default: {
        const handled = this.fs.handle(body, {
          ok: (response) => this.ok(frame.id, response),
          err: (code, message, hint) => this.err(frame.id, code, message, hint),
          later: (ms, fn) => this.later(ms, fn),
          transient: (event) => this.sendTransient(event),
        });
        if (handled) return;
        this.err(
          frame.id,
          "unsupported",
          `The mock daemon does not implement \`${body.type}\`.`,
          "Phase 2 exercises threads, approvals, the IDE and attachments.",
        );
      }
    }
  }

  private blobInfo(id: string, blob: MockBlob): BlobInfo {
    return {
      id,
      name: blob.name,
      mime: blob.mime,
      size: blob.size,
      created_at: blob.created_at,
      has_thumbnail: blob.mime.startsWith("image/"),
    };
  }

  private attachmentsFor(ids: string[]): Attachment[] {
    const attachments: Attachment[] = [];
    for (const id of ids) {
      const blob = this.blobs.get(id);
      if (!blob) continue;
      attachments.push({
        id,
        name: blob.name,
        mime: blob.mime,
        size: blob.size,
        local_path: "",
      });
    }
    return attachments;
  }

  private handleBinary(data: ArrayBuffer | Uint8Array): void {
    const frame = decodeFrame(data);
    if (!frame || frame.kind !== BinaryKind.BlobChunk) return;
    const uploadId = this.uploadsByStream.get(frame.streamId);
    const upload = uploadId ? this.uploads.get(uploadId) : undefined;
    if (!upload) return;
    if (frame.seq < upload.nextSeq) {
      this.ackChunk(frame.streamId, frame.seq);
      return;
    }
    if (frame.seq > upload.nextSeq) return;
    if (upload.received + frame.payload.length > upload.size) return;
    upload.chunks.push(frame.payload.slice());
    upload.received += frame.payload.length;
    upload.nextSeq = frame.seq + 1;
    this.ackChunk(frame.streamId, frame.seq);
  }

  private ackChunk(streamId: number, seq: number): void {
    if (this.disposed) return;
    const encoded = encodeFrame(BinaryKind.Ack, streamId, seq);
    this.handlers.onBinary?.(encoded.buffer as ArrayBuffer);
  }

  private streamBlob(streamId: number, bytes: Uint8Array): void {
    const chunkSize = 256 * 1024;
    let offset = 0;
    const send = () => {
      if (this.disposed || offset >= bytes.length) return;
      const slice = bytes.subarray(offset, Math.min(offset + chunkSize, bytes.length));
      const encoded = encodeFrame(
        BinaryKind.BlobChunk,
        streamId,
        Math.floor(offset / chunkSize),
        slice,
      );
      this.handlers.onBinary?.(encoded.buffer as ArrayBuffer);
      offset += slice.length;
      if (offset < bytes.length) this.later(0, send);
    };
    this.later(0, send);
  }

  private runTurn(
    threadId: string,
    prompt: string,
    turnId: string,
    attachments: Attachment[] = [],
  ): void {
    const entry = this.threads.get(threadId);
    if (!entry || this.disposed) return;

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
    this.emit(threadId, userMessage(threadId, turnId, uid("msg"), prompt, attachments));

    const reply =
      "\n\nI'll start by mapping the relevant files, then make the smallest change that explains the failure. " +
      "I’ll keep the branch clean and report the command output before touching anything irreversible.";
    let offset = 0;
    const tick = () => {
      if (this.disposed) return;
      if (offset >= reply.length) {
        this.emit(threadId, assistantMessage(threadId, turnId, itemId, reply));
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
        this.emit(threadId, assistantMessage(threadId, turn.turn_id, itemId, text));
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
        this.emit(
          approval.threadId,
          assistantMessage(approval.threadId, approval.turnId, itemId, text),
        );
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
    const seq = this.seq + 1;
    this.seq = seq;
    const entry = this.threads.get(threadId);
    if (entry) {
      entry.events.push(event);
      entry.wire.push({ seq, event });
    }
    if (event.type === "approval_requested") {
      this.approvals.set(event.approval_id, {
        threadId,
        approvalId: event.approval_id,
        turnId: event.turn_id,
      });
    }
    this.sendEvent(seq, event);
  }

  private sendEvent(seq: number, event: Event): void {
    const frame: ServerFrame = { kind: "event", seq, event };
    this.handlers.onMessage(JSON.stringify(frame));
  }

  /** Sends a non-journaled event at the current watermark (fs hints, search). */
  private sendTransient(event: Event): void {
    if (this.disposed) return;
    this.sendEvent(this.seq, event);
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

export function createMockTransport(scenario: string, attempt = 0): Transport {
  let daemon: MockDaemon | null = null;
  let openTimer: ReturnType<typeof setTimeout> | null = null;
  let closeTimer: ReturnType<typeof setTimeout> | null = null;
  let closed = false;

  return {
    kind: "mock",
    connect(handlers) {
      // The loading scenario opens the handshake so cached hosts can paint, but
      // it never answers list_threads, which keeps the skeleton screens up.
      daemon = new MockDaemon(handlers, scenario, attempt);
      openTimer = setTimeout(() => {
        openTimer = null;
        if (closed) return;
        handlers.onOpen();
        if (attempt === 0 && (scenario === "offline" || scenario === "error")) {
          closeTimer = setTimeout(
            () => {
              closeTimer = null;
              if (closed) return;
              handlers.onClose({
                code: 1006,
                reason:
                  scenario === "offline"
                    ? "This device lost its connection."
                    : "The relay dropped the connection.",
                wasClean: false,
              });
            },
            scenario === "offline" ? 1_800 : 1_200,
          );
        }
      }, 220);
    },
    send(data: string | ArrayBuffer | Uint8Array) {
      daemon?.receive(data);
    },
    close() {
      closed = true;
      if (openTimer) clearTimeout(openTimer);
      if (closeTimer) clearTimeout(closeTimer);
      daemon?.dispose();
      daemon = null;
    },
  };
}

export type { TurnStatus };
