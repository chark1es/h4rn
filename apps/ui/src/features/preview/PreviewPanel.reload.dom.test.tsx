// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary, Usage } from "@/gen";
import { PreviewPanel } from "./PreviewPanel";

type Capabilities = { platform: string; documentStartScripts: unknown };

const state = vi.hoisted(() => ({
  capabilities: {
    platform: "android",
    documentStartScripts: false,
  } as Capabilities,
  capabilityCalls: 0,
  reads: 0,
}));

vi.mock("@/lib/platform/openExternal", () => ({
  openExternalUrl: async () => undefined,
}));

// The panel and the preview hook both reach the transport through the store;
// this test only needs one file read and no watch traffic.
vi.mock("@/lib/store/appStore", () => {
  const file = (path: string) => ({
    path,
    content: "<p>hi</p>",
    etag: "etag",
    encoding: "utf-8",
    size_bytes: 9,
    binary: false,
    truncated: false,
    bytes_base64: null,
    bytes_omitted: null,
    content_omitted: null,
    modified_at: null,
    hint: null,
  });
  const client = {
    request: async (request: { type?: string; path?: string }) => {
      if (request.type === "fs_read") {
        state.reads += 1;
        return { type: "fs_file", file: file(String(request.path)) };
      }
      return { type: "ok" };
    },
    onReady: () => () => undefined,
    onEvent: () => () => undefined,
  };
  return {
    getClient: () => client,
    onAppEvent: () => () => undefined,
    onClientChange: () => () => undefined,
    closePreview: vi.fn(),
    collapsePreview: vi.fn(),
    expandPreview: vi.fn(),
    setPanelTab: vi.fn(),
  };
});

vi.mock("@/lib/client/fsWatch", () => ({
  retainWorkspaceWatch: () => () => undefined,
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
  unread: false,
  resume_at: null,
  limit: null,
};

let roots: Root[] = [];

function render(): { container: HTMLElement } {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  act(() => root.render(<PreviewPanel thread={thread} path="reports/page.html" />));
  roots.push(root);
  return { container };
}

/** Lets queued promise callbacks run inside act. */
async function flush(times = 8): Promise<void> {
  await act(async () => {
    for (let index = 0; index < times; index += 1) await Promise.resolve();
  });
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  state.capabilities = { platform: "android", documentStartScripts: false };
  state.capabilityCalls = 0;
  state.reads = 0;
  (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {
    invoke: async (command: string) => {
      if (command === "preview_capabilities") {
        state.capabilityCalls += 1;
        return state.capabilities;
      }
      // The shell's preview origin is not exercised here; `null` keeps the
      // document in `srcdoc`.
      return null;
    },
  };
});

afterEach(() => {
  act(() => {
    for (const root of roots) root.unmount();
  });
  roots = [];
  document.body.innerHTML = "";
  delete (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
});

describe("preview capability reload", () => {
  it("clears the Android notice when Reload finds a WebView that can run scripts", async () => {
    const { container } = render();
    await flush();
    expect(container.textContent).toContain("Update Android System WebView");
    expect(container.querySelector("iframe")).toBeNull();
    expect(state.capabilityCalls).toBe(1);

    // The person updated Android System WebView and came back to the panel.
    state.capabilities = { platform: "android", documentStartScripts: true };
    const reload = [...container.querySelectorAll("button")].find(
      (entry) => entry.textContent === "Reload",
    );
    expect(reload).not.toBeUndefined();
    act(() => reload?.click());
    await flush();

    expect(state.capabilityCalls).toBe(2);
    expect(container.textContent).not.toContain("Android System WebView");
    expect(container.querySelector("iframe")).not.toBeNull();
  });

  it("keeps the notice when Reload still reports the same WebView", async () => {
    const { container } = render();
    await flush();

    const reload = [...container.querySelectorAll("button")].find(
      (entry) => entry.textContent === "Reload",
    );
    act(() => reload?.click());
    await flush();

    expect(state.capabilityCalls).toBe(2);
    expect(container.textContent).toContain("Update Android System WebView");
    expect(container.querySelector("iframe")).toBeNull();
  });
});
