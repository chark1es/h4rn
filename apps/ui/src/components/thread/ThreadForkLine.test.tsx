import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it } from "vitest";

import type { ThreadSummary } from "@/gen";
import { store } from "@/lib/store/appStore";
import { ThreadForkLine } from "./ThreadForkLine";

function summary(): ThreadSummary {
  return {
    thread_id: "thread_fork",
    parent_thread_id: null,
    host: "host_1",
    harness: "claude_code",
    title: "Fix the parser (fork)",
    workspace_path: "/work",
    model: "claude-sonnet",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
    tab: "done",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    forked_from: { thread_id: "thread_source", turn_id: "turn_1" },
    last_activity: "2026-01-01T00:00:00Z",
    agent_state: "done",
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
    needs_attention: false,
    pending_guest_requests: 0,
    unread: false,
    queued_count: 0,
    queue_revision: 0,
    steer: false,
    resume_at: null,
    limit: null,
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    rewind: "handoff",
    file_rewind: false,
  };
}

function origin(): ThreadSummary {
  const source = { ...summary(), thread_id: "thread_source", title: "Fix the parser" };
  delete source.forked_from;
  return source;
}

describe("ThreadForkLine", () => {
  beforeEach(() => {
    const state = store.getState();
    store.setState({ ...state, threads: {} });
  });

  it("names the source and links back to it", () => {
    const state = store.getState();
    store.setState({ ...state, threads: { thread_source: origin() } });
    const markup = renderToStaticMarkup(<ThreadForkLine summary={summary()} />);
    expect(markup).toContain("Forked from");
    expect(markup).toContain("Fix the parser");
    expect(markup).toContain("Open Fix the parser");
  });

  it("keeps the fact but drops the link when the source is gone", () => {
    const markup = renderToStaticMarkup(<ThreadForkLine summary={summary()} />);
    expect(markup).toContain("no longer in your list");
    expect(markup).not.toContain("<button");
  });

  it("renders nothing for an ordinary thread", () => {
    const ordinary = origin();
    expect(renderToStaticMarkup(<ThreadForkLine summary={ordinary} />)).toBe("");
  });
});
