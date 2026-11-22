// @vitest-environment jsdom
import { act, type ReactElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { WorkspaceFileRow } from "@/components/panel/WorkspaceFiles";
import type { FileEntry } from "@/gen";
import { closePreview, getState, openPreview, useApp } from "@/lib/store/appStore";
import { PreviewFrame, previewReadyTimeoutMs } from "./PreviewPanel";

const bridgeTimeout = previewReadyTimeoutMs(new TextEncoder().encode("<p>hi</p>").length);

const { openExternalUrl } = vi.hoisted(() => ({
  openExternalUrl: vi.fn(async (_url: string) => undefined),
}));
vi.mock("@/lib/platform/openExternal", () => ({ openExternalUrl }));

const entry: FileEntry = {
  name: "report.html",
  path: "reports/report.html",
  kind: "file",
  size_bytes: 1200,
  modified_at: null,
  git_status: null,
};

function render(element: ReactElement): { root: Root; container: HTMLElement } {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  act(() => root.render(element));
  return { root, container };
}

let roots: Root[] = [];

beforeEach(() => {
  // React 19 reads this to allow act() outside a test renderer.
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  openExternalUrl.mockClear();
  closePreview();
});

afterEach(() => {
  act(() => {
    for (const root of roots) root.unmount();
  });
  roots = [];
  document.body.innerHTML = "";
});

/** The real Files row wired to the real store action. */
function Harness() {
  const tab = useApp((state) => state.ui.layout.panelTab);
  const preview = useApp((state) => state.ui.preview);
  return (
    <>
      <WorkspaceFileRow
        entry={entry}
        depth={1}
        open={false}
        active={false}
        onToggle={() => undefined}
        onOpenFile={() => undefined}
        onOpenPreview={(path) => openPreview(path, "thread_1")}
      />
      <p data-testid="tab">{tab}</p>
      <p data-testid="file">{preview?.path ?? ""}</p>
    </>
  );
}

describe("Files row preview action", () => {
  it("opens the file in the Preview tab when the row action is clicked", () => {
    const { root, container } = render(<Harness />);
    roots.push(root);

    const action = container.querySelector<HTMLButtonElement>(
      'button[aria-label="Open preview of report.html"]',
    );
    expect(action).not.toBeNull();
    act(() => action?.click());

    expect(container.querySelector('[data-testid="tab"]')?.textContent).toBe("preview");
    expect(container.querySelector('[data-testid="file"]')?.textContent).toBe(
      "reports/report.html",
    );
    expect(getState().ui.layout.panelTab).toBe("preview");
  });
});

describe("preview link bridge", () => {
  function frame(container: HTMLElement): HTMLIFrameElement {
    const element = container.querySelector("iframe");
    if (!element) throw new Error("no preview frame");
    return element;
  }

  function message(source: Window | null, token: string, href: string) {
    act(() => {
      window.dispatchEvent(
        new MessageEvent("message", {
          data: { type: "h4rn:preview-open", token, href },
          source,
        }),
      );
    });
  }

  function ready(source: Window | null, token: string) {
    act(() => {
      window.dispatchEvent(
        new MessageEvent("message", {
          data: { type: "h4rn:preview-ready", token },
          source,
        }),
      );
    });
  }

  it("asks before opening a link and opens only on the click", () => {
    const { root, container } = render(
      <PreviewFrame srcdoc="<p>hi</p>" url={null} token="t1" title="Preview" />,
    );
    roots.push(root);

    message(frame(container).contentWindow, "t1", "https://example.com/page");
    expect(openExternalUrl).not.toHaveBeenCalled();

    const bar = container.querySelector(".q-pv-link");
    expect(bar?.textContent).toContain("example.com");
    const open = [...container.querySelectorAll("button")].find(
      (button) => button.textContent === "Open",
    );
    act(() => open?.click());
    expect(openExternalUrl).toHaveBeenCalledWith("https://example.com/page");
    expect(container.querySelector(".q-pv-link")).toBeNull();
  });

  it("ignores a forged token, a foreign source and non-web schemes", () => {
    const { root, container } = render(
      <PreviewFrame srcdoc="<p>hi</p>" url={null} token="t1" title="Preview" />,
    );
    roots.push(root);
    const source = frame(container).contentWindow;

    message(source, "forged", "https://example.com/page");
    message(window, "t1", "https://example.com/page");
    message(source, "t1", "javascript:alert(1)");
    message(source, "t1", "file:///etc/passwd");
    expect(openExternalUrl).not.toHaveBeenCalled();
    expect(container.querySelector(".q-pv-link")).toBeNull();
  });

  it("freezes the confirmation URL until the user answers", () => {
    const { root, container } = render(
      <PreviewFrame srcdoc="<p>hi</p>" url={null} token="t1" title="Preview" />,
    );
    roots.push(root);
    const source = frame(container).contentWindow;

    message(source, "t1", "https://first.example/page");
    expect(container.querySelector(".q-pv-link")?.textContent).toContain("first.example");

    // The page controls the whole frame, so a later message cannot retarget
    // a confirmation the user is already looking at.
    message(source, "t1", "https://second.example/other");
    expect(container.querySelector(".q-pv-link")?.textContent).toContain("first.example");

    const open = [...container.querySelectorAll("button")].find(
      (button) => button.textContent === "Open",
    );
    act(() => open?.click());
    expect(openExternalUrl).toHaveBeenCalledTimes(1);
    expect(openExternalUrl).toHaveBeenCalledWith("https://first.example/page");
  });

  it("asks again after the bar is dismissed", () => {
    const { root, container } = render(
      <PreviewFrame srcdoc="<p>hi</p>" url={null} token="t1" title="Preview" />,
    );
    roots.push(root);
    const source = frame(container).contentWindow;

    message(source, "t1", "https://first.example/page");
    const cancel = [...container.querySelectorAll("button")].find(
      (button) => button.textContent === "Cancel",
    );
    act(() => cancel?.click());
    expect(container.querySelector(".q-pv-link")).toBeNull();

    message(source, "t1", "https://second.example/other");
    expect(container.querySelector(".q-pv-link")?.textContent).toContain("second.example");
  });

  it("shows the Android notice from the shell's capability report, without waiting", () => {
    const { root, container } = render(
      <PreviewFrame
        srcdoc="<p>hi</p>"
        url={null}
        token="t1"
        title="Preview"
        capabilities={{ platform: "android", documentStartScripts: false }}
      />,
    );
    roots.push(root);
    expect(container.querySelector("iframe")).toBeNull();
    expect(container.textContent).toContain("Update Android System WebView");
  });

  it("shows the generic note on an Android WebView that supports document-start scripts", () => {
    vi.useFakeTimers();
    try {
      const { root, container } = render(
        <PreviewFrame
          srcdoc="<p>hi</p>"
          url={null}
          token="t1"
          title="Preview"
          capabilities={{ platform: "android", documentStartScripts: true }}
        />,
      );
      roots.push(root);
      act(() => frame(container).dispatchEvent(new Event("load")));
      act(() => {
        vi.advanceTimersByTime(bridgeTimeout);
      });
      expect(container.querySelector("iframe")).toBeNull();
      expect(container.textContent).toContain("Preview scripts didn’t start");
      expect(container.textContent).not.toContain("Android System WebView");
    } finally {
      vi.useRealTimers();
    }
  });

  it("offers Reload for a desktop frame that never reports its bridge running", () => {
    vi.useFakeTimers();
    try {
      const reload = vi.fn();
      const { root, container } = render(
        <PreviewFrame
          srcdoc="<p>hi</p>"
          url={null}
          token="t1"
          title="Preview"
          capabilities={{ platform: "desktop", documentStartScripts: true }}
          onReload={reload}
        />,
      );
      roots.push(root);
      act(() => frame(container).dispatchEvent(new Event("load")));
      act(() => {
        vi.advanceTimersByTime(bridgeTimeout);
      });
      expect(container.textContent).toContain("Preview scripts didn’t start");
      expect(container.textContent).not.toContain("Android System WebView");
      const button = [...container.querySelectorAll("button")].find(
        (entry) => entry.textContent === "Reload",
      );
      act(() => button?.click());
      expect(reload).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps the frame when the bridge reports that scripts run", () => {
    vi.useFakeTimers();
    try {
      const { root, container } = render(
        <PreviewFrame srcdoc="<p>hi</p>" url={null} token="t1" title="Preview" />,
      );
      roots.push(root);
      const iframe = frame(container);
      ready(iframe.contentWindow, "t1");
      act(() => iframe.dispatchEvent(new Event("load")));
      act(() => {
        vi.advanceTimersByTime(bridgeTimeout);
      });
      expect(container.querySelector("iframe")).not.toBeNull();
      expect(container.textContent).not.toContain("Preview scripts didn’t start");
    } finally {
      vi.useRealTimers();
    }
  });

  it("scales the ready deadline with the document size", () => {
    expect(previewReadyTimeoutMs(0)).toBe(1_000);
    expect(previewReadyTimeoutMs(2 * 1024 * 1024)).toBeGreaterThan(
      previewReadyTimeoutMs(64 * 1024),
    );
    expect(previewReadyTimeoutMs(64 * 1024 * 1024)).toBe(20_000);
  });

  it("renders the shell URL as src and keeps srcdoc when there is none", () => {
    const first = render(
      <PreviewFrame
        srcdoc="<p>hi</p>"
        url="h4rn-preview://localhost/abc"
        token="t1"
        title="Preview"
      />,
    );
    roots.push(first.root);
    const framed = frame(first.container);
    expect(framed.getAttribute("src")).toBe("h4rn-preview://localhost/abc");
    expect(framed.getAttribute("srcdoc")).toBeNull();
    expect(framed.getAttribute("sandbox")).toBe("allow-scripts");
    expect(framed.getAttribute("sandbox")).not.toContain("allow-same-origin");
  });
});
