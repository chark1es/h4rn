// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ComposerMenu, type ComposerMenuRow } from "./ComposerMenu";

const rows: ComposerMenuRow[] = [{ kind: "file", id: "src/main.ts", path: "src/main.ts" }];

/** jsdom has no layout: give the popover and its parent the boxes a browser would. */
function stubLayout({
  width,
  height,
  parent,
}: {
  width: number;
  height: number;
  parent: { left: number; top: number; width: number; height: number };
}): void {
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains("q-comp-menu") ? width : 0;
  });
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains("q-comp-menu") ? height : 0;
  });
  // The menu places itself against `offsetParent`; without layout jsdom
  // reports none, so point it at the page and give the page the parent box.
  vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains("q-comp-menu") ? document.body : null;
  });
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this === document.body) {
      return {
        ...parent,
        right: parent.left + parent.width,
        bottom: parent.top + parent.height,
        x: parent.left,
        y: parent.top,
        toJSON: () => parent,
      } as DOMRect;
    }
    return {
      left: 0,
      top: 0,
      width: 0,
      height: 0,
      right: 0,
      bottom: 0,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    } as DOMRect;
  });
}

function renderMenu(anchor: { left: number; bottom: number }) {
  const view = render(
    <ComposerMenu
      kind="file"
      rows={rows}
      activeIndex={0}
      anchor={anchor}
      loading={false}
      error={null}
      workspaceName="repo"
      query=""
      isPhone={false}
      indexedCount={rows.length}
      truncated={false}
      listId="menu"
      onSelect={() => undefined}
      onHighlight={() => undefined}
      onClose={() => undefined}
    />,
  );
  const menu = view.container.querySelector<HTMLElement>(".q-comp-menu");
  if (!menu) throw new Error("The popover did not render.");
  return { view, menu };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ComposerMenu placement wiring", () => {
  it("applies the measured, right-clamped position", async () => {
    vi.spyOn(window, "innerWidth", "get").mockReturnValue(1_200);
    vi.spyOn(window, "innerHeight", "get").mockReturnValue(800);
    stubLayout({
      width: 420,
      height: 320,
      parent: { left: 0, top: 600, width: 500, height: 96 },
    });

    const { menu } = renderMenu({ left: 900, bottom: 80 });

    await waitFor(() => expect(menu.style.left).toBe("772px"));
    expect(menu.style.bottom).toBe("80px");
    expect(menu.style.top).toBe("");
    expect(menu.getAttribute("data-flipped")).toBeNull();
    expect(screen.getByRole("listbox", { name: "Files" })).toBeTruthy();
  });

  it("flips below the caret when the measured height does not fit above", async () => {
    vi.spyOn(window, "innerWidth", "get").mockReturnValue(1_200);
    vi.spyOn(window, "innerHeight", "get").mockReturnValue(800);
    stubLayout({
      width: 300,
      height: 320,
      parent: { left: 0, top: 0, width: 500, height: 96 },
    });

    const { menu } = renderMenu({ left: 100, bottom: 80 });

    await waitFor(() => expect(menu.style.top).toBe("47px"));
    expect(menu.style.bottom).toBe("");
    expect(menu.style.left).toBe("100px");
    expect(menu.getAttribute("data-flipped")).toBe("below");
  });
});
