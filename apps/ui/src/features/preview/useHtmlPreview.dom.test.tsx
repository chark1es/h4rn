// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { FsFileContent, ThreadSummary, Usage } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";
import { useHtmlPreview } from "./useHtmlPreview";

const state = vi.hoisted(() => ({
  requests: [] as unknown[],
  onEvent: null as ((envelope: EventEnvelope) => void) | null,
  read: null as null | ((path: string) => FsFileContent | Error),
  prepare: null as null | ((html: string) => Promise<{ url: string; token: string } | null>),
  release: null as null | ((url: string | null) => Promise<void>),
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({
    request: async (request: unknown) => {
      state.requests.push(request);
      const file = state.read?.(String((request as { path?: string }).path));
      if (file instanceof Error) throw file;
      if (!file) throw new Error("no reader");
      return { type: "fs_file", file };
    },
    onEvent: (listener: (envelope: EventEnvelope) => void) => {
      state.onEvent = listener;
      return () => {
        state.onEvent = null;
      };
    },
  }),
}));

vi.mock("@/lib/client/fsWatch", () => ({
  retainWorkspaceWatch: () => () => undefined,
}));

vi.mock("./previewFrame", () => ({
  preparePreviewFrame: (html: string) => state.prepare?.(html) ?? Promise.resolve(null),
  releasePreviewFrame: (url: string | null) => state.release?.(url) ?? Promise.resolve(),
}));

const usage: Usage = {
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  cost_micros: 0,
  duration_ms: 0,
  generation_ms: 0,
  tps: 0,
};

const thread: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Write the report",
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

function htmlFile(path: string, content: string): FsFileContent {
  return {
    path,
    content,
    etag: "etag",
    encoding: "utf-8",
    size_bytes: new TextEncoder().encode(content).length,
    binary: false,
    truncated: false,
    bytes_base64: null,
    bytes_omitted: null,
    content_omitted: null,
    modified_at: null,
    hint: null,
  };
}

function Harness({ path }: { path: string | null }) {
  const preview = useHtmlPreview(thread, path);
  return (
    <p data-testid="state">{preview.document ? "loaded" : preview.failure ? "failed" : "empty"}</p>
  );
}

let roots: ReturnType<typeof createRoot>[] = [];

function render(path: string | null) {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  act(() => root.render(<Harness path={path} />));
  roots.push(root);
  return { container, root };
}

/** Lets queued promise callbacks run inside act. */
async function flush(times = 4): Promise<void> {
  await act(async () => {
    for (let index = 0; index < times; index += 1) await Promise.resolve();
  });
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  state.requests.length = 0;
  state.onEvent = null;
  state.read = (path) => htmlFile(path, "<p>hi</p>");
  state.prepare = async () => ({ url: "h4rn-preview://localhost/abc", token: "t1" });
  state.release = async () => undefined;
});

afterEach(() => {
  act(() => {
    for (const root of roots) root.unmount();
  });
  roots = [];
  document.body.innerHTML = "";
});

describe("useHtmlPreview document lifetime", () => {
  it("releases a prepared URL when the load is cancelled while the shell answers", async () => {
    let resolvePrepare: ((frame: { url: string; token: string }) => void) | null = null;
    state.prepare = () =>
      new Promise((resolve) => {
        resolvePrepare = resolve;
      });
    const release = vi.fn(async () => undefined);
    state.release = release;

    const { root } = render("reports/page.html");
    await flush();
    expect(state.requests.length).toBeGreaterThan(0);
    expect(resolvePrepare).not.toBeNull();

    act(() => root.unmount());
    roots = [];

    act(() => resolvePrepare?.({ url: "h4rn-preview://localhost/abc", token: "t1" }));
    await flush();
    expect(release).toHaveBeenCalledWith("h4rn-preview://localhost/abc");
  });
});

describe("useHtmlPreview recovery", () => {
  it("reloads when a failed document is recreated on disk", async () => {
    vi.useFakeTimers();
    try {
      let missing = true;
      state.read = (path) => (missing ? new Error("not found") : htmlFile(path, "<p>back</p>"));
      const { container } = render("reports/page.html");
      await flush();
      expect(container.querySelector('[data-testid="state"]')?.textContent).toBe("failed");

      missing = false;
      act(() => {
        state.onEvent?.({
          seq: 1,
          event: { type: "fs_changed", workspace: "/work", path: "reports/page.html" },
          replayed: false,
        } as EventEnvelope);
      });
      await act(async () => {
        vi.advanceTimersByTime(250);
      });
      await flush();
      expect(container.querySelector('[data-testid="state"]')?.textContent).toBe("loaded");
    } finally {
      vi.useRealTimers();
    }
  });
});
