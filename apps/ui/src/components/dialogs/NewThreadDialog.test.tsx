// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostSummary } from "@/gen";

const mock = vi.hoisted(() => {
  const state: {
    connection: { status: string };
    hosts: Record<string, HostSummary>;
    localHostId: string | null;
    ui: {
      newThreadOpen: boolean;
      newThreadPreset: { host: string; workspace: string } | null;
    };
  } = {
    connection: { status: "online" },
    hosts: {},
    localHostId: null,
    ui: { newThreadOpen: true, newThreadPreset: null },
  };
  const harness: {
    agents: unknown[];
    probes: Record<string, unknown>;
    probeErrors: Record<string, string>;
    probing: Record<string, boolean>;
    rows: unknown[];
    loaded: boolean;
  } = {
    agents: [],
    probes: {},
    probeErrors: {},
    probing: {},
    rows: [],
    loaded: false,
  };
  const attachments = {
    attachments: [] as unknown[],
    tasks: [] as unknown[],
    uploading: false,
    failed: false,
    blockedReason: null,
    inputRef: { current: null },
    addFiles: () => undefined,
    addPastedText: () => false,
    openPicker: () => undefined,
    onInputChange: () => undefined,
    remove: () => undefined,
    retry: () => undefined,
    clear: () => undefined,
  };
  return {
    state,
    harness,
    attachments,
    createThread: vi.fn<(input: Record<string, unknown>) => Promise<boolean>>(),
    closeNewThread: vi.fn(),
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  createThread: mock.createThread,
  closeNewThread: mock.closeNewThread,
}));

vi.mock("@/features/harnesses/harnessStore", () => ({
  // The real store always carries a full state object; the dialog reads the
  // agents list and the harness rows from it.
  useHarnesses: (selector: (value: unknown) => unknown) => selector(mock.harness),
  useAcpAgents: () => undefined,
  useHarnessVersions: () => undefined,
  probeAcpAgent: () => Promise.resolve(null),
}));

vi.mock("@/features/proxy/proxyStore", () => ({
  useProxyCatalog: () => undefined,
  useProxy: () => undefined,
  proxyOverrideFor: () => null,
  proxyModelsFor: () => [],
  proxyDefaultModel: () => null,
}));

vi.mock("@/hooks/use-media-query", () => ({
  useIsPhone: () => false,
}));

vi.mock("@/features/attachments/use-attachments", () => ({
  useAttachmentDrafts: () => mock.attachments,
  useAttachShortcut: () => undefined,
}));

import {
  DEFAULT_NEW_THREAD_WORKSPACE,
  drafts,
  flushDrafts,
  readNewThreadDraft,
  writeNewThreadDraft,
} from "@/lib/drafts";
import { NewThreadDialog } from "./NewThreadDialog";

function host(hostId: string, name: string): HostSummary {
  return {
    host_id: hostId,
    name,
    platform: "linux",
    online: true,
    last_seen: "2026-01-01T00:00:00.000Z",
    harnesses: ["claude_code"],
    latency_ms: null,
    path: null,
  };
}

function textarea(): HTMLTextAreaElement {
  return document.getElementById("new-thread-task") as HTMLTextAreaElement;
}

function folderField(): HTMLInputElement {
  return document.getElementById("new-thread-workspace") as HTMLInputElement;
}

function commitFolder(value: string): void {
  fireEvent.change(folderField(), { target: { value } });
  fireEvent.blur(folderField());
}

