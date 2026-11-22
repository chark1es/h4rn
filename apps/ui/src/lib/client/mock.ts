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
  AutoResumePolicy,
  BlobInfo,
  ClientFrame,
  ClientRequest,
  DirectDevice,
  DirectoryListing,
  Event,
  FsTurnChanges,
  HarnessKind,
  HarnessUpdateEvent,
  HarnessVersionInfo,
  HostSummary,
  Item,
  LimitStatus,
  LimitsReport,
  McpImportPreview,
  McpRequest,
  McpServerInput,
  McpServerView,
  McpTestCallResult,
  McpToolInfo,
  McpTransportView,
  PairScope,
  Project,
  ProjectSource,
  ProtoError,
  ProxyEnvLine,
  ProxyHarnessOverride,
  ProxyModel,
  ProxyModelCatalog,
  ProxyProfile,
  ProxyProfilePatch,
  ProxyTestReport,
  Resets,
  ResponseBody,
  ResumeSchedule,
  SeqEvent,
  ServerFrame,
  SkillConflict,
  SkillDiff,
  SkillImportPreview,
  SkillInstallState,
  SkillInventory,
  SkillSource,
  SkillSummary,
  SkillSyncStatus,
  SkippedWorktree,
  SubagentView,
  TerminalExit,
  TerminalInfo,
  ThreadSummary,
  ToolCall,
  TrackedWorktree,
  TransportStatus,
  TurnStatus,
  Usage,
  UsageGroup,
  UsageGroupBy,
  UsageReport,
  UsageSeries,
  UsageTotals,
} from "@/gen";
import { decodeBinaryFrame, encodeBinaryFrame, BinaryKind as TerminalBinaryKind } from "./binary";
import { MockForge } from "./mockForge";
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

type SkillsBody = Extract<ClientRequest, { type: `skills_${string}` }>;

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** Every direct pairing starts from view + approve unless scopes are asked for. */
const DEFAULT_DIRECT_SCOPES: PairScope[] = ["view", "approve"];
const PAIR_SCOPE_ORDER: readonly PairScope[] = ["view", "approve", "terminal", "files", "admin"];
const NETWORK_PORT = 47810;

const iso = (offsetMs = 0) => new Date(Date.now() + offsetMs).toISOString();

/**
 * Encodes a mock frame the way the daemon does: `u64` fields arrive as JSON
 * numbers, never as `bigint`, so the browser decodes them with `JSON.parse`.
 */
function wire(frame: unknown): string {
  return JSON.stringify(frame, (_key, value) =>
    typeof value === "bigint" ? Number(value) : value,
  );
}

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
    // The daemon derives these; the mock fixes a plausible tool-free share.
    generation_ms: Math.round(durationMs * 0.8),
    tps: Math.round(output / (durationMs * 0.0008)),
  };
}

const TOTAL_USAGE: Usage = usage(3_000, 15_400, 252_000, 420_000);

/** The files the mock turn touched, matching the mock workspace snapshots. */
function turnChanges(threadId: string, turnId: string): FsTurnChanges {
  return {
    thread_id: threadId,
    turn_id: turnId,
    note: null,
    files: [
      {
        path: "tests/setup.ts",
        added: 4,
        removed: 1,
        before_etag: "mock-before-setup",
        after_etag: "mock-after-setup",
      },
      {
        path: "src/auth/session.test.ts",
        added: 12,
        removed: 2,
        before_etag: "mock-before-test",
        after_etag: "mock-after-test",
      },
    ],
  };
}

const continuedText = [
  "The failure only reproduces under the CI runner because the fake clock is installed after the database pool opens, so the first `find` races the timer advance.",
  "\n\nI traced it to `tests/setup.ts`: `vi.useFakeTimers()` runs in `beforeEach`, but `db.connect()` is module-scoped and settles on a real timer from the previous test file.",
  "\n\nI can pin the clock in the setup hook and make the pool lazy, then run the suite three times with `CI=1` to confirm the race is gone.",
  "\n\nOne more check: `session.test.ts` asserts the expiry boundary with `Date.now()` directly. I'd switch that to the injected clock so the test cannot drift when the suite runs near midnight.",
].join("");

/**
 * A thread's history, stamped around the thread's own last activity so a
 * three-hour-old turn never claims to have started a moment ago.
 */
function historyFor(thread: ThreadSummary): Event[] {
  const shift = Date.parse(thread.last_activity) + 5_000 - Date.now();
  const move = (value: string | null | undefined) =>
    value ? new Date(Date.parse(value) + shift).toISOString() : (value ?? null);
  return buildHistory(thread).map((event) => {
    if (event.type === "turn_started")
      return { ...event, started_at: move(event.started_at) ?? "" };
    if (event.type === "tool_call") {
      return {
        ...event,
        tool_call: {
          ...event.tool_call,
          started_at: move(event.tool_call.started_at) ?? "",
          completed_at: move(event.tool_call.completed_at),
        },
      };
    }
    return event;
  });
}

