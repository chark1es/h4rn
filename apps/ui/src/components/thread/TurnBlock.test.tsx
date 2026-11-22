import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

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
  unread: false,
  resume_at: null,
  limit: null,
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
});