describe("NewThreadDialog drafts", () => {
  beforeEach(() => {
    cleanup();
    flushDrafts();
    window.localStorage.clear();
    mock.state.connection.status = "online";
    mock.state.hosts = {};
    mock.state.localHostId = null;
    mock.state.ui.newThreadOpen = true;
    mock.state.ui.newThreadPreset = null;
    mock.harness.agents = [];
    mock.harness.probes = {};
    mock.harness.probeErrors = {};
    mock.harness.probing = {};
    mock.harness.rows = [];
    mock.harness.loaded = false;
    mock.createThread.mockReset();
    mock.closeNewThread.mockReset();
  });

  afterEach(() => {
    cleanup();
  });

  it("keeps the task box and Start disabled until a host is bound", async () => {
    mock.state.hosts = {};
    mock.state.localHostId = null;

    const view = render(<NewThreadDialog />);

    expect(textarea().disabled).toBe(true);
    expect(textarea().placeholder).toBe("Connecting…");
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(
      true,
    );

    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    view.rerender(<NewThreadDialog />);

    await waitFor(() => expect(textarea().disabled).toBe(false));
    expect(textarea().placeholder).not.toBe("Connecting…");
  });

  it("disables the task box again after closing and reopening before a host resolves", async () => {
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    const view = render(<NewThreadDialog />);
    await waitFor(() => expect(textarea().disabled).toBe(false));

    mock.state.ui.newThreadOpen = false;
    view.rerender(<NewThreadDialog />);

    // Reopen while no host is known yet: the old binding must not survive.
    mock.state.hosts = {};
    mock.state.localHostId = null;
    mock.state.ui.newThreadOpen = true;
    view.rerender(<NewThreadDialog />);

    await waitFor(() => expect(textarea().disabled).toBe(true));
    expect(textarea().placeholder).toBe("Connecting…");
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("adopts the default host and its saved draft when the host resolves", async () => {
    writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "saved prompt");
    flushDrafts();

    const view = render(<NewThreadDialog />);
    expect(textarea().value).toBe("");

    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    view.rerender(<NewThreadDialog />);

    await waitFor(() => expect(textarea().value).toBe("saved prompt"));
    expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE);
  });

  it("switches the prompt to a folder's draft only when the folder is committed", async () => {
    writeNewThreadDraft("host_a", "~/code/web", "web work");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    fireEvent.change(folderField(), { target: { value: "~/code/web" } });
    expect(textarea().value).toBe("");
    expect(readNewThreadDraft("host_a", "~/code/web")).toBe("web work");

    fireEvent.blur(folderField());
    await waitFor(() => expect(textarea().value).toBe("web work"));
    expect(folderField().value).toBe("~/code/web");
  });

  it("commits the folder on Enter", async () => {
    writeNewThreadDraft("host_a", "~/code/web", "web work");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    fireEvent.change(folderField(), { target: { value: "~/code/web" } });
    fireEvent.keyDown(folderField(), { key: "Enter" });

    await waitFor(() => expect(textarea().value).toBe("web work"));
  });

  it("switches host and draft together when the user picks another online host", async () => {
    writeNewThreadDraft("host_b", DEFAULT_NEW_THREAD_WORKSPACE, "for beta");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "alpha"), host_b: host("host_b", "beta") };
    mock.state.localHostId = null;

    render(<NewThreadDialog />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Host: alpha" })).toBeTruthy());

    fireEvent.pointerDown(screen.getByRole("button", { name: "Host: alpha" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    const option = await screen.findByRole("menuitem", { name: /beta/ });
    fireEvent.click(option);

    await waitFor(() => expect(textarea().value).toBe("for beta"));
    expect(screen.getByRole("button", { name: "Host: beta" })).toBeTruthy();
  });

  it("rebinds a chosen host that stops being eligible", async () => {
    writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "for alpha");
    writeNewThreadDraft("host_b", DEFAULT_NEW_THREAD_WORKSPACE, "for beta");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "alpha"), host_b: host("host_b", "beta") };
    mock.state.localHostId = null;

    const view = render(<NewThreadDialog />);
    await waitFor(() => expect(screen.getByRole("button", { name: "Host: alpha" })).toBeTruthy());

    fireEvent.pointerDown(screen.getByRole("button", { name: "Host: alpha" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /beta/ }));
    await waitFor(() => expect(textarea().value).toBe("for beta"));

    // The user's chosen host is gone; the dialog falls back to the default.
    mock.state.hosts = { host_a: host("host_a", "alpha") };
    view.rerender(<NewThreadDialog />);

    await waitFor(() => expect(textarea().value).toBe("for alpha"));
    expect(screen.getByRole("button", { name: "Host: alpha" })).toBeTruthy();
  });

  it("rebinds to the daemon's own host when it arrives before the user picks one", async () => {
    writeNewThreadDraft("host_local", DEFAULT_NEW_THREAD_WORKSPACE, "local draft");
    flushDrafts();
    mock.state.hosts = { host_remote: host("host_remote", "alpha") };
    mock.state.localHostId = null;

    const view = render(<NewThreadDialog />);
    await waitFor(() => expect(textarea().value).toBe(""));
    expect(screen.getByRole("button", { name: "Host: alpha" })).toBeTruthy();

    mock.state.hosts = {
      host_remote: host("host_remote", "alpha"),
      host_local: host("host_local", "zulu"),
    };
    mock.state.localHostId = "host_local";
    view.rerender(<NewThreadDialog />);

    await waitFor(() => expect(textarea().value).toBe("local draft"));
    expect(screen.getByRole("button", { name: "Host: zulu" })).toBeTruthy();
  });

  it("does nothing on Cmd+Enter before a host resolves", () => {
    mock.state.hosts = {};
    mock.state.localHostId = null;

    render(<NewThreadDialog />);
    fireEvent.change(textarea(), { target: { value: "typed before a host" } });
    fireEvent.keyDown(textarea(), { key: "Enter", metaKey: true });

    expect(mock.createThread).not.toHaveBeenCalled();
    expect(
      Object.keys(window.localStorage).filter((key) => key.startsWith("h4rn.newThread.workspace.")),
    ).toEqual([]);
  });

  it("falls back to the daemon's cached models when the probe answer carries an error", async () => {
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    mock.harness.agents = [
      {
        id: "demo",
        name: "Demo agent",
        command: "demo-agent",
        env: {},
        working_dir: { kind: "thread_workspace" },
        installed: true,
        models: [{ id: "cached-model", name: "Cached" }],
        modes: [],
        version: null,
        path: "/usr/bin/demo-agent",
      },
    ];
    // The probe completed, but the agent failed its handshake: its error is on
    // the answer, not a rejected request.
    mock.harness.probes = {
      demo: {
        id: "demo",
        installed: true,
        version: null,
        models: [],
        modes: [],
        error: "the agent exited before the handshake",
      },
    };

    render(<NewThreadDialog />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Claude Code" })).toBeTruthy(),
    );
    fireEvent.pointerDown(screen.getByRole("button", { name: "Harness: Claude Code" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /Demo agent/ }));

    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Demo agent" })).toBeTruthy(),
    );
    const options = Array.from(
      document.querySelectorAll<HTMLOptionElement>("#h4rn-models-acp option"),
    ).map((option) => option.value);
    expect(options).toEqual(["cached-model"]);
    expect(screen.getByText("the agent exited before the handshake")).toBeTruthy();
  });

  it("keeps the model as default after a probe unless the user picks one", async () => {
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    mock.harness.agents = [
      {
        id: "demo",
        name: "Demo agent",
        command: "demo-agent",
        env: {},
        working_dir: { kind: "thread_workspace" },
        installed: true,
        models: [],
        modes: [],
        version: null,
        path: "/usr/bin/demo-agent",
      },
    ];
    mock.harness.probes = {
      demo: {
        id: "demo",
        installed: true,
        version: null,
        models: [
          { id: "first-model", name: "First" },
          { id: "second-model", name: "Second" },
        ],
        modes: [],
        error: null,
      },
    };

    render(<NewThreadDialog />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Claude Code" })).toBeTruthy(),
    );
    fireEvent.pointerDown(screen.getByRole("button", { name: "Harness: Claude Code" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /Demo agent/ }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Demo agent" })).toBeTruthy(),
    );

    const field = document.getElementById("new-thread-model") as HTMLInputElement;
    expect(field.value).toBe("default");
  });

  it("offers only the approval modes the agent actually advertises", async () => {
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    mock.harness.agents = [
      {
        id: "demo",
        name: "Demo agent",
        command: "demo-agent",
        env: {},
        working_dir: { kind: "thread_workspace" },
        installed: true,
        models: [],
        modes: [],
        version: null,
        path: "/usr/bin/demo-agent",
      },
    ];
    // An agent with its own mode ids: h4rn can select none of them.
    mock.harness.probes = {
      demo: {
        id: "demo",
        installed: true,
        version: null,
        models: [{ id: "demo-model", name: "Demo" }],
        modes: [
          { id: "ask", name: "Ask", description: null },
          { id: "code", name: "Code", description: null },
        ],
        error: null,
      },
    };

    render(<NewThreadDialog />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Claude Code" })).toBeTruthy(),
    );
    fireEvent.pointerDown(screen.getByRole("button", { name: "Harness: Claude Code" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /Demo agent/ }));

    await waitFor(() => expect(screen.getByText("Mode is managed by the agent.")).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Mode: Approval required" })).toBeNull();
    expect(screen.queryByText("Fixed for the life of the thread.")).toBeNull();
  });

  it("shows a fixed working directory and starts the thread there", async () => {
    mock.createThread.mockResolvedValue(true);
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";
    mock.harness.agents = [
      {
        id: "demo",
        name: "Demo agent",
        command: "demo-agent",
        env: {},
        working_dir: { kind: "fixed_path", path: "/srv/agent-home" },
        installed: true,
        models: [],
        modes: [{ id: "default", name: "Default", description: null }],
        version: null,
        path: "/usr/bin/demo-agent",
      },
    ];
    mock.harness.probes = {
      demo: {
        id: "demo",
        installed: true,
        version: null,
        models: [{ id: "demo-model", name: "Demo" }],
        modes: [{ id: "default", name: "Default", description: null }],
        error: null,
      },
    };

    render(<NewThreadDialog />);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Harness: Claude Code" })).toBeTruthy(),
    );
    fireEvent.pointerDown(screen.getByRole("button", { name: "Harness: Claude Code" }), {
      button: 0,
      ctrlKey: false,
      pointerType: "mouse",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: /Demo agent/ }));
    await waitFor(() => expect(folderField().value).toBe("/srv/agent-home"));
    expect(folderField().disabled).toBe(true);

    fireEvent.change(textarea(), { target: { value: "build it" } });
    fireEvent.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      harness: "acp",
      agentId: "demo",
      workspacePath: "/srv/agent-home",
      prompt: "build it",
    });
  });

  it("does not keep the Start-press flag after the pointer is cancelled", async () => {
    writeNewThreadDraft("host_a", "~/code/web", "web work");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    const start = screen.getByRole("button", { name: "Start" });
    fireEvent.pointerDown(start, { button: 0, pointerType: "mouse" });
    fireEvent.pointerCancel(start, { button: 0, pointerType: "mouse" });

    // The flag is gone, so leaving the folder field commits it and shows its draft.
    commitFolder("~/code/web");
    await waitFor(() => expect(textarea().value).toBe("web work"));
  });

  it("does not keep the Start-press flag after the dialog closes", async () => {
    writeNewThreadDraft("host_a", "~/code/web", "web work");
    flushDrafts();
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    const view = render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    fireEvent.pointerDown(screen.getByRole("button", { name: "Start" }), {
      button: 0,
      pointerType: "mouse",
    });
    mock.state.ui.newThreadOpen = false;
    view.rerender(<NewThreadDialog />);
    mock.state.ui.newThreadOpen = true;
    view.rerender(<NewThreadDialog />);
    await waitFor(() => expect(textarea().disabled).toBe(false));

    commitFolder("~/code/web");
    await waitFor(() => expect(textarea().value).toBe("web work"));
  });

  it("starts on the visible host and folder and clears the submitted draft", async () => {
    mock.createThread.mockResolvedValue(true);
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    fireEvent.change(textarea(), { target: { value: "build it" } });
    fireEvent.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      host: "host_a",
      workspacePath: DEFAULT_NEW_THREAD_WORKSPACE,
      prompt: "build it",
    });
    await waitFor(() => expect(mock.closeNewThread).toHaveBeenCalled());
    expect(readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
  });

  it("starts with the folder on screen without swapping the visible text", async () => {
    writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "default work");
    flushDrafts();
    mock.createThread.mockResolvedValue(true);
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(textarea().value).toBe("default work"));

    // The folder is typed but not committed: Start still sends the text that
    // is on screen, to the folder on screen.
    fireEvent.change(folderField(), { target: { value: "~/code/api" } });
    expect(textarea().value).toBe("default work");
    fireEvent.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      host: "host_a",
      workspacePath: "~/code/api",
      prompt: "default work",
    });
    await waitFor(() => expect(mock.closeNewThread).toHaveBeenCalled());
    // Only the slot the text came from is cleared.
    expect(readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
    expect(readNewThreadDraft("host_a", "~/code/api")).toBe("");
    // The folder that was used is the one the next open starts in.
    expect(drafts.readLastWorkspace("host_a")).toBe("~/code/api");
  });

  it("starts with the folder on screen even when pressing Start blurs the field", async () => {
    writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "default work");
    flushDrafts();
    mock.createThread.mockResolvedValue(true);
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(textarea().value).toBe("default work"));

    fireEvent.change(folderField(), { target: { value: "~/code/api" } });
    const start = screen.getByRole("button", { name: "Start" });
    // A real press focuses the button, blurring the folder field first.
    fireEvent.pointerDown(start, { button: 0, pointerType: "mouse" });
    fireEvent.blur(folderField());
    fireEvent.pointerUp(start, { button: 0, pointerType: "mouse" });
    expect(textarea().value).toBe("default work");
    fireEvent.click(start);

    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));
    expect(mock.createThread.mock.calls[0]?.[0]).toMatchObject({
      workspacePath: "~/code/api",
      prompt: "default work",
    });
    expect(readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
  });

  it("clears only the submitted folder when the folder changes while Start waits", async () => {
    writeNewThreadDraft("host_a", "~/code/api", "api work");
    writeNewThreadDraft("host_a", "~/code/web", "web work");
    flushDrafts();
    let finish: (ok: boolean) => void = () => undefined;
    mock.createThread.mockReturnValue(
      new Promise<boolean>((resolve) => {
        finish = resolve;
      }),
    );
    mock.state.hosts = { host_a: host("host_a", "atlas") };
    mock.state.localHostId = "host_a";

    render(<NewThreadDialog />);
    await waitFor(() => expect(folderField().value).toBe(DEFAULT_NEW_THREAD_WORKSPACE));

    commitFolder("~/code/api");
    await waitFor(() => expect(textarea().value).toBe("api work"));
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    await waitFor(() => expect(mock.createThread).toHaveBeenCalledTimes(1));

    commitFolder("~/code/web");
    await waitFor(() => expect(textarea().value).toBe("web work"));

    await act(async () => {
      finish(true);
    });

    expect(readNewThreadDraft("host_a", "~/code/api")).toBe("");
    expect(readNewThreadDraft("host_a", "~/code/web")).toBe("web work");
  });
});
