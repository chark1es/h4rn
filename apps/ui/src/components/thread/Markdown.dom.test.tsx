// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { closePreview, getState, navigate } from "@/lib/store/appStore";
import { Markdown } from "./Markdown";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let roots: Root[] = [];

function render(text: string, interceptHtmlLinks: boolean): HTMLElement {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  act(() => root.render(<Markdown text={text} interceptHtmlLinks={interceptHtmlLinks} />));
  roots.push(root);
  return container;
}

function click(container: HTMLElement, init: MouseEventInit = {}): MouseEvent {
  const anchor = container.querySelector("a");
  if (!anchor) throw new Error("no link");
  const event = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
  act(() => {
    anchor.dispatchEvent(event);
  });
  return event;
}

beforeEach(() => {
  closePreview();
  navigate({ kind: "thread", threadId: "thread_1" });
});

afterEach(() => {
  act(() => {
    for (const root of roots) root.unmount();
  });
  roots = [];
  document.body.innerHTML = "";
  closePreview();
});

describe("markdown HTML links", () => {
  it("opens a relative HTML link in the preview from a thread message", () => {
    const container = render("[report](reports/report.html)", true);
    const event = click(container);
    expect(event.defaultPrevented).toBe(true);
    expect(getState().ui.preview).toEqual({
      threadId: "thread_1",
      path: "reports/report.html",
    });
  });

  it("leaves the link alone outside thread messages", () => {
    const container = render("[report](reports/report.html)", false);
    const event = click(container);
    expect(event.defaultPrevented).toBe(false);
    expect(getState().ui.preview).toBeNull();
  });

  it("never intercepts a modifier-click", () => {
    const container = render("[report](reports/report.html)", true);
    const event = click(container, { metaKey: true });
    expect(event.defaultPrevented).toBe(false);
    expect(getState().ui.preview).toBeNull();
  });

  it("leaves non-HTML links to the browser", () => {
    const container = render("[png](reports/chart.png)", true);
    const event = click(container);
    expect(event.defaultPrevented).toBe(false);
    expect(getState().ui.preview).toBeNull();
  });
});
