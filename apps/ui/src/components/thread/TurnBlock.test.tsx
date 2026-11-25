import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

import type { ThreadSummary, Usage } from "@/gen";
import type { StreamTurn } from "@/lib/store/projection";
import { TurnBlock } from "./TurnBlock";

const usage: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  cost_micros: 0,
  duration_ms: 0,
  generation_ms: 0,
  tps: 0,
};

const summary: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Stopped",
  workspace_path: "/work",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "working",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "done",
  usage,
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  needs_attention: false,
  pending_guest_requests: 0,
  unread: false,
  resume_at: null,
  rewind: "handoff",
  file_rewind: false,
  limit: null,
  queued_count: 0,
  queue_revision: 0,
  steer: true,
};

/** A turn whose only item is the user's prompt, as before the agent spoke. */
function userOnlyTurn(status: StreamTurn["status"]): StreamTurn {
  return {
    id: "turn_1",
    startedAt: "2026-01-01T00:00:00Z",
    status,
    items: [
      {
        kind: "message",
        itemId: "item_1",
        role: "user",
        text: "ship it",
        attachments: [],
        streaming: false,
      },
    ],
    usage: null,
    changes: null,
  };
}

function render(turn: StreamTurn): string {
  return renderToStaticMarkup(
    <TurnBlock turn={turn} threadId="thread_1" summary={summary} onReply={() => {}} />,
  );
}

describe("TurnBlock", () => {
  it("shows the stopped notice when a token limit ends the turn before any output", () => {
    const markup = render(userOnlyTurn("token_limit"));
    expect(markup).toContain("Stopped: the agent hit its token limit.");
    expect(markup).not.toContain("Nothing after this ran.");
  });

  it("offers Reply, not a resend, for a stopped turn", () => {
    const markup = render(userOnlyTurn("token_limit"));
    expect(markup).toContain("Reply");
    expect(markup).not.toContain("Retry");
  });

  it("shows the stopped notice when a turn limit ends the turn before any output", () => {
    expect(render(userOnlyTurn("turn_limit"))).toContain(
      "Stopped: the agent hit its turn-request limit.",
    );
  });

  it("leaves a running user turn without a stopped notice", () => {
    expect(render(userOnlyTurn("running"))).not.toContain("Stopped:");
  });

  it("shows plan steps and their status quietly", () => {
    const markup = render({
      ...userOnlyTurn("running"),
      items: [
        {
          kind: "plan",
          itemId: "plan:turn_1",
          entries: [
            { content: "Inspect the workspace", status: "completed", priority: "high" },
            { content: "Implement the fix", status: "in_progress", priority: "medium" },
            { content: "Run the tests", status: "pending", priority: "low" },
          ],
        },
      ],
    });
    expect(markup).toContain("Plan");
    expect(markup).toContain("Inspect the workspace");
    expect(markup).toContain("Implement the fix");
    expect(markup).toContain("Run the tests");
    expect(markup).toContain('data-status="completed"');
    expect(markup).toContain('data-status="in_progress"');
    expect(markup).toContain('data-status="pending"');
    expect(markup).toContain("(done)");
    expect(markup).toContain("(in progress)");
  });

  it("marks a step in progress with a neutral mark, not the accent", () => {
    const css = readFileSync(new URL("../../styles/quiet.css", import.meta.url), "utf8");
    const start = css.indexOf('.q-plan .q-plan-step[data-status="in_progress"] .q-plan-mark');
    expect(start).toBeGreaterThan(-1);
    const rule = css.slice(start, css.indexOf("}", start));
    // Blue means "you are needed"; the agent working through a plan is not a
    // request for attention.
    expect(rule).not.toMatch(/--accent/);
    expect(rule).toMatch(/--fg/);
  });

  it("offers Reply, not a resend, after a failed or interrupted turn", () => {
    for (const status of ["failed", "interrupted"] as const) {
      const markup = render({
        ...userOnlyTurn(status),
        items: [
          ...userOnlyTurn(status).items,
          {
            kind: "message",
            itemId: "item_2",
            role: "assistant",
            text: "partial output",
            attachments: [],
            streaming: false,
          },
        ],
      });
      expect(markup).toContain("Reply");
      expect(markup).not.toContain("Retry");
    }
  });

  it("never calls an imported reply live, even while the agent state is active", () => {
    const turn: StreamTurn = {
      ...userOnlyTurn("running"),
      imported: true,
      items: [
        {
          kind: "message",
          itemId: "item_2",
          role: "assistant",
          text: "the reply imported from Claude Code",
          attachments: [],
          streaming: false,
        },
      ],
    };
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={turn}
        threadId="thread_1"
        summary={{ ...summary, tab: "working", agent_state: "thinking" }}
        onReply={() => {}}
        staleAt={Date.now()}
      />,
    );
    expect(markup).not.toContain("· live");
    expect(markup).not.toContain("Stream paused");
  });

  it("does not call a running turn live when the agent state is not active", () => {
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={{
          ...userOnlyTurn("running"),
          items: [
            ...userOnlyTurn("running").items,
            {
              kind: "message",
              itemId: "item_2",
              role: "assistant",
              text: "a reply",
              attachments: [],
              streaming: false,
            },
          ],
        }}
        threadId="thread_1"
        summary={{ ...summary, tab: "working", agent_state: "idle" }}
        onReply={() => {}}
      />,
    );
    expect(markup).not.toContain("· live");
  });

  it("keeps the live line for a turn the agent is working on", () => {
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={{
          ...userOnlyTurn("running"),
          items: [
            ...userOnlyTurn("running").items,
            {
              kind: "message",
              itemId: "item_2",
              role: "assistant",
              text: "working on it",
              attachments: [],
              streaming: true,
            },
          ],
        }}
        threadId="thread_1"
        summary={{ ...summary, tab: "working", agent_state: "thinking" }}
        onReply={() => {}}
      />,
    );
    expect(markup).toContain("· live");
  });
});

describe("Edit from here", () => {
  it("sits under a sent message and names the message it would rewind", () => {
    const onEdit = vi.fn();
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={userOnlyTurn("completed")}
        threadId="thread_1"
        summary={summary}
        onReply={() => {}}
        onEditFromHere={onEdit}
      />,
    );
    expect(markup).toContain("Edit from here");
    expect(markup).not.toContain("disabled");
  });

  it("is disabled with its reason while the thread cannot rewind", () => {
    const markup = renderToStaticMarkup(
      <TurnBlock
        turn={userOnlyTurn("completed")}
        threadId="thread_1"
        summary={summary}
        onReply={() => {}}
        onEditFromHere={() => {}}
        editDisabledReason="Wait for the current turn to finish, or stop it first."
      />,
    );
    expect(markup).toContain("Wait for the current turn to finish");
    expect(markup).toContain("disabled");
  });
});