function buildHistory(thread: ThreadSummary): Event[] {
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
          stats: { kind: "matches", count: 3, unit: "matches" },
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
          stats: { kind: "diff", added: 4, removed: 1 },
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
          stats: { kind: "diff", added: 11, removed: 2 },
        }),
      },
      {
        type: "approval_requested",
        thread_id: thread.thread_id,
        turn_id: turnId,
        approval_id: "ap_a_1",
        subagent_id: null,
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
          summary: '"reconnect|since_seq"',
          stats: { kind: "matches", count: 11, unit: "matches" },
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
        summary: thread.tab === "done" ? "pnpm test --filter session" : "pnpm lint · no findings",
        ...(thread.tab === "done" ? { stats: { kind: "tests", passed: 48, failed: 0 } } : {}),
      }),
    });
    events.push({
      type: "turn_completed",
      thread_id: thread.thread_id,
      turn_id: turnId,
      usage: thread.usage,
      status: "completed",
      changes: turnChanges(thread.thread_id, turnId),
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
  branch?: string;
  pinned?: boolean;
  snoozedMs?: number;
  lastActivityMs: number;
  unread?: boolean;
  needsAttention?: boolean;
  usage?: Usage;
  /** When auto-resume will retry, for rate-limited rows (ADR-024). */
  resumeAtMs?: number;
  limitResetAtMs?: number;
  ttftMs?: number;
  contextWindow?: number;
}): ThreadSummary {
  return {
    thread_id: input.id,
    parent_thread_id: null,
    host: input.host,
    harness: input.harness,
    title: input.title,
    workspace_path: input.workspace ?? "/home/you/code/api",
    ...(input.branch ? { branch: input.branch } : {}),
    model: input.model,
    tab: input.tab,
    placed_by_user: false,
    pinned: input.pinned ?? false,
    snoozed_until: input.snoozedMs ? iso(input.snoozedMs) : null,
    // A small buffer keeps `relativeAge` rounding to the seeded minute even
    // when the store snapshots `now` a few milliseconds before the seed.
    last_activity: iso(-input.lastActivityMs - 5_000),
    agent_state: input.state,
    usage: input.usage ?? usage(12_000, 3_400, 42_000, 31_000),
    needs_attention: input.needsAttention ?? false,
    unread: input.unread ?? false,
    resume_at: input.resumeAtMs === undefined ? null : iso(input.resumeAtMs),
    ttft_ms: input.ttftMs ?? 640,
    context_window: input.contextWindow ?? 200_000,
    context_fill_percent: 6,
    limit:
      input.limitResetAtMs === undefined
        ? null
        : {
            harness: input.harness,
            reset_at: iso(input.limitResetAtMs),
            message: "Weekly limit reached. Auto-resume is armed.",
          },
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
      branch: "h4rn/fix-flaky-auth",
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
      branch: "h4rn/relay-backoff",
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
      resumeAtMs: 35 * 60_000,
      limitResetAtMs: 35 * 60_000,
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
  // A fresh install: this machine and nothing else, so the sidebar footer
  // reads "atlas" the way the empty-state design does.
  const only = scenario === "empty" ? ["atlas"] : null;
  return allMockHosts().filter((host) => only === null || only.includes(host.host_id));
}

function allMockHosts(): HostSummary[] {
  return [
    {
      host_id: "atlas",
      name: "atlas",
      platform: "linux",
      online: true,
      last_seen: iso(-2_000),
      harnesses: ["claude_code", "codex", "opencode"],
      latency_ms: 12,
      path: "direct",
    },
    {
      host_id: "orion",
      name: "orion",
      platform: "macos",
      online: true,
      last_seen: iso(-9_000),
      harnesses: ["codex", "claude_code"],
      latency_ms: 42,
      path: "relay",
    },
    {
      host_id: "atlas-wsl",
      name: "Ubuntu-24.04",
      platform: "wsl:Ubuntu 24.04",
      online: true,
      last_seen: iso(-1_000),
      harnesses: ["codex", "opencode"],
      latency_ms: 18,
      path: "direct",
    },
    {
      host_id: "buildbox",
      name: "buildbox",
      platform: "linux",
      online: false,
      last_seen: iso(-3 * HOUR - 60_000),
      harnesses: ["claude_code", "antigravity"],
      latency_ms: null,
      path: null,
    },
  ];
}

// ---------------------------------------------------------------------------
// Settings → Tools seeds
// ---------------------------------------------------------------------------

const MOCK_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

function mcpServer(
  view: Omit<McpServerView, "id" | "updated_at"> & { updated_at?: string },
): McpServerView {
  return {
    id: `srv_${view.name.replace(/[^a-z0-9]+/gi, "_")}`,
    updated_at: iso(-2 * DAY),
    ...view,
  };
}

function buildMcpServers(): McpServerView[] {
  return [
    mcpServer({
      name: "playwright",
      transport: {
        type: "http",
        url: "http://127.0.0.1:8931/mcp",
        header_keys: ["Authorization"],
      },
      tags: ["browser", "e2e"],
      enabled: {
        by_harness: [
          { harness: "claude_code", enabled: true },
          { harness: "codex", enabled: true },
          { harness: "opencode", enabled: false },
        ],
        by_machine: [{ machine: "orion", enabled: true }],
      },
      status: "ready",
      tool_count: 21,
      last_error: "orion already has a server named playwright with a different configuration.",
      runs_on: null,
      shared_to: ["orion"],
      imported_from: "user",
    }),
    mcpServer({
      name: "linear",
      transport: { type: "http", url: "http://127.0.0.1:8932/mcp", header_keys: [] },
      tags: ["issues"],
      enabled: {
        by_harness: [
          { harness: "claude_code", enabled: true },
          { harness: "codex", enabled: true },
        ],
        by_machine: [],
      },
      status: "ready",
      tool_count: 8,
      last_error: null,
      runs_on: null,
      shared_to: [],
      imported_from: "user",
    }),
    mcpServer({
      name: "postgres-dev",
      transport: {
        type: "stdio",
        command: "mcp-postgres",
        args: ["--readonly"],
        cwd: null,
        env_keys: ["DATABASE_URL"],
      },
      tags: ["database"],
      enabled: {
        by_harness: [{ harness: "codex", enabled: true }],
        by_machine: [],
      },
      status: "remote",
      tool_count: 6,
      last_error: null,
      runs_on: "orion",
      shared_to: [],
      imported_from: "orion",
    }),
    mcpServer({
      name: "sentry",
      transport: { type: "http", url: "http://127.0.0.1:8933/mcp", header_keys: [] },
      tags: ["observability"],
      enabled: {
        by_harness: [
          { harness: "claude_code", enabled: true },
          { harness: "codex", enabled: false },
          { harness: "opencode", enabled: false },
        ],
        by_machine: [],
      },
      status: "error",
      tool_count: 1,
      last_error: "Health check failed: connection refused on 127.0.0.1:8933.",
      runs_on: null,
      shared_to: [],
      imported_from: "user",
    }),
    mcpServer({
      name: "github",
      transport: {
        type: "stdio",
        command: "npx",
        args: ["-y", "@modelcontextprotocol/server-github"],
        cwd: null,
        env_keys: ["GITHUB_TOKEN"],
      },
      tags: ["source"],
      enabled: {
        by_harness: [
          { harness: "claude_code", enabled: true },
          { harness: "codex", enabled: true },
          { harness: "opencode", enabled: false },
        ],
        by_machine: [],
      },
      status: "stopped",
      tool_count: 4,
      last_error: "GITHUB_TOKEN is not configured on atlas.",
      runs_on: null,
      shared_to: [],
      imported_from: "claude_code",
    }),
  ];
}

const MOCK_MCP_TOOLS: Record<string, McpToolInfo[]> = {
  playwright: [
    tool("playwright", "open_page", "Open a URL and return the page state.", true),
    tool("playwright", "click", "Click an element by accessible name.", false),
    tool("playwright", "screenshot", "Capture the current viewport.", true),
    tool("playwright", "snapshot", "Return an accessibility snapshot.", true),
    tool("playwright", "fill", "Fill an input field.", false),
  ],
  linear: [
    tool("linear", "list_issues", "List issues for a team or project.", true),
    tool("linear", "create_issue", "Create an issue with a title and body.", false),
    tool("linear", "comment", "Add a comment to an issue.", false),
  ],
  "postgres-dev": [
    tool("postgres-dev", "query", "Run a read-only SQL query.", true),
    tool("postgres-dev", "tables", "List tables in the current schema.", true),
  ],
  sentry: [tool("sentry", "list_projects", "List projects the token can read.", true)],
  github: [
    tool("github", "search_issues", "Search issues and pull requests.", true),
    tool("github", "get_file", "Read a file from a repository.", true),
    tool("github", "create_pr", "Open a pull request.", false),
  ],
};

function tool(server: string, name: string, description: string, readOnly: boolean): McpToolInfo {
  return {
    name: `${server}__${name}`,
    server,
    tool: name,
    description,
    input_schema: { type: "object", properties: {} },
    read_only: readOnly,
  };
}

function install(
  harness: HarnessKind,
  version: string | null,
  options: { missing?: boolean; enabled?: boolean; managed?: boolean } = {},
): SkillInstallState {
  const suffix =
    harness === "claude_code"
      ? ".claude/skills"
      : harness === "codex"
        ? ".codex/skills"
        : harness === "opencode"
          ? ".config/opencode/skills"
          : ".gemini/antigravity/skills";
  const installed = !options.missing && version !== null;
  return {
    harness,
    installed,
    managed: installed && (options.managed ?? true),
    enabled: options.enabled ?? installed,
    version: installed ? version : null,
    path: installed ? `~/${suffix}/SKILL_NAME` : null,
  };
}

function skillInstalls(
  version: string,
  options: {
    disabled?: HarnessKind[];
    missing?: HarnessKind[];
    versions?: Partial<Record<HarnessKind, string>>;
  } = {},
): SkillInstallState[] {
  return MOCK_HARNESSES.map((harness) =>
    install(harness, options.versions?.[harness] ?? version, {
      ...(options.missing?.includes(harness) ? { missing: true } : {}),
      enabled: !options.disabled?.includes(harness),
    }),
  ).map((entry) => ({
    ...entry,
    path: entry.path
      ? entry.path.replace("SKILL_NAME", entry.harness === "codex" ? "codex" : "browser")
      : null,
  }));
}

function buildSkillInventory(): SkillInventory {
  const rows: Array<{
    name: string;
    description: string;
    version: string;
    source: SkillSource;
    bytes: number;
    installs: SkillInstallState[];
    conflicts?: number;
  }> = [
    {
      name: "browser",
      description: "Drive the in-app browser: open pages, click, type and screenshot.",
      version: "3f6a1c9",
      source: { type: "local_folder", path: "~/code/h4rn/skills/browser" },
      bytes: 18_240,
      installs: skillInstalls("3f6a1c9"),
    },
    {
      name: "docs",
      description: "Read and edit shared documents without losing comments.",
      version: "9b2e4d8",
      source: {
        type: "git",
        url: "git@github.com:acme/skills.git",
        commit: "9b2e4d8a6c1f",
        subdir: "docs",
      },
      bytes: 22_100,
      installs: skillInstalls("9b2e4d8", {
        versions: { codex: "1d7f0a2" },
      }),
      conflicts: 1,
    },
    {
      name: "pdf",
      description: "Read, split, merge and fill PDF files.",
      version: "a4c8e21",
      source: { type: "harness_dir", harness: "claude_code", path: "~/.claude/skills/pdf" },
      bytes: 41_600,
      installs: skillInstalls("a4c8e21"),
    },
    {
      name: "pptx",
      description: "Build and edit presentations as real PowerPoint files.",
      version: "6c1d9f4",
      source: { type: "local_folder", path: "~/code/h4rn/skills/pptx" },
      bytes: 35_800,
      installs: skillInstalls("6c1d9f4"),
    },
    {
      name: "xlsx",
      description: "Work with spreadsheets: formulas, formatting and charts.",
      version: "e8a3b70",
      source: { type: "local_folder", path: "~/code/h4rn/skills/xlsx" },
      bytes: 33_200,
      installs: skillInstalls("e8a3b70", { disabled: ["opencode"] }),
    },
    {
      name: "impeccable",
      description: "Review and shape interfaces against a craft floor.",
      version: "2f7c5a1",
      source: { type: "local_folder", path: "~/code/h4rn/skills/impeccable" },
      bytes: 27_400,
      installs: skillInstalls("2f7c5a1"),
    },
    {
      name: "find-skills",
      description: "Discover installable skills for a task.",
      version: "7d4b9e2",
      source: { type: "connect", skill_id: "skl_find_skills", version: "7d4b9e2" },
      bytes: 12_800,
      installs: skillInstalls("7d4b9e2", { missing: ["antigravity"] }),
    },
    {
      name: "deep-research",
      description: "Research across sources and synthesize a narrative report.",
      version: "c5e0a17",
      source: {
        type: "git",
        url: "https://github.com/acme/research-skills",
        commit: "c5e0a17",
        subdir: null,
      },
      bytes: 29_700,
      installs: skillInstalls("c5e0a17", { disabled: ["codex"] }),
    },
    {
      name: "skill-creator",
      description: "Author, test and benchmark new skills.",
      version: "1a9f3c6",
      source: { type: "local_folder", path: "~/code/h4rn/skills/skill-creator" },
      bytes: 24_900,
      installs: skillInstalls("1a9f3c6", { missing: ["opencode", "antigravity"] }),
    },
  ];

  return {
    skills: rows.map((row) => ({
      id: `skl_${row.name.replace(/-/g, "_")}`,
      name: row.name,
      description: row.description,
      version: row.version,
      source: row.source,
      total_bytes: row.bytes,
      updated_at: iso(-DAY - row.bytes),
      installs: row.installs,
      conflicts: row.conflicts ?? 0,
    })),
    harnesses: [
      {
        harness: "claude_code",
        detected: true,
        user_root: "~/.claude/skills",
        project_root: ".claude/skills",
      },
      {
        harness: "codex",
        detected: true,
        user_root: "~/.codex/skills",
        project_root: ".codex/skills",
      },
      {
        harness: "opencode",
        detected: true,
        user_root: "~/.config/opencode/skills",
        project_root: ".opencode/skills",
      },
      {
        harness: "antigravity",
        detected: false,
        user_root: null,
        project_root: null,
      },
    ],
  };
}

function buildSkillConflicts(): SkillConflict[] {
  return [
    {
      id: "cnf_docs_1",
      skill_id: "skl_docs",
      name: "docs",
      local_version: "9b2e4d8",
      remote_version: "f1a7c30",
      reason: "a different version already uses this name",
      detected_at: iso(-HOUR),
    },
  ];
}

function mockSyncStatus(): SkillSyncStatus {
  return {
    state: "synced",
    last_sync: iso(-2 * HOUR),
    pushed: 2,
    pulled: 1,
    conflicts: 1,
    etag: "mock-etag-7",
    message: null,
    hint: null,
  };
}

/**
 * What a scan of the harness folders finds. `imported` names are in the
 * registry already (so they read as duplicates); `adopted` paths carry h4rn's
 * marker after an import with `manage`.
 */
function buildSkillImportPreview(
  imported: ReadonlySet<string> = new Set(),
  adopted: ReadonlySet<string> = new Set(),
): SkillImportPreview {
  const found = [
    {
      id: "cand_release_notes",
      name: "release-notes",
      description: "Draft release notes from merged pull requests.",
      version: "b7e2d41",
      source_path: "~/.claude/skills/release-notes",
      harness: "claude_code" as const,
      conflict: null,
      duplicate: imported.has("release-notes"),
    },
    {
      id: "cand_changelog",
      name: "changelog",
      description: "Keep CHANGELOG.md in step with tagged releases.",
      version: "4c90aa2",
      source_path: "~/.claude/skills/changelog",
      harness: "claude_code" as const,
      conflict: null,
      duplicate: imported.has("changelog"),
    },
    {
      id: "cand_pdf",
      name: "pdf",
      description: "Read, split, merge and fill PDF files.",
      version: "a4c8e21",
      source_path: "~/.codex/skills/pdf",
      harness: "codex" as const,
      conflict: null,
      duplicate: true,
    },
    {
      id: "cand_browser",
      name: "browser",
      description: "Drive the in-app browser: open pages, click, type and screenshot.",
      version: "3f6a1c0",
      source_path: "~/.claude/skills/browser",
      harness: "claude_code" as const,
      conflict: null,
      duplicate: true,
      managed: true,
    },
    {
      id: "cand_docs",
      name: "docs",
      description: "Read and edit shared documents without losing comments.",
      version: "f1a7c30",
      source_path: "~/.config/opencode/skills/docs",
      harness: "opencode" as const,
      conflict: "a different version already uses this name",
      duplicate: false,
    },
  ];
  const candidates = found.map((candidate) => ({
    ...candidate,
    managed: candidate.managed === true || adopted.has(candidate.source_path),
  }));
  return {
    candidates,
    conflicts: candidates.filter((candidate) => candidate.conflict).length,
    duplicates: candidates.filter((candidate) => candidate.duplicate).length,
  };
}

function buildMcpImportPreview(existing: McpServerView[]): McpImportPreview {
  const conflicting = existing.find((server) => server.name === "playwright");
  const transport: McpTransportView = {
    type: "stdio",
    command: "npx",
    args: ["-y", "@modelcontextprotocol/server-figma"],
    cwd: null,
    env_keys: ["FIGMA_TOKEN"],
  };
  const incoming: McpTransportView = {
    type: "http",
    url: "http://127.0.0.1:8944/mcp",
    header_keys: [],
  };
  return {
    harness: "claude_code",
    origins: ["~/.claude.json", "~/code/api/.mcp.json"],
    candidates: [
      {
        name: "figma",
        transport,
        origin: "~/.claude.json",
        existing: false,
      },
      {
        name: "chrome-devtools",
        transport: incoming,
        origin: "~/code/api/.mcp.json",
        existing: false,
      },
    ],
    conflicts: conflicting
      ? [
          {
            name: "playwright",
            existing: conflicting.transport,
            incoming,
            options: [
              { action: "keep_mine" },
              { action: "use_theirs" },
              { action: "rename", name: "playwright-local" },
              { action: "skip" },
            ],
          },
        ]
      : [],
  };
}

function transportView(input: McpServerInput["transport"]): McpTransportView {
  if (input.type === "stdio") {
    return {
      type: "stdio",
      command: input.command,
      args: input.args,
      cwd: input.cwd,
      env_keys: input.env.map((entry) => entry.name),
    };
  }
  return {
    type: input.type,
    url: input.url,
    header_keys: input.headers.map((entry) => entry.name),
  };
}

function skillFromSource(
  source: SkillSource,
  name: string,
  harnesses: HarnessKind[],
): SkillSummary {
  const version = `${Math.random().toString(16).slice(2, 9)}`;
  const installs = MOCK_HARNESSES.map((harness) =>
    install(harness, harnesses.includes(harness) ? version : null, {
      missing: !harnesses.includes(harness),
    }),
  );
  return {
    id: `skl_${name.replace(/[^a-z0-9]+/gi, "_")}`,
    name,
    description: "Added from the mock daemon.",
    version,
    source,
    total_bytes: 8_400,
    updated_at: iso(),
    installs,
    conflicts: 0,
  };
}

function makeSubagent(
  overrides: Partial<SubagentView> &
    Pick<
      SubagentView,
      "subagent_id" | "parent_thread_id" | "task" | "harness" | "host" | "model" | "state"
    >,
): SubagentView {
  const terminal =
    overrides.state === "completed" ||
    overrides.state === "cancelled" ||
    overrides.state === "failed" ||
    overrides.state === "budget_exceeded";
  const agentState: AgentState =
    overrides.state === "completed"
      ? "done"
      : overrides.state === "running"
        ? "running_tool"
        : overrides.state === "awaiting_approval"
          ? "awaiting_approval"
          : overrides.state === "failed" || overrides.state === "budget_exceeded"
            ? "error"
            : "queued";
  return {
    child_thread_id: null,
    isolation: "shared",
    workspace_path: "/home/you/code/h4rn",
    branch: null,
    depth: 1,
    agent_state: agentState,
    progress: { turns_started: 0, turns_completed: 0, tokens: 0, elapsed_ms: 0, current: null },
    budget: { max_turns: 20, max_wall_time_secs: 900, max_tokens: 50_000 },
    context: { kind: "none" },
    credential: { label: "Claude subscription", shared_with_parent: true },
    created_at: iso(-90_000),
    started_at: null,
    finished_at: terminal ? iso(-10_000) : null,
    result: null,
    ...overrides,
  };
}

/** A realistic delegation tree for the mock inbox. */
function buildSubagents(scenario: string): Map<string, SubagentView> {
  const map = new Map<string, SubagentView>();
  if (scenario === "empty" || scenario === "nohosts") return map;
  const seeds: SubagentView[] = [
    makeSubagent({
      subagent_id: "sa_bisect",
      parent_thread_id: "thr_a",
      child_thread_id: "thr_sa_bisect",
      task: "Bisect the auth fixture change on CI",
      harness: "codex",
      host: "atlas",
      model: "gpt-5.1-codex",
      isolation: "worktree",
      branch: "h4rn/fix-flaky-auth-bisect",
      state: "awaiting_approval",
      depth: 1,
      created_at: iso(-7 * 60_000),
      started_at: iso(-7 * 60_000),
      progress: {
        turns_started: 5,
        turns_completed: 4,
        tokens: 12_800,
        elapsed_ms: 96_000,
        current: "Waiting to run `cargo test -p h4rn-daemon auth`",
      },
    }),
    makeSubagent({
      subagent_id: "sa_retry",
      parent_thread_id: "thr_a",
      child_thread_id: "thr_sa_retry",
      task: "Trace the flaky retry timer in the web suite",
      harness: "opencode",
      host: "atlas-wsl",
      model: "claude-sonnet-5-5",
      state: "running",
      depth: 1,
      created_at: iso(-4 * 60_000),
      started_at: iso(-4 * 60_000),
      progress: {
        turns_started: 3,
        turns_completed: 2,
        tokens: 6_100,
        elapsed_ms: 41_000,
        current: "Reading apps/web/src/retry.test.ts",
      },
    }),
    makeSubagent({
      subagent_id: "sa_retry_fixture",
      parent_thread_id: "thr_a",
      child_thread_id: "thr_sa_retry_fixture",
      task: "Shrink the retry fixture so the timer cannot drift",
      harness: "opencode",
      host: "atlas-wsl",
      model: "claude-sonnet-5-5",
      state: "starting",
      depth: 2,
      created_at: iso(-40_000),
      progress: {
        turns_started: 0,
        turns_completed: 0,
        tokens: 0,
        elapsed_ms: 8_000,
        current: "Preparing workspace",
      },
    }),
    makeSubagent({
      subagent_id: "sa_audit",
      parent_thread_id: "thr_a",
      child_thread_id: "thr_sa_audit",
      task: "Audit the session store for double writes",
      harness: "claude_code",
      host: "atlas",
      model: "claude-sonnet-5-5",
      state: "completed",
      depth: 1,
      created_at: iso(-19 * 60_000),
      started_at: iso(-19 * 60_000),
      finished_at: iso(-11 * 60_000),
      progress: {
        turns_started: 8,
        turns_completed: 8,
        tokens: 31_400,
        elapsed_ms: 214_000,
        current: null,
      },
      result: {
        text: "The store writes the same session twice when a refresh lands mid-flight. One guarded write fixes it; tests added.",
        usage: {
          input_tokens: 22_400,
          output_tokens: 9_000,
          cached_input_tokens: 6_000,
          cost_micros: 0,
          duration_ms: 214_000,
          generation_ms: 180_000,
          tps: 50,
        },
        files_changed: [
          { path: "src/auth/session.ts", status: "M" },
          { path: "src/auth/session.test.ts", status: "A" },
        ],
        error: null,
      },
    }),
  ];
  for (const view of seeds) map.set(view.subagent_id, view);
  return map;
}

/** An in-browser daemon used by `?mock=1` for development and screenshots. */
const MOCK_PROXY_MODELS: ProxyModel[] = [
  {
    id: "claude-sonnet-5-5",
    display_name: "Claude Sonnet 5.5",
    shape: "claude",
    source: "claude",
    alias_of: null,
    owned_by: "anthropic",
    context_length: 200_000n,
    max_output_tokens: 64_000n,
    fork_enriched: true,
  },
  {
    id: "claude-opus-5-5",
    display_name: "Claude Opus 5.5",
    shape: "claude",
    source: "claude",
    alias_of: null,
    owned_by: "anthropic",
    context_length: 200_000n,
    max_output_tokens: 32_000n,
    fork_enriched: true,
  },
  {
    id: "gpt-5.1-codex",
    display_name: "GPT-5.1 Codex",
    shape: "open_ai",
    source: "open_ai",
    alias_of: null,
    owned_by: "openai",
    context_length: 272_000n,
    max_output_tokens: 128_000n,
    fork_enriched: true,
  },
  {
    id: "fast",
    display_name: "Fast",
    shape: "open_ai",
    source: "alias",
    alias_of: "claude-haiku-5-5",
    owned_by: null,
    context_length: 200_000n,
    max_output_tokens: 16_000n,
    fork_enriched: false,
  },
  {
    id: "gemini-3-pro",
    display_name: "Gemini 3 Pro",
    shape: "gemini",
    source: "gemini",
    alias_of: null,
    owned_by: "google",
    context_length: 1_000_000n,
    max_output_tokens: 65_536n,
    fork_enriched: false,
  },
];

function mockProxyCatalog(profileId: string): ProxyModelCatalog {
  return {
    profile_id: profileId,
    models: MOCK_PROXY_MODELS,
    aliases_merged: true,
    fetched_at: iso(),
  };
}

const MOCK_HARNESS_PATHS: Record<HarnessKind, string> = {
  claude_code: "~/.claude/settings.json",
  codex: "~/.codex/config.toml",
  opencode: "~/.config/opencode/opencode.jsonc",
  antigravity: "~/.gemini/settings.json",
};

/** The redacted settings the daemon would report for one harness. */
function mockHarnessRow(
  harness: HarnessKind,
  enabled: boolean,
  defaultModel: string | null,
  verified: boolean,
  note: string | null,
  baseUrl = "http://127.0.0.1:8317",
): ProxyHarnessOverride {
  const model = defaultModel ?? "claude-sonnet-5-5";
  const preview: ProxyEnvLine[] =
    harness === "claude_code"
      ? [
          { name: "ANTHROPIC_BASE_URL", value: baseUrl },
          { name: "ANTHROPIC_AUTH_TOKEN", value: null },
          { name: "ANTHROPIC_MODEL", value: model },
        ]
      : harness === "codex"
        ? [
            { name: "H4RN_CODEX_BASE_URL", value: `${baseUrl}/v1` },
            { name: "CLIPROXY_API_KEY", value: null },
          ]
        : harness === "opencode"
          ? [
              { name: "H4RN_OPENCODE_BASE_URL", value: `${baseUrl}/v1` },
              { name: "H4RN_OPENCODE_API_KEY", value: null },
            ]
          : [
              { name: "GOOGLE_GEMINI_BASE_URL", value: baseUrl },
              { name: "GEMINI_API_KEY", value: null },
            ];
  return {
    harness,
    enabled,
    default_model: defaultModel,
    verified,
    note,
    config_path: MOCK_HARNESS_PATHS[harness],
    preview,
  };
}

function mockProxyProfile(): ProxyProfile {
  return {
    id: "prx_mock",
    name: "CLIProxyAPI on atlas",
    base_url: "http://127.0.0.1:8317",
    default: true,
    has_management_key: true,
    has_api_key: true,
    harnesses: [
      mockHarnessRow("claude_code", true, "claude-sonnet-5-5", true, null),
      mockHarnessRow("codex", false, null, true, null),
      mockHarnessRow("opencode", false, null, true, null),
      mockHarnessRow("antigravity", false, null, false, "Unverified in this repository"),
    ],
    capabilities: {
      reachable: true,
      management: "available",
      version: "v8.0.69",
      commit: "a1b2c3d",
      build_date: "2026-10-30T09:00:00Z",
      plugin_support: true,
      usage_queue: true,
      legacy_usage: false,
      v8_config: true,
      v8_etag: 'W/"mock"',
      performance: true,
      api_key_usage: true,
      credentials_verify: false,
      cooldowns: true,
      quota_providers: true,
      openai_models: true,
      claude_models: true,
      gemini_models: true,
      fork_model_enrichment: true,
      detected_at: iso(-12 * 60_000),
    },
  };
}

function mockProxyReport(): ProxyTestReport {
  return {
    capabilities: mockProxyProfile().capabilities ?? {
      reachable: true,
      management: "available",
      version: null,
      commit: null,
      build_date: null,
      plugin_support: false,
      usage_queue: false,
      legacy_usage: false,
      v8_config: false,
      v8_etag: null,
      performance: false,
      api_key_usage: false,
      credentials_verify: false,
      cooldowns: false,
      quota_providers: false,
      openai_models: false,
      claude_models: false,
      gemini_models: false,
      fork_model_enrichment: false,
      detected_at: iso(),
    },
    latency_ms: 23n,
    diagnostics: [
      {
        level: "info",
        code: "fork_detected",
        message: "This is a CLIProxyAPI fork with the performance and usage-queue routes.",
        hint: "No action needed.",
      },
      {
        level: "warning",
        code: "legacy_usage_missing",
        message: "The ≤ 6.9 usage endpoint is absent, so h4rn counts usage itself.",
        hint: "That is expected on 6.10 and newer.",
      },
    ],
  };
}

function totalOf(usage: Usage): UsageTotals {
  const input = Math.round(usage.input_tokens * 0.82);
  return {
    input_tokens: usage.input_tokens,
    output_tokens: usage.output_tokens,
    cached_input_tokens: Math.round(input * 0.35),
    cached_creation_tokens: Math.round(input * 0.05),
    cost_micros: usage.cost_micros,
    ttft_ms: 640,
    tps: usage.duration_ms > 0 ? (usage.output_tokens / (usage.duration_ms / 1000)) * 0.8 : 0,
    wall_ms: usage.duration_ms,
    generation_ms: Math.round(usage.duration_ms * 0.72),
    active_ms: usage.duration_ms,
    idle_ms: Math.round(usage.duration_ms * 0.35),
    runtime_ms: Math.round(usage.duration_ms * 1.35),
    turns: 1,
  };
}

function addTotals(left: UsageTotals, right: UsageTotals): UsageTotals {
  return {
    input_tokens: left.input_tokens + right.input_tokens,
    output_tokens: left.output_tokens + right.output_tokens,
    cached_input_tokens: left.cached_input_tokens + right.cached_input_tokens,
    cached_creation_tokens: left.cached_creation_tokens + right.cached_creation_tokens,
    cost_micros: left.cost_micros + right.cost_micros,
    ttft_ms: left.ttft_ms + right.ttft_ms,
    tps: left.tps + right.tps,
    wall_ms: left.wall_ms + right.wall_ms,
    generation_ms: left.generation_ms + right.generation_ms,
    active_ms: left.active_ms + right.active_ms,
    idle_ms: left.idle_ms + right.idle_ms,
    runtime_ms: left.runtime_ms + right.runtime_ms,
    turns: left.turns + right.turns,
  };
}

function emptyTotals(): UsageTotals {
  return {
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cached_creation_tokens: 0,
    cost_micros: 0,
    ttft_ms: 0,
    tps: 0,
    wall_ms: 0,
    generation_ms: 0,
    active_ms: 0,
    idle_ms: 0,
    runtime_ms: 0,
    turns: 0,
  };
}

function meanTotals(totals: UsageTotals, turns: number): UsageTotals {
  if (turns <= 0) return totals;
  return {
    ...totals,
    ttft_ms: totals.ttft_ms / turns,
    tps: totals.tps / turns,
  };
}

function scaleTotals(totals: UsageTotals, ratio: number): UsageTotals {
  const scale = (value: number) => Math.round(value * ratio);
  return {
    input_tokens: scale(totals.input_tokens),
    output_tokens: scale(totals.output_tokens),
    cached_input_tokens: scale(totals.cached_input_tokens),
    cached_creation_tokens: scale(totals.cached_creation_tokens),
    cost_micros: scale(totals.cost_micros),
    ttft_ms: totals.ttft_ms,
    tps: totals.tps,
    wall_ms: scale(totals.wall_ms),
    generation_ms: scale(totals.generation_ms),
    active_ms: scale(totals.active_ms),
    idle_ms: scale(totals.idle_ms),
    runtime_ms: scale(totals.runtime_ms),
    turns: totals.turns * ratio,
  };
}

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
  private readonly subagents: Map<string, SubagentView>;
  private readonly fs = createMockFs();
  private mcpServers: McpServerView[];
  private skillInventory: SkillInventory;
  private skillConflicts: SkillConflict[];
  private skillPreview: SkillImportPreview | null = null;
  /** Registered projects and the fake folder tree the add-project browser walks. */
  private projects: Project[] = [
    {
      id: "prj_infra",
      host: "atlas",
      path: "/home/you/code/infra",
      name: "infra",
      created_at: new Date(Date.now() - 86_400_000).toISOString(),
      git: true,
    },
  ];
  private folders = new Map<string, { git: boolean }>([
    ["/home/you", { git: false }],
    ["/home/you/code", { git: false }],
    ["/home/you/code/api", { git: true }],
    ["/home/you/code/h4rn", { git: true }],
    ["/home/you/code/h4rn-connect", { git: true }],
    ["/home/you/code/web", { git: true }],
    ["/home/you/code/desktop", { git: true }],
    ["/home/you/code/infra", { git: true }],
    ["/home/you/code/scratch", { git: false }],
    ["/home/you/Documents", { git: false }],
    ["/home/you/Downloads", { git: false }],
  ]);
  /** Harness folders that carry the h4rn marker after an import with `manage`. */
  private adoptedSkillPaths = new Set<string>();
  private syncEmitted = false;
  private readonly forge = new MockForge();
  private created = 0;
  private nextTerminal = 0;
  private readonly failLists: boolean;
  private readonly failThread: boolean;
  /** Keep the skeleton screens up while still publishing cached hosts. */
  private readonly holdLists: boolean;
  private resumePolicy: AutoResumePolicy = {
    failover: true,
    max_wait_ms: 6 * 60 * 60_000,
    prompt: "The provider limit has reset. Continue the turn from where it stopped.",
    jitter_ms: 15_000,
  };
  private schedules: ResumeSchedule[] = [];
  private limitHistory: LimitStatus[] = [];
  private proxyProfiles: ProxyProfile[] = [];
  private harnessVersions: HarnessVersionInfo[] = mockHarnessVersions();
  private harnessUpdating = new Set<HarnessKind>();
  private worktrees: TrackedWorktree[] = mockWorktrees();
  private proxyCatalogs = new Map<string, ProxyModelCatalog>();
  private networkAccess = { enabled: false };
  /** Devices paired directly; seeded so Settings shows a realistic list. */
  private directDevices: DirectDevice[] = [
    {
      key: "3f9a1c0e".padEnd(64, "0"),
      scopes: ["view", "approve"],
      paired_at: new Date(Date.now() - 3 * 86_400_000).toISOString(),
      connected: false,
    },
  ];

  constructor(
    private readonly handlers: TransportHandlers,
    scenario: string,
    private readonly attempt = 0,
  ) {
    this.hosts = buildHosts(scenario);
    this.threads = buildThreads(scenario);
    this.mcpServers = buildMcpServers();
    this.skillInventory = buildSkillInventory();
    this.skillConflicts = buildSkillConflicts();
    this.subagents = buildSubagents(scenario);
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
    const limited = [...this.threads.values()].find(
      (entry) => entry.summary.limit?.reset_at,
    )?.summary;
    if (limited) {
      this.limitHistory = [
        {
          thread_id: limited.thread_id,
          harness: limited.harness,
          account: "orion",
          provider: "openai",
          model: limited.model,
          reset_at: limited.limit?.reset_at ?? null,
          message: limited.limit?.message ?? "Weekly limit reached.",
          consumed: 100,
          limit: 100,
          observed_at: iso(-60 * 60_000),
          source: "harness",
        },
        {
          thread_id: null,
          harness: "claude_code",
          account: "atlas",
          provider: "anthropic",
          model: "claude-sonnet-5-5",
          reset_at: iso(3 * 24 * 60 * 60_000),
          message: "Weekly budget is 78% consumed.",
          consumed: 78,
          limit: 100,
          observed_at: iso(-3 * 60 * 60_000),
          source: "proxy",
        },
        {
          thread_id: null,
          harness: "antigravity",
          account: "orion",
          provider: "google",
          model: "gemini-3-pro",
          reset_at: iso(11 * 60 * 60_000),
          message: "Daily budget is 23% consumed.",
          consumed: 23,
          limit: 100,
          observed_at: iso(-5 * 60 * 60_000),
          source: "provider",
        },
      ];
      if (limited.resume_at) {
        this.schedules = [
          {
            schedule_id: "sched_mock1",
            thread_id: limited.thread_id,
            scheduled_at: iso(-30 * 60_000),
            resume_at: limited.resume_at,
            attempt: 0,
            state: "scheduled",
            reason: "The provider reset time is known.",
            prompt: this.resumePolicy.prompt,
            failover: {
              model: "claude-sonnet-5-5",
              account: "anthropic-work.json",
              account_label: "anthropic-work",
              reason: "Same family, warmer credential",
            },
            last_error: null,
          },
        ];
      }
    }
    this.proxyProfiles = [mockProxyProfile()];
    this.proxyCatalogs.set("prx_mock", mockProxyCatalog("prx_mock"));
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
      `\u001b[38;5;110m${cwd}\u001b[0m \u001b[38;5;75mh4rn/fix-flaky-auth\u001b[0m\r\n` +
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

  /** The highest durable sequence any thread's mock journal has emitted. */
  private wireHead(): number {
    let head = 0;
    for (const entry of this.threads.values()) {
      for (const record of entry.wire) head = Math.max(head, record.seq);
    }
    return head;
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
    if (body.type.startsWith("skills_")) {
      this.handleSkills(frame.id, body as SkillsBody);
      return;
    }
    switch (body.type) {
      case "hello":
        this.ok(frame.id, {
          type: "hello",
          protocol_version: 1,
          server_version: "1.0.1-mock",
          device_id: "mock-device",
          daemon_machine_id: null,
          local_host: "atlas",
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
            `buildbox stopped answering at ${new Intl.DateTimeFormat(undefined, {
              hour: "2-digit",
              minute: "2-digit",
              hour12: false,
            }).format(new Date(Date.now() - 2 * HOUR))} while this turn was running.`,
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
        this.ok(frame.id, { type: "subscribed", replay_through: this.wireHead() });
        return;
      case "create_thread": {
        const id = `thr_new_${++this.created}`;
        const turnId = uid("turn");
        const now = Date.now();
        const thread: ThreadSummary = {
          thread_id: id,
          parent_thread_id: null,
          host: body.host,
          harness: body.harness,
          title: body.prompt.split("\n")[0]?.slice(0, 56) || "New thread",
          workspace_path: body.workspace_path,
          model: body.model,
          tab: "working",
          placed_by_user: false,
          pinned: false,
          snoozed_until: null,
          last_activity: new Date(now).toISOString(),
          agent_state: "queued",
          usage: usage(0, 0, 0, 0),
          needs_attention: false,
          unread: false,
          resume_at: null,
          limit: null,
          ttft_ms: 0,
          context_window: 200_000,
          context_fill_percent: 0,
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
        this.handlers.onMessage(wire({ kind: "terminal_event", event: notice }));
        return;
      }
      case "subagent": {
        const request = body.request;
        if (request.type === "list_subagents") {
          const views = [...this.subagents.values()]
            .filter(
              (view) =>
                (request.parent_thread === null ||
                  view.parent_thread_id === request.parent_thread) &&
                (request.include_finished ||
                  (view.state !== "completed" &&
                    view.state !== "cancelled" &&
                    view.state !== "failed" &&
                    view.state !== "budget_exceeded")),
            )
            .sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
          this.ok(frame.id, { type: "subagents", subagents: views });
          return;
        }
        if (request.type === "get_subagent") {
          const view = this.subagents.get(request.subagent_id);
          if (!view) {
            this.err(frame.id, "not_found", "That subagent is gone.", null);
            return;
          }
          this.ok(frame.id, { type: "subagent", subagent: view });
          return;
        }
        if (request.type === "cancel_subagent") {
          const view = this.subagents.get(request.subagent_id);
          if (!view) {
            this.err(frame.id, "not_found", "That subagent is gone.", null);
            return;
          }
          const terminal =
            view.state === "completed" ||
            view.state === "cancelled" ||
            view.state === "failed" ||
            view.state === "budget_exceeded";
          if (!terminal) {
            view.state = "cancelled";
            view.agent_state = "idle";
            view.finished_at = iso();
            view.progress = { ...view.progress, current: null };
            this.emitSubagent(view, "subagent_finished");
          }
          this.ok(frame.id, { type: "subagent", subagent: view });
          return;
        }
        const parent = this.threads.get(request.request.parent_thread);
        const task = request.request.task.trim() || "Untitled delegation";
        const view = makeSubagent({
          subagent_id: uid("sa"),
          parent_thread_id: request.request.parent_thread,
          task,
          harness: request.request.harness ?? parent?.summary.harness ?? "claude_code",
          host: request.request.host ?? parent?.summary.host ?? this.hosts[0]?.host_id ?? "atlas",
          model: request.request.model ?? parent?.summary.model ?? "claude-sonnet-5-5",
          isolation: request.request.isolation,
          workspace_path: parent?.summary.workspace_path ?? "/home/you/code/h4rn",
          context: request.request.context,
          budget: {
            max_turns: request.request.budget.max_turns ?? 20,
            max_wall_time_secs: request.request.budget.max_wall_time_secs ?? 900,
            max_tokens: request.request.budget.max_tokens ?? 50_000,
          },
          state: "starting",
          created_at: iso(),
          progress: {
            turns_started: 0,
            turns_completed: 0,
            tokens: 0,
            elapsed_ms: 0,
            current: "Preparing workspace",
          },
        });
        this.subagents.set(view.subagent_id, view);
        this.emitSubagent(view, "subagent_spawned");
        this.ok(frame.id, { type: "subagent", subagent: view });
        this.advanceSubagent(view.subagent_id);
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
      case "mcp": {
        this.handleMcp(frame.id, body.request);
        return;
      }
      case "mcp_list": {
        this.ok(frame.id, { type: "mcp_servers", servers: this.mcpServers });
        return;
      }
      case "mcp_remove": {
        this.mcpServers = this.mcpServers.filter((server) => server.name !== body.name);
        this.ok(frame.id, { type: "ack" });
        return;
      }
      case "get_usage":
        this.ok(frame.id, { type: "usage_report", report: this.usageReport(body) });
        return;
      case "get_limits":
        this.ok(frame.id, { type: "limits", report: this.limitsReport() });
        return;
      case "pairing_confirmations":
        this.ok(frame.id, { type: "pairing_confirmations", confirmations: [] });
        return;
      case "confirm_pairing":
        this.ok(frame.id, { type: "ack" });
        return;
      case "get_network_access":
        this.ok(frame.id, this.networkSnapshot());
        return;
      case "set_network_access": {
        this.networkAccess.enabled = body.enabled;
        this.ok(frame.id, this.networkSnapshot());
        return;
      }
      case "create_direct_pairing": {
        if (!this.networkAccess.enabled) {
          this.err(
            frame.id,
            "conflict",
            "Turn on network access first",
            "Enable it above, then try again.",
          );
          return;
        }
        const requested = body.scopes.length > 0 ? body.scopes : DEFAULT_DIRECT_SCOPES;
        const held = new Set<PairScope>(["view", ...requested]);
        const scopes = PAIR_SCOPE_ORDER.filter((scope) => held.has(scope));
        const expiresAt = Math.floor((Date.now() + 10 * 60_000) / 1_000);
        const addresses = this.networkAddresses();
        const primary = addresses[0] ?? "192.168.1.24:47810";
        const secondary = addresses[1] ?? "100.101.7.42:47810";
        this.ok(frame.id, {
          type: "direct_pairing_offer",
          uri:
            `h4rn://pair?e=${encodeURIComponent(primary)}` +
            `&d=${"ab".repeat(32)}&p=${"cd".repeat(32)}` +
            `&a=${encodeURIComponent(secondary)}&l=This%20computer`,
          expires_at: expiresAt,
          addresses,
          scopes,
        });
        return;
      }
      case "cancel_direct_pairing":
        this.ok(frame.id, { type: "ack" });
        return;
      case "list_direct_devices":
        this.ok(frame.id, { type: "direct_devices", devices: this.directDevices });
        return;
      case "revoke_direct_device": {
        const before = this.directDevices.length;
        this.directDevices = this.directDevices.filter((device) => device.key !== body.key);
        if (this.directDevices.length === before) {
          this.err(
            frame.id,
            "not_found",
            "no device paired directly with that key",
            "Refresh the paired devices list; it may already be removed",
          );
          return;
        }
        this.ok(frame.id, { type: "direct_devices", devices: this.directDevices });
        return;
      }
      case "list_resets":
        this.ok(frame.id, {
          type: "resets",
          resets: { schedules: this.schedules, history: this.limitHistory } satisfies Resets,
        });
        return;
      case "cancel_resume": {
        this.schedules = this.schedules.filter((schedule) => schedule.thread_id !== body.thread_id);
        const thread = this.threads.get(body.thread_id);
        if (thread) {
          thread.summary = { ...thread.summary, resume_at: null };
          this.emit(body.thread_id, { type: "thread_upserted", ...thread.summary });
        }
        this.sendUsage({ type: "schedule_changed", schedule: null });
        this.ok(frame.id, { type: "resume_changed", schedule: null });
        return;
      }
      case "resume_now": {
        this.schedules = this.schedules.filter((schedule) => schedule.thread_id !== body.thread_id);
        const thread = this.threads.get(body.thread_id);
        if (thread) {
          thread.summary = {
            ...thread.summary,
            resume_at: null,
            agent_state: "thinking",
            tab: "working",
          };
          this.emit(body.thread_id, { type: "thread_upserted", ...thread.summary });
        }
        this.sendUsage({ type: "schedule_changed", schedule: null });
        this.ok(frame.id, { type: "resume_changed", schedule: null });
        return;
      }
      case "list_failover_options": {
        const thread = this.threads.get(body.thread_id);
        const current = thread?.summary.model ?? "claude-sonnet-5-5";
        const family = current.split("-")[0] ?? "";
        const options = MOCK_PROXY_MODELS.filter((model) => model.id !== current)
          .sort((a, b) => Number(!a.id.startsWith(family)) - Number(!b.id.startsWith(family)))
          .map((model) => ({
            model: model.id,
            account: "anthropic-work.json" as string | null,
            account_label: "anthropic-work" as string | null,
            reason: model.id.startsWith(family)
              ? `same family as ${current} and not cooling down`
              : `${current} was limited; this model has no active cooldown`,
          }));
        this.ok(frame.id, {
          type: "failover_options",
          options: {
            thread_id: body.thread_id,
            current_model: current,
            default: options[0] ?? null,
            options,
            unavailable: options.length === 0 ? "Every other model is cooling down" : null,
          },
        });
        return;
      }
      case "fail_over": {
        const thread = this.threads.get(body.thread_id);
        this.schedules = this.schedules.filter((schedule) => schedule.thread_id !== body.thread_id);
        if (thread) {
          thread.summary = {
            ...thread.summary,
            model: body.model ?? "claude-opus-5-5",
            resume_at: null,
            agent_state: "thinking",
            tab: "working",
          };
          this.emit(body.thread_id, { type: "thread_upserted", ...thread.summary });
        }
        this.sendUsage({ type: "schedule_changed", schedule: null });
        this.ok(frame.id, { type: "resume_changed", schedule: null });
        return;
      }
      case "set_auto_resume_policy":
        this.resumePolicy = body.policy;
        this.ok(frame.id, { type: "ack" });
        return;
      case "list_projects":
        this.ok(frame.id, { type: "projects", projects: this.projects });
        return;
      case "add_project":
        this.addProject(frame.id, body.host, body.source, body.name);
        return;
      case "rename_project": {
        const project = this.projects.find((entry) => entry.id === body.project_id);
        if (!project) {
          this.err(frame.id, "not_found", `no project ${body.project_id}`, null);
          return;
        }
        project.name = body.name.trim() || project.name;
        this.ok(frame.id, { type: "project", project: { ...project } });
        return;
      }
      case "remove_project":
        this.projects = this.projects.filter((entry) => entry.id !== body.project_id);
        this.ok(frame.id, { type: "ack" });
        return;
      case "browse_directories": {
        const listing = this.browse(body.path);
        if (!listing) {
          this.err(
            frame.id,
            "invalid_request",
            `cannot open ${body.path}`,
            "Check the path, or start from your home folder.",
          );
          return;
        }
        this.ok(frame.id, { type: "directories", listing });
        return;
      }
      case "list_harness_versions":
        this.ok(frame.id, { type: "harness_versions", harnesses: this.harnessVersions });
        return;
      case "update_harness":
        this.updateHarness(frame.id, body.harness);
        return;
      case "list_worktrees":
        this.ok(frame.id, { type: "worktrees", worktrees: this.worktrees });
        return;
      case "clear_worktrees": {
        const { removed, skipped } = clearMockWorktrees(
          this.worktrees,
          body.paths ?? null,
          body.force,
          body.bulk,
        );
        this.worktrees = this.worktrees.filter((entry) => !removed.includes(entry.path));
        this.ok(frame.id, {
          type: "worktrees_cleared",
          removed,
          skipped,
          worktrees: this.worktrees,
        });
        return;
      }
      case "list_proxy_profiles":
        this.ok(frame.id, { type: "proxy_profiles", profiles: this.proxyProfiles });
        return;
      case "add_proxy_profile": {
        const profile: ProxyProfile = {
          id: `prx_${this.proxyProfiles.length + 1}`,
          name: body.profile.name,
          base_url: body.profile.base_url,
          default: body.profile.default || this.proxyProfiles.length === 0,
          has_management_key: Boolean(body.profile.management_key),
          has_api_key: Boolean(body.profile.api_key),
          harnesses: [],
          capabilities: null,
        };
        this.proxyProfiles = [...this.proxyProfiles, profile];
        this.proxyCatalogs.set(profile.id, mockProxyCatalog(profile.id));
        this.ok(frame.id, { type: "proxy_profile", profile });
        return;
      }
      case "update_proxy_profile": {
        const profile = this.proxyProfiles.find((candidate) => candidate.id === body.profile_id);
        if (!profile) {
          this.err(frame.id, "not_found", "That profile is gone.", "Reload profiles.");
          return;
        }
        const patch: ProxyProfilePatch = body.patch;
        const next: ProxyProfile = {
          ...profile,
          name: patch.name ?? profile.name,
          base_url: patch.base_url ?? profile.base_url,
          default: patch.default ?? profile.default,
          has_management_key: patch.clear_management_key
            ? false
            : patch.management_key !== null
              ? true
              : profile.has_management_key,
          has_api_key: patch.clear_api_key
            ? false
            : patch.api_key !== null
              ? true
              : profile.has_api_key,
        };
        this.proxyProfiles = this.proxyProfiles.map((candidate) =>
          candidate.id === profile.id ? next : candidate,
        );
        this.ok(frame.id, { type: "proxy_profile", profile: next });
        return;
      }
      case "remove_proxy_profile": {
        this.proxyProfiles = this.proxyProfiles.filter((profile) => profile.id !== body.profile_id);
        this.proxyCatalogs.delete(body.profile_id);
        this.ok(frame.id, { type: "proxy_profiles", profiles: this.proxyProfiles });
        return;
      }
      case "test_proxy_profile": {
        const report = mockProxyReport();
        this.proxyProfiles = this.proxyProfiles.map((profile) =>
          profile.id === body.profile_id
            ? { ...profile, capabilities: report.capabilities }
            : profile,
        );
        this.ok(frame.id, { type: "proxy_tested", report });
        return;
      }
      case "list_proxy_models": {
        const catalog =
          this.proxyCatalogs.get(body.profile_id) ?? mockProxyCatalog(body.profile_id);
        this.proxyCatalogs.set(body.profile_id, catalog);
        this.ok(frame.id, { type: "proxy_models", catalog });
        return;
      }
      case "set_proxy_harness_override": {
        const current = this.proxyProfiles.find((profile) => profile.id === body.profile_id);
        if (!current) {
          this.err(frame.id, "not_found", "That profile is gone.", "Reload profiles.");
          return;
        }
        const existing = current.harnesses.find((override) => override.harness === body.harness);
        const override = mockHarnessRow(
          body.harness,
          body.enabled,
          body.default_model,
          existing?.verified ?? false,
          existing?.note ?? null,
          current.base_url,
        );
        const next: ProxyProfile = {
          ...current,
          harnesses: existing
            ? current.harnesses.map((candidate) =>
                candidate.harness === body.harness ? override : candidate,
              )
            : [...current.harnesses, override],
        };
        this.proxyProfiles = this.proxyProfiles.map((profile) =>
          profile.id === current.id ? next : profile,
        );
        this.ok(frame.id, { type: "proxy_profile", profile: next });
        return;
      }
      case "apply_proxy_overrides": {
        const profile = this.proxyProfiles.find((candidate) => candidate.id === body.profile_id);
        const routed = new Set(
          (profile?.harnesses ?? []).filter((row) => row.enabled).map((row) => row.harness),
        );
        const live = [...this.threads.values()].filter(
          (thread) => routed.has(thread.summary.harness) && thread.summary.tab === "working",
        );
        this.ok(frame.id, {
          type: "proxy_applied",
          restarted: live.map((thread) => thread.summary.thread_id),
          interrupted: live.map((thread) => thread.summary.thread_id),
        });
        return;
      }
      case "list_proxy_limits": {
        const profile = this.proxyProfiles.find((candidate) => candidate.id === body.profile_id);
        const limits = {
          profile_id: body.profile_id,
          available: Boolean(profile?.has_management_key),
          credentials: profile?.has_management_key
            ? [
                {
                  auth_index: "0",
                  name: "anthropic-work.json",
                  provider: "anthropic",
                  status: "ready",
                  disabled: false,
                  success: 1_284n,
                  failed: 3n,
                  supports_quota: true,
                  cooldowns: [],
                },
                {
                  auth_index: "1",
                  name: "openai-team.json",
                  provider: "openai",
                  status: "cooling down",
                  disabled: false,
                  success: 402n,
                  failed: 18n,
                  supports_quota: true,
                  cooldowns: [
                    {
                      scope: "model",
                      model_key: "gpt-5.1-codex",
                      reason: "429 rate limit",
                      retry_at: iso(8 * 60_000),
                      remaining_seconds: 480n,
                      backoff_level: 2,
                      http_status: 429,
                    },
                  ],
                },
              ]
            : [],
          observed_at: iso(),
          note: profile?.has_management_key
            ? null
            : "Add the management key to read credential health.",
        };
        this.ok(frame.id, { type: "proxy_limits", limits });
        return;
      }
      case "pick_proxy_failover": {
        this.ok(frame.id, {
          type: "proxy_failover",
          choice: {
            model: body.current_model.includes("claude") ? "claude-sonnet-5-5" : "gpt-5.1-codex",
            account: "anthropic-work.json",
            account_label: "anthropic-work",
            reason: "Same family, lowest cooldown backoff",
          },
        });
        return;
      }
      default: {
        const forgeHandled = this.forge.handle(body, {
          ok: (response) => this.ok(frame.id, response),
          err: (code, message, hint) => this.err(frame.id, code, message, hint),
          later: (ms, fn) => this.later(ms, fn),
          transient: (event) => this.sendTransient(event),
        });
        if (forgeHandled) return;
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

  // -------------------------------------------------------------------------
  // MCP
  // -------------------------------------------------------------------------

  private sortedMcp(): McpServerView[] {
    return [...this.mcpServers].sort((left, right) => left.name.localeCompare(right.name));
  }

  // -------------------------------------------------------------------------
  // Network access
  // -------------------------------------------------------------------------

  private networkAddresses(): string[] {
    if (!this.networkAccess.enabled) return [];
    return ["192.168.1.24:47810", "100.101.7.42:47810"];
  }

  private networkSnapshot(): ResponseBody {
    return {
      type: "network_access",
      enabled: this.networkAccess.enabled,
      listening: this.networkAccess.enabled,
      port: NETWORK_PORT,
      addresses: this.networkAddresses(),
      error: null,
    };
  }

  private handleMcp(id: number, request: McpRequest): void {
    switch (request.type) {
      case "list_servers": {
        this.ok(id, { type: "mcp_servers", servers: this.sortedMcp() });
        return;
      }
      case "upsert": {
        const input = request.input;
        const existing = this.mcpServers.find((server) => server.name === input.name);
        const view: McpServerView = {
          id: existing?.id ?? `srv_${input.name.replace(/[^a-z0-9]+/gi, "_")}`,
          name: input.name,
          transport: transportView(input.transport),
          tags: input.tags,
          enabled: input.enabled,
          status: "starting",
          tool_count: existing?.tool_count ?? 0,
          last_error: null,
          runs_on: null,
          shared_to: existing?.shared_to ?? [],
          imported_from: existing?.imported_from ?? "user",
          updated_at: iso(),
        };
        this.mcpServers = [
          ...this.mcpServers.filter((server) => server.name !== input.name),
          view,
        ].sort((left, right) => left.name.localeCompare(right.name));
        this.ok(id, { type: "mcp_server", server: view });
        const tools = MOCK_MCP_TOOLS[input.name] ?? [];
        this.later(450, () => {
          if (this.disposed) return;
          const index = this.mcpServers.findIndex((server) => server.name === input.name);
          if (index < 0) return;
          const current = this.mcpServers[index];
          if (!current) return;
          this.mcpServers[index] = {
            ...current,
            status: "ready",
            tool_count: tools.length,
            last_error: null,
          };
          this.emitGlobal({
            type: "mcp_server_status",
            server: input.name,
            status: "ready",
            tool_count: tools.length,
            message: null,
            at: iso(),
          });
        });
        return;
      }
      case "remove": {
        const before = this.mcpServers.length;
        this.mcpServers = this.mcpServers.filter((server) => server.name !== request.name);
        if (this.mcpServers.length === before) {
          this.err(id, "not_found", `${request.name} is not configured.`, "The list is current.");
          return;
        }
        this.ok(id, { type: "ack" });
        return;
      }
      case "set_enabled": {
        const index = this.mcpServers.findIndex((server) => server.name === request.name);
        if (index < 0) {
          this.err(id, "not_found", `${request.name} is not configured.`, "Refresh the list.");
          return;
        }
        const current = this.mcpServers[index];
        if (!current) return;
        const enabled = {
          by_harness: [...current.enabled.by_harness],
          by_machine: [...current.enabled.by_machine],
        };
        if (request.harness) {
          enabled.by_harness = enabled.by_harness.filter(
            (entry) => entry.harness !== request.harness,
          );
          enabled.by_harness.push({ harness: request.harness, enabled: request.enabled });
        }
        if (request.machine) {
          enabled.by_machine = enabled.by_machine.filter(
            (entry) => entry.machine !== request.machine,
          );
          enabled.by_machine.push({ machine: request.machine, enabled: request.enabled });
        }
        const next = { ...current, enabled, updated_at: iso() };
        this.mcpServers[index] = next;
        this.ok(id, { type: "mcp_server", server: next });
        return;
      }
      case "import": {
        this.ok(id, {
          type: "mcp_import_preview",
          preview: buildMcpImportPreview(this.mcpServers),
        });
        return;
      }
      case "apply_import": {
        const preview = buildMcpImportPreview(this.mcpServers);
        for (const candidate of preview.candidates) {
          if (candidate.existing) continue;
          if (this.mcpServers.some((server) => server.name === candidate.name)) continue;
          this.mcpServers.push({
            id: `srv_${candidate.name.replace(/[^a-z0-9]+/gi, "_")}`,
            name: candidate.name,
            transport: candidate.transport,
            tags: ["imported"],
            enabled: { by_harness: [], by_machine: [] },
            status: "stopped",
            tool_count: 0,
            last_error: null,
            runs_on: null,
            shared_to: [],
            imported_from: "claude_code",
            updated_at: iso(),
          });
        }
        this.mcpServers.sort((left, right) => left.name.localeCompare(right.name));
        this.ok(id, { type: "mcp_servers", servers: this.sortedMcp() });
        return;
      }
      case "resolve_conflict": {
        const index = this.mcpServers.findIndex((server) => server.name === request.name);
        if (index < 0) {
          this.err(id, "not_found", `${request.name} is not configured.`, "Refresh the list.");
          return;
        }
        const current = this.mcpServers[index];
        if (!current) return;
        const preview = buildMcpImportPreview(this.mcpServers);
        const conflict = preview.conflicts.find((entry) => entry.name === request.name);
        let next = { ...current, last_error: null, updated_at: iso() };
        if (request.resolution.action === "use_theirs" && conflict) {
          next = { ...next, transport: conflict.incoming, status: "starting" };
        } else if (request.resolution.action === "rename" && conflict) {
          const name = request.resolution.name.trim();
          if (name) {
            this.mcpServers.push({
              ...next,
              id: `srv_${name.replace(/[^a-z0-9]+/gi, "_")}`,
              name,
              transport: conflict.incoming,
              imported_from: request.harness,
            });
          }
        }
        this.mcpServers[index] = next;
        this.mcpServers.sort((left, right) => left.name.localeCompare(right.name));
        this.ok(id, { type: "mcp_server", server: next });
        return;
      }
      case "share": {
        const index = this.mcpServers.findIndex((server) => server.name === request.name);
        if (index < 0) {
          this.err(id, "not_found", `${request.name} is not configured.`, "Refresh the list.");
          return;
        }
        const current = this.mcpServers[index];
        if (!current) return;
        const shared_to = Array.from(new Set([...current.shared_to, request.to_machine]));
        const next = { ...current, shared_to };
        this.mcpServers[index] = next;
        this.ok(id, { type: "mcp_server", server: next });
        return;
      }
      case "unshare": {
        const index = this.mcpServers.findIndex((server) => server.name === request.name);
        if (index < 0) {
          this.err(id, "not_found", `${request.name} is not configured.`, "Refresh the list.");
          return;
        }
        const current = this.mcpServers[index];
        if (!current) return;
        this.mcpServers[index] = {
          ...current,
          shared_to: current.shared_to.filter((machine) => machine !== request.machine),
        };
        this.ok(id, { type: "ack" });
        return;
      }
      case "list_tools": {
        const tools = MOCK_MCP_TOOLS[request.name] ?? [];
        this.ok(id, { type: "mcp_tools", server: request.name, tools });
        return;
      }
      case "test_call": {
        const isError = request.tool === "fail";
        const duration = 18 + Math.floor(Math.random() * 40);
        const result: McpTestCallResult = {
          content: isError
            ? "The tool reported an error: permission denied."
            : JSON.stringify(
                { ok: true, tool: request.tool, arguments: request.arguments },
                null,
                2,
              ),
          is_error: isError,
          truncated: false,
          duration_ms: duration as unknown as bigint,
        };
        this.ok(id, { type: "mcp_call", result });
        this.later(80, () => {
          if (this.disposed) return;
          this.emitGlobal({
            type: "mcp_tool_called",
            server: request.name,
            tool: request.tool,
            qualified_name: `${request.name}__${request.tool}`,
            caller: { kind: "client" },
            ok: !isError,
            duration_ms: duration as unknown as bigint,
            error: isError ? "permission denied" : null,
            at: iso(),
          });
        });
        return;
      }
      default: {
        this.err(
          id,
          "unsupported",
          `The mock daemon does not implement MCP \`${(request as { type: string }).type}\`.`,
          "Use the list, upsert, toggle, import, share or call actions.",
        );
      }
    }
  }

  // -------------------------------------------------------------------------
  // Skills
  // -------------------------------------------------------------------------

  private handleSkills(id: number, request: SkillsBody): void {
    switch (request.type) {
      case "skills_list": {
        this.ok(id, { type: "skills", inventory: this.skillInventory });
        if (!this.syncEmitted) {
          this.syncEmitted = true;
          this.later(140, () => {
            if (this.disposed) return;
            this.emitGlobal({ type: "sync_status", ...mockSyncStatus() });
          });
        }
        return;
      }
      case "skills_add": {
        const source = request.source;
        const haystack =
          source.type === "local_folder"
            ? source.path
            : source.type === "git"
              ? `${source.url}${source.subdir ?? ""}`
              : source.type === "harness_dir"
                ? source.path
                : (request.name ?? "");
        if (/secret|credential/i.test(`${request.name ?? ""}${haystack}`)) {
          this.err(
            id,
            "invalid_request",
            "This skill contains what looks like a credential.",
            "Remove the secret from every file and add the skill again. h4rn will not install it.",
          );
          return;
        }
        const fallback =
          source.type === "local_folder"
            ? (source.path.split("/").filter(Boolean).at(-1) ?? "skill")
            : source.type === "git"
              ? (source.subdir?.split("/").filter(Boolean).at(-1) ??
                source.url
                  .split("/")
                  .filter(Boolean)
                  .at(-1)
                  ?.replace(/\.git$/, "") ??
                "skill")
              : "skill";
        const name = (request.name?.trim() || fallback).replace(/[^a-z0-9-]+/gi, "-");
        if (this.skillInventory.skills.some((skill) => skill.name === name)) {
          this.err(
            id,
            "conflict",
            `${name} already exists at a different version.`,
            "Open Skills → Conflicts to settle the two versions.",
          );
          return;
        }
        const skill = skillFromSource(source, name, request.harnesses);
        this.skillInventory = {
          ...this.skillInventory,
          skills: [...this.skillInventory.skills, skill].sort((left, right) =>
            left.name.localeCompare(right.name),
          ),
        };
        this.ok(id, { type: "skill_added", skill });
        this.emitGlobal({ type: "skills_changed", host: "atlas", at: iso() });
        return;
      }
      case "skills_update": {
        const index = this.skillInventory.skills.findIndex(
          (skill) => skill.id === request.skill_id,
        );
        const current = this.skillInventory.skills[index];
        if (!current) {
          this.err(id, "not_found", "That skill is not in the registry.", "Refresh the list.");
          return;
        }
        const skill = skillFromSource(request.source, current.name, MOCK_HARNESSES);
        const next = {
          ...skill,
          id: current.id,
          description: current.description,
          total_bytes: current.total_bytes,
        };
        const skills = [...this.skillInventory.skills];
        skills[index] = next;
        this.skillInventory = { ...this.skillInventory, skills };
        this.ok(id, { type: "skill_added", skill: next });
        this.emitGlobal({ type: "skills_changed", host: "atlas", at: iso() });
        return;
      }
      case "skills_remove": {
        this.skillInventory = {
          ...this.skillInventory,
          skills: this.skillInventory.skills.filter((skill) => skill.id !== request.skill_id),
        };
        this.skillConflicts = this.skillConflicts.filter(
          (conflict) => conflict.skill_id !== request.skill_id,
        );
        this.ok(id, { type: "ack" });
        this.emitGlobal({ type: "skills_changed", host: "atlas", at: iso() });
        return;
      }
      case "skills_set_enabled": {
        const index = this.skillInventory.skills.findIndex(
          (skill) => skill.id === request.skill_id,
        );
        const current = this.skillInventory.skills[index];
        if (!current) {
          this.err(id, "not_found", "That skill is not in the registry.", "Refresh the list.");
          return;
        }
        const installs = current.installs.map((entry) =>
          entry.harness === request.harness
            ? {
                ...entry,
                enabled: request.enabled,
                installed: request.enabled ? true : entry.installed,
                managed: request.enabled ? true : entry.managed,
                version: request.enabled ? current.version : entry.version,
                path: request.enabled && !entry.path ? "~/.claude/skills/mock" : entry.path,
              }
            : entry,
        );
        const skill = { ...current, installs };
        const skills = [...this.skillInventory.skills];
        skills[index] = skill;
        this.skillInventory = { ...this.skillInventory, skills };
        this.ok(id, { type: "skill_added", skill });
        return;
      }
      case "skills_import_preview": {
        const preview = buildSkillImportPreview(
          new Set(this.skillInventory.skills.map((skill) => skill.name)),
          this.adoptedSkillPaths,
        );
        this.skillPreview = preview;
        this.ok(id, { type: "skills_import_preview", preview });
        return;
      }
      case "skills_import_commit": {
        const preview = this.skillPreview ?? buildSkillImportPreview();
        const ids = new Set(request.candidate_ids);
        for (const candidate of preview.candidates) {
          if (!ids.has(candidate.id) || candidate.conflict) continue;
          if (request.manage) this.adoptedSkillPaths.add(candidate.source_path);
          if (candidate.duplicate) continue;
          if (this.skillInventory.skills.some((skill) => skill.name === candidate.name)) continue;
          const skill = skillFromSource(
            { type: "harness_dir", harness: candidate.harness, path: candidate.source_path },
            candidate.name,
            [candidate.harness],
          );
          skill.version = candidate.version;
          skill.description = candidate.description;
          this.skillInventory = {
            ...this.skillInventory,
            skills: [...this.skillInventory.skills, skill].sort((left, right) =>
              left.name.localeCompare(right.name),
            ),
          };
        }
        this.ok(id, { type: "ack" });
        this.emitGlobal({ type: "skills_changed", host: "atlas", at: iso() });
        return;
      }
      case "skills_conflicts": {
        this.ok(id, { type: "skill_conflicts", conflicts: this.skillConflicts });
        return;
      }
      case "skills_resolve_conflict": {
        const conflict = this.skillConflicts.find((entry) => entry.id === request.conflict_id);
        if (conflict) {
          if (request.resolution === "keep_remote" && conflict.skill_id) {
            this.skillInventory = {
              ...this.skillInventory,
              skills: this.skillInventory.skills.map((skill) =>
                skill.id === conflict.skill_id
                  ? { ...skill, version: conflict.remote_version }
                  : skill,
              ),
            };
          }
          this.skillConflicts = this.skillConflicts.filter(
            (entry) => entry.id !== request.conflict_id,
          );
        }
        this.ok(id, { type: "ack" });
        this.emitGlobal({ type: "skills_changed", host: "atlas", at: iso() });
        return;
      }
      case "skills_sync_now": {
        this.ok(id, { type: "skill_sync_status", status: mockSyncStatus() });
        return;
      }
      case "skills_diff": {
        const diff: SkillDiff = {
          skill_id: request.skill_id,
          from_version: request.from_version,
          to_version: request.to_version ?? "9b2e4d8",
          files: [
            {
              path: "SKILL.md",
              change: "modified",
              old_blake3: "7c1e",
              new_blake3: "b4a9",
              old_size: 1_840,
              new_size: 2_210,
            },
            {
              path: "references/release-checklist.md",
              change: "added",
              old_blake3: null,
              new_blake3: "0f3d",
              old_size: null,
              new_size: 940,
            },
            {
              path: "assets/old-template.png",
              change: "removed",
              old_blake3: "a8c2",
              new_blake3: null,
              old_size: 18_400,
              new_size: null,
            },
          ],
        };
        this.ok(id, { type: "skill_diff", diff });
        return;
      }
      default: {
        this.err(
          id,
          "unsupported",
          `The mock daemon does not implement \`${(request as { type: string }).type}\`.`,
          "Phase 2 exercises the skill list, toggles, import, sync and diff.",
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

  private mockLimit(threadId: string, turnId: string): void {
    const entry = this.threads.get(threadId);
    if (!entry || this.disposed) return;
    const resetAt = new Date(Date.now() + 90_000).toISOString();
    const status: LimitStatus = {
      thread_id: threadId,
      harness: entry.summary.harness,
      account: "orion",
      provider: "anthropic",
      model: entry.summary.model,
      reset_at: resetAt,
      message: "The provider's short-window budget is exhausted.",
      consumed: 100,
      limit: 100,
      observed_at: iso(),
      source: "harness",
    };
    this.limitHistory = [status, ...this.limitHistory];
    const schedule: ResumeSchedule = {
      schedule_id: `sched_${this.schedules.length + 1}`,
      thread_id: threadId,
      scheduled_at: iso(),
      resume_at: resetAt,
      attempt: 0,
      state: "scheduled",
      reason: "The provider reported a reset time; auto-resume is armed.",
      prompt: this.resumePolicy.prompt,
      failover: this.resumePolicy.failover
        ? {
            model: "claude-sonnet-5-5",
            account: "anthropic-work.json",
            account_label: "anthropic-work",
            reason: "Same family, warmer credential",
          }
        : null,
      last_error: null,
    };
    this.schedules = [schedule, ...this.schedules];
    entry.summary = {
      ...entry.summary,
      tab: "working",
      agent_state: "rate_limited",
      needs_attention: false,
      resume_at: resetAt,
      limit: {
        harness: entry.summary.harness,
        reset_at: resetAt,
        message: status.message,
      },
      last_activity: iso(),
    };
    this.emit(threadId, {
      type: "usage_limit_hit",
      thread_id: threadId,
      reset_at: resetAt,
      auto_resume: true,
    });
    this.emit(threadId, { type: "thread_upserted", ...entry.summary });
    this.sendUsage({ type: "limit_status", status });
    this.sendUsage({ type: "schedule_changed", schedule });
    this.sendUsage({
      type: "usage_tick",
      thread_id: threadId,
      run_id: turnId,
      usage: usage(1_200, 40, 900, 400),
      ttft_ms: 380,
      tps: null,
      elapsed_ms: 900,
      tool_ms: 0,
      at: iso(),
    });
  }

  private sendUsage(event: import("@/gen").UsageStreamEvent): void {
    if (this.disposed) return;
    const frame: ServerFrame = { kind: "usage_event", event };
    this.handlers.onMessage(wire(frame));
  }

  private usageReport(body: Extract<ClientRequest, { type: "get_usage" }>): UsageReport {
    const bucket = body.bucket ?? "day";
    const span = bucket === "hour" ? 60 * 60_000 : 24 * 60 * 60_000;
    const now = Date.now();
    const since = body.range.since ? Date.parse(body.range.since) : now - 14 * span;
    const count = Math.max(1, Math.min(30, Math.ceil((now - since) / span)));
    const lastStart = Math.floor(now / span) * span;
    const starts = Array.from(
      { length: count },
      (_, index) => lastStart - (count - 1 - index) * span,
    );

    const keyOf = (thread: ThreadSummary, grouping: UsageGroupBy): string => {
      switch (grouping) {
        case "harness":
          return thread.harness;
        case "model":
          return thread.model;
        case "machine":
          return thread.host;
        case "workspace":
          return thread.workspace_path;
        case "thread":
          return thread.thread_id;
        default:
          return "all";
      }
    };

    const threads = [...this.threads.values()]
      .map((entry) => entry.summary)
      .filter((thread) => {
        if (body.scope === "thread") return thread.thread_id === body.scope_id;
        if (body.scope === "workspace") return thread.workspace_path === body.scope_id;
        if (body.scope === "machine") return thread.host === body.scope_id;
        return true;
      });

    const seriesGrouping = body.series_group_by ?? body.group_by;
    const seriesTotals = new Map<string, UsageTotals[]>();
    const totals = emptyTotals();
    const groups = new Map<string, UsageTotals>();
    const buckets = starts.map((start) => ({
      start: new Date(start).toISOString(),
      end: new Date(start + span).toISOString(),
      totals: emptyTotals(),
    }));

    threads.forEach((thread, threadIndex) => {
      const threadTotals = totalOf(thread.usage);
      const weights = starts.map((_, index) => 0.4 + ((index * 7 + threadIndex * 13) % 9) / 10);
      const weightSum = weights.reduce((sum, weight) => sum + weight, 0);
      const seriesKey = keyOf(thread, seriesGrouping);
      const keyed = seriesTotals.get(seriesKey) ?? starts.map(() => emptyTotals());
      starts.forEach((_, index) => {
        const ratio = (weights[index] ?? 0) / weightSum;
        const share = scaleTotals(threadTotals, ratio);
        const point = buckets[index];
        if (point) point.totals = addTotals(point.totals, share);
        const keyedPoint = keyed[index];
        if (keyedPoint) keyed[index] = addTotals(keyedPoint, share);
      });
      seriesTotals.set(seriesKey, keyed);

      const groupKey = keyOf(thread, body.group_by);
      groups.set(groupKey, addTotals(groups.get(groupKey) ?? emptyTotals(), threadTotals));
      const added = addTotals(totals, threadTotals);
      Object.assign(totals, added);
    });

    const series: UsageSeries[] = [...seriesTotals.entries()].map(([key, points]) => ({
      key,
      points: points.map((pointTotals, index) => ({
        start: new Date(starts[index] ?? lastStart).toISOString(),
        end: new Date((starts[index] ?? lastStart) + span).toISOString(),
        totals: pointTotals,
      })),
    }));

    const turns = totals.turns;
    const grouped: UsageGroup[] = [...groups.entries()].map(([key, groupTotals]) => ({
      key,
      totals: meanTotals(groupTotals, groupTotals.turns),
    }));

    return {
      scope: body.scope,
      scope_id: body.scope_id,
      range: body.range,
      group_by: body.group_by,
      bucket,
      series_group_by: body.series_group_by,
      totals: meanTotals(totals, turns),
      buckets,
      series,
      groups: grouped,
    };
  }

  private limitsReport(): LimitsReport {
    return { limits: this.limitHistory, policy: this.resumePolicy };
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
    if (prompt.includes("[h4rn-limit]")) {
      this.later(900, () => this.mockLimit(threadId, turnId));
      return;
    }
    const startedAt = Date.now();
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
            summary: "pnpm test --filter auth",
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
              tool_call: {
                ...lastTool.tool_call,
                status: "completed",
                completed_at: iso(),
                stats: { kind: "tests", passed: 48, failed: 0 },
              },
            });
          }
          this.emit(threadId, {
            type: "turn_completed",
            thread_id: threadId,
            turn_id: turnId,
            usage: usage(8_400, 1_900, 24_000, 14_000),
            status: "completed",
            changes: turnChanges(threadId, turnId),
          });
          this.sendUsage({
            type: "usage_tick",
            thread_id: threadId,
            run_id: turnId,
            usage: usage(8_400, 1_900, 24_000, 14_000),
            ttft_ms: 610,
            tps: 61,
            elapsed_ms: 24_000,
            tool_ms: 1_200,
            at: iso(),
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
      const elapsed = Date.now() - startedAt;
      this.sendUsage({
        type: "usage_tick",
        thread_id: threadId,
        run_id: turnId,
        usage: usage(
          2_400,
          Math.max(24, Math.round(offset / 3)),
          elapsed,
          Math.round(offset * 0.9),
        ),
        ttft_ms: 420,
        tps: elapsed > 0 ? Math.round(offset / 3 / (elapsed / 1000)) : null,
        elapsed_ms: elapsed,
        tool_ms: 0,
        at: iso(),
      });
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
          changes: turnChanges(approval.threadId, approval.turnId),
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

  private emitSubagent(
    view: SubagentView,
    type: "subagent_spawned" | "subagent_updated" | "subagent_finished",
  ): void {
    this.emit(view.parent_thread_id, {
      type,
      parent_thread_id: view.parent_thread_id,
      subagent: view,
    });
  }

  /** Walks a freshly spawned child through a short, plausible run. */
  private advanceSubagent(id: string): void {
    this.later(700, () => {
      const view = this.subagents.get(id);
      if (view?.state !== "starting") return;
      view.state = "running";
      view.agent_state = "thinking";
      view.started_at = iso();
      view.child_thread_id = uid("thr");
      view.progress = {
        turns_started: 1,
        turns_completed: 0,
        tokens: 900,
        elapsed_ms: 700,
        current: "Planning the change",
      };
      this.emitSubagent(view, "subagent_updated");
      this.tickSubagent(id, 1);
    });
  }

  private tickSubagent(id: string, turn: number): void {
    this.later(1_400, () => {
      const view = this.subagents.get(id);
      if (view?.state !== "running") return;
      const turns = turn + 1;
      const tokens = view.progress.tokens + 1_800 + turn * 400;
      const elapsed = view.progress.elapsed_ms + 1_400;
      if (turns > 3) {
        view.state = "completed";
        view.agent_state = "done";
        view.finished_at = iso();
        view.progress = {
          turns_started: turns,
          turns_completed: turns,
          tokens,
          elapsed_ms: elapsed,
          current: null,
        };
        view.result = {
          text: `Finished: ${view.task}.`,
          usage: {
            input_tokens: Math.round(tokens * 0.7),
            output_tokens: Math.round(tokens * 0.3),
            cached_input_tokens: 0,
            cost_micros: 0,
            duration_ms: elapsed,
            generation_ms: elapsed,
            tps: 0,
          },
          files_changed: [],
          error: null,
        };
        this.emitSubagent(view, "subagent_finished");
        return;
      }
      view.agent_state = turn % 2 === 0 ? "reading" : "editing";
      view.progress = {
        turns_started: turns,
        turns_completed: turns - 1,
        tokens,
        elapsed_ms: elapsed,
        current: turn % 2 === 0 ? "Reading the diff" : "Applying the change",
      };
      this.emitSubagent(view, "subagent_updated");
      this.tickSubagent(id, turns);
    });
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
    this.handlers.onMessage(wire(frame));
  }

  /** Sends a non-journaled event at the current watermark (fs hints, search). */
  private sendTransient(event: Event): void {
    if (this.disposed) return;
    this.sendEvent(this.seq, event);
  }

  /** Sends a journaled host event and advances the watermark. */
  private emitGlobal(event: Event): void {
    if (this.disposed) return;
    const seq = this.seq + 1;
    this.seq = seq;
    this.sendEvent(seq, event);
  }

  private sendHarness(event: HarnessUpdateEvent): void {
    if (this.disposed) return;
    this.handlers.onMessage(wire({ kind: "harness_event", event }));
  }

  /** Streams a believable update, then re-reads the version like the daemon. */
  private updateHarness(id: number, harness: HarnessKind): void {
    const row = this.harnessVersions.find((candidate) => candidate.harness === harness);
    const runnable = row && row.method.kind !== "manual" && row.method.kind !== "unavailable";
    if (!row || !runnable) {
      this.err(
        id,
        "unsupported",
        "h4rn cannot update this harness automatically on this host.",
        "Run the command shown in Settings yourself.",
      );
      return;
    }
    if (this.harnessUpdating.has(harness)) {
      this.err(id, "conflict", "This harness is already updating.", "Wait for it to finish.");
      return;
    }
    if (harness === "claude_code" && this.mockTurnRunning("claude_code")) {
      this.err(
        id,
        "conflict",
        "Claude Code is working on a turn.",
        "Finish or stop the turn, then try again.",
      );
      return;
    }
    this.harnessUpdating.add(harness);
    this.ok(id, { type: "ack" });
    const lines =
      row.method.kind === "self_update"
        ? ["Checking for updates…", `Downloading ${row.latest}…`, "Installed."]
        : [`added 1 package in 4s`, `${row.method.kind === "npm" ? "npm" : "bun"}: updated`];
    lines.forEach((line, index) => {
      this.later(350 * (index + 1), () =>
        this.sendHarness({ type: "output", harness, stream: "stdout", line }),
      );
    });
    this.later(350 * (lines.length + 1), () => {
      const from = row.installed;
      const to = row.latest;
      this.harnessVersions = this.harnessVersions.map((candidate) =>
        candidate.harness === harness
          ? { ...candidate, installed: to, update_available: false }
          : candidate,
      );
      this.harnessUpdating.delete(harness);
      this.sendHarness({
        type: "finished",
        harness,
        ok: true,
        message: `Updated ${harnessName(harness)} from ${from} to ${to}.`,
        from_version: from,
        to_version: to,
      });
    });
  }

  private mockTurnRunning(harness: HarnessKind): boolean {
    for (const thread of this.threads.values()) {
      if (thread.summary.harness === harness && thread.summary.tab === "working") return true;
    }
    return false;
  }

  private ok(id: number, result: ResponseBody): void {
    if (this.disposed) return;
    const frame: ServerFrame = { kind: "response", id, result: { Ok: result } };
    this.handlers.onMessage(wire(frame));
  }

  private expand(path: string): string {
    const trimmed = path.trim().replace(/\/+$/, "") || "/";
    if (trimmed === "~") return "/home/you";
    if (trimmed.startsWith("~/")) return `/home/you/${trimmed.slice(2)}`;
    return trimmed;
  }

  private browse(path: string | null): DirectoryListing | null {
    const target = this.expand(path ?? "~");
    if (!this.folders.has(target)) return null;
    const entries = [...this.folders.entries()]
      .filter(([candidate]) => {
        const parent = candidate.slice(0, candidate.lastIndexOf("/")) || "/";
        return parent === target && candidate !== target;
      })
      .map(([candidate, info]) => ({
        name: candidate.slice(candidate.lastIndexOf("/") + 1),
        path: candidate,
        git: info.git,
      }))
      .sort((left, right) => left.name.localeCompare(right.name));
    const parent = target.slice(0, target.lastIndexOf("/")) || null;
    return {
      path: target,
      parent: parent && parent !== target ? parent : null,
      home: "/home/you",
      entries,
      git: this.folders.get(target)?.git ?? false,
    };
  }

  private addProject(id: number, host: string, source: ProjectSource, name: string | null): void {
    let path: string;
    let git = false;
    if (source.type === "existing") {
      path = this.expand(source.path);
      if (!this.folders.has(path)) {
        this.err(id, "invalid_request", `${path} is not a folder`, "Pick an existing folder.");
        return;
      }
      git = this.folders.get(path)?.git ?? false;
    } else {
      const parent = this.expand(source.parent);
      const folder =
        source.type === "new_folder"
          ? source.name.trim()
          : ((source.name?.trim() ||
              source.url
                .trim()
                .replace(/\/+$/, "")
                .split(/[/:]/)
                .pop()
                ?.replace(/\.git$/, "")) ??
            "");
      if (!folder || folder.includes("/") || folder.startsWith("-") || folder === "..") {
        this.err(id, "invalid_request", `“${folder}” is not a folder name`, "Use one folder name.");
        return;
      }
      path = `${parent}/${folder}`;
      if (this.folders.has(path)) {
        this.err(
          id,
          "invalid_request",
          `${path} already exists`,
          "Choose another name, or open the existing folder instead.",
        );
        return;
      }
      if (
        source.type === "clone" &&
        !/^(https?:|git@|ssh:|[\w.-]+\/[\w.-]+$)/.test(source.url.trim())
      ) {
        this.err(
          id,
          "invalid_request",
          `“${source.url}” is not a repository URL`,
          "Paste an HTTPS or SSH clone URL, or type owner/repo for GitHub.",
        );
        return;
      }
      git = source.type === "clone" || source.git_init;
      this.folders.set(path, { git });
    }
    const existing = this.projects.find((entry) => entry.path === path);
    if (existing) {
      this.ok(id, { type: "project", project: existing });
      return;
    }
    const project: Project = {
      id: uid("prj"),
      host,
      path,
      name: name?.trim() || path.slice(path.lastIndexOf("/") + 1),
      created_at: iso(),
      git,
    };
    this.projects = [...this.projects, project];
    // A clone takes a moment on a real host; mirror that so the busy state shows.
    const delay = source.type === "clone" ? 900 : 120;
    window.setTimeout(() => this.ok(id, { type: "project", project }), delay);
  }

  private err(id: number, code: ProtoError["code"], message: string, hint: string | null): void {
    if (this.disposed) return;
    const frame: ServerFrame = { kind: "response", id, result: { Err: { code, message, hint } } };
    this.handlers.onMessage(wire(frame));
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

/** The one sample branch with commits not yet merged into HEAD. */
const MOCK_UNMERGED_BRANCH = "h4rn/sa-b71d04aa";

/** The sample agent checkout that holds a submodule of the repository. */
const MOCK_SUBMODULE_PATH = "/home/you/code/api/.claude/worktrees/agent-a41c9e0d";

/**
 * Workspaces of the sample threads. A row that is one of them, or holds one
 * anywhere inside it, is a thread workspace that Clear all leaves alone.
 */
const MOCK_THREAD_WORKSPACES = [
  "/home/you/.t3/worktrees/api/feature-search-index",
  "/home/you/code/api/.claude/worktrees/agent-77e0b3f1/packages/web",
];

/** A path's components, split on both `/` and `\` (Windows paths too). */
function mockPathComponents(path: string): string[] {
  return path.split(/[\\/]/).filter((part) => part !== "");
}

/** Whether Clear all leaves this row alone, compared by whole path components. */
function isMockThreadWorkspace(entry: TrackedWorktree, workspaces: readonly string[]): boolean {
  if (entry.origin === "thread") return true;
  const root = mockPathComponents(entry.path);
  return workspaces.some((workspace) => {
    const parts = mockPathComponents(workspace);
    return parts.length >= root.length && root.every((part, index) => parts[index] === part);
  });
}

/**
 * What a clear does to the sample rows, as the daemon would decide it.
 *
 * Settings sends the explicit paths of the rows the user saw (for Clear all
 * too, with `bulk`); `paths: null` keeps other callers working. Clear all
 * (`bulk`, or no paths) never removes a thread workspace, by origin or by path,
 * and reports each one it leaves out as skipped.
 */
export function clearMockWorktrees(
  worktrees: readonly TrackedWorktree[],
  paths: readonly string[] | null,
  force: boolean,
  bulk: boolean,
  workspaces: readonly string[] = MOCK_THREAD_WORKSPACES,
): { removed: string[]; skipped: SkippedWorktree[] } {
  const removed: string[] = [];
  const skipped: SkippedWorktree[] = [];
  const targets =
    paths ?? worktrees.filter((entry) => entry.status !== "active").map((entry) => entry.path);
  const excludeThreads = bulk || paths === null;
  for (const path of targets) {
    const entry = worktrees.find((candidate) => candidate.path === path);
    const reason = !entry
      ? "not tracked"
      : excludeThreads && isMockThreadWorkspace(entry, workspaces)
        ? "is a thread workspace"
        : worktreeSkipReason(entry, force);
    if (reason) skipped.push({ path, reason });
    else removed.push(path);
  }
  return { removed, skipped };
}

/** Why a clear leaves this worktree in place, or null when it can go. */
function worktreeSkipReason(entry: TrackedWorktree, force: boolean): string | null {
  if (entry.status === "active") {
    return `in use by ${entry.accessed_by.find((a) => a.working)?.title ?? "a thread"}`;
  }
  if (force) return null;
  if (entry.dirty) return "has uncommitted changes";
  if (entry.owned && entry.branch === MOCK_UNMERGED_BRANCH) {
    return `branch ${entry.branch} has unmerged commits`;
  }
  if (entry.path === MOCK_SUBMODULE_PATH) return "contains submodules";
  return null;
}

/**
 * Six worktrees: one in use, then a kept branch, an orphan, two agent
 * checkouts (one holding a thread's workspace) and a dirty one.
 */
function mockWorktrees(): TrackedWorktree[] {
  const repo = "/home/you/code/api";
  const minutes = (ago: number) => new Date(Date.now() - ago * 60_000).toISOString();
  const access = (title: string, working: boolean, ago: number) => ({
    thread_id: `mock-wt-${title.length}-${ago}`,
    title,
    subagent_id: null,
    working,
    last_seen: minutes(ago),
  });
  return [
    {
      path: "/home/you/.local/share/h4rn/subagents/3f9a1c2e/worktree",
      repo,
      branch: "h4rn/sa-3f9a1c2e",
      origin: "subagent",
      status: "active",
      exists: true,
      dirty: true,
      owned: true,
      subagent_id: "3f9a1c2e",
      task: "Add rate limiting to the session endpoints",
      accessed_by: [access("Rate limit session endpoints", true, 1)],
      first_seen: minutes(40),
      last_seen: minutes(1),
    },
    {
      path: "/home/you/.local/share/h4rn/subagents/b71d04aa/worktree",
      repo,
      branch: "h4rn/sa-b71d04aa",
      origin: "subagent",
      status: "orphaned",
      exists: true,
      dirty: false,
      owned: true,
      subagent_id: "b71d04aa",
      task: "Split the billing module",
      accessed_by: [access("Split the billing module", false, 1500)],
      first_seen: minutes(1600),
      last_seen: minutes(1500),
    },
    {
      path: "/home/you/code/api/.claude/worktrees/agent-a41c9e0d",
      repo,
      branch: "worktree-agent-a41c9e0d",
      origin: "harness",
      status: "settled",
      exists: true,
      dirty: false,
      owned: false,
      subagent_id: null,
      task: null,
      accessed_by: [access("Audit the auth middleware", false, 300)],
      first_seen: minutes(320),
      last_seen: minutes(300),
    },
    {
      path: "/home/you/code/api/.claude/worktrees/agent-77e0b3f1",
      repo,
      branch: "worktree-agent-77e0b3f1",
      origin: "harness",
      status: "settled",
      exists: true,
      dirty: false,
      owned: false,
      subagent_id: null,
      task: null,
      accessed_by: [access("Tidy the web package", false, 900)],
      first_seen: minutes(960),
      last_seen: minutes(900),
    },
    {
      path: "/home/you/.t3/worktrees/api/feature-search-index",
      repo,
      branch: "feature/search-index",
      origin: "thread",
      status: "settled",
      exists: true,
      dirty: true,
      owned: false,
      subagent_id: null,
      task: null,
      accessed_by: [access("Index usage events by harness", false, 2900)],
      first_seen: minutes(4000),
      last_seen: minutes(2900),
    },
    {
      path: "/home/you/.local/share/h4rn/subagents/0c52e7b8/worktree",
      repo,
      branch: "h4rn/sa-0c52e7b8",
      origin: "subagent",
      status: "settled",
      exists: false,
      dirty: null,
      owned: true,
      subagent_id: "0c52e7b8",
      task: "Rename harness instance ids",
      accessed_by: [access("Rename harness instance ids", false, 6000)],
      first_seen: minutes(6100),
      last_seen: minutes(6000),
    },
  ];
}

function harnessName(harness: HarnessKind): string {
  return {
    claude_code: "Claude Code",
    codex: "Codex",
    opencode: "OpenCode",
    antigravity: "Antigravity",
  }[harness];
}

/** Four harnesses in four states: self-update, npm, hand-run, and no source. */
function mockHarnessVersions(): HarnessVersionInfo[] {
  return [
    {
      harness: "claude_code",
      installed: "2.1.323",
      latest: "2.2.0",
      update_available: true,
      method: {
        kind: "self_update",
        program: "/home/you/.local/bin/claude",
        args: ["update"],
        command: "claude update",
      },
      path: "/home/you/.local/bin/claude",
    },
    {
      harness: "codex",
      installed: "0.168.0",
      latest: "0.169.0",
      update_available: true,
      method: { kind: "npm", package: "@openai/codex", command: "npm i -g @openai/codex@latest" },
      path: "/usr/local/lib/node_modules/@openai/codex/bin/codex",
    },
    {
      harness: "opencode",
      installed: "2.2.7",
      latest: "2.3.0",
      update_available: true,
      method: { kind: "manual", command: "npm i -g opencode-ai@latest" },
      path: "/opt/homebrew/bin/opencode",
    },
    {
      harness: "antigravity",
      installed: "1.5.8",
      latest: null,
      update_available: false,
      method: { kind: "unavailable" },
      path: "/usr/local/bin/agy",
    },
  ];
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
        // A plausible multipath snapshot so the hosts menu and footer render
        // a real path, RTT and encryption state instead of a placeholder.
        const now = new Date().toISOString();
        const status: TransportStatus = {
          session_id: "mock-session",
          state: "online",
          active: {
            kind: "direct",
            endpoint: "192.168.1.24:47800",
            state: "active",
            rtt_ms: 12,
            encrypted: true,
          },
          candidates: [
            {
              kind: "direct",
              endpoint: "192.168.1.24:47800",
              state: "active",
              rtt_ms: 12,
              encrypted: true,
            },
          ],
          switched: false,
          detail: null,
          updated_at: now,
        };
        handlers.onTransportStatus?.(status);
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
