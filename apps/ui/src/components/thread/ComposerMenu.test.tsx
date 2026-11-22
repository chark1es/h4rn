// @vitest-environment jsdom

import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
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

function renderMenu(
  anchor: { left: number; bottom: number },
  overrides: {
    isPhone?: boolean;
    indexedCount?: number;
    truncated?: boolean;
    rows?: ComposerMenuRow[];
    query?: string;
  } = {},
) {
  const view = render(
    <ComposerMenu
      kind="file"
      rows={overrides.rows ?? rows}
      activeIndex={0}
      anchor={anchor}
      loading={false}
      error={null}
      workspaceName="repo"
      query={overrides.query ?? ""}
      isPhone={overrides.isPhone ?? false}
      indexedCount={overrides.indexedCount ?? rows.length}
      truncated={overrides.truncated ?? false}
      listId="menu"
      onSelect={() => undefined}
      onHighlight={() => undefined}
      onClose={() => undefined}
    />,
  );
  const menu = view.container.querySelector<HTMLElement>(".q-comp-menu, .q-comp-sheet");
  if (!menu) throw new Error("The popover did not render.");
  return { view, menu };
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
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

describe("ComposerMenu partial index note", () => {
  it("keeps the note outside the scrolling list so it stays visible", () => {
    const { menu } = renderMenu(
      { left: 0, bottom: 80 },
      { indexedCount: 5, truncated: true, query: "nothing-matches", rows: [] },
    );

    const list = within(menu).getByRole("listbox");
    const note = within(menu).getByText("Showing the first 5 files.");
    expect(list.contains(note)).toBe(false);
    expect(menu.contains(note)).toBe(true);
    expect(within(menu).getByText("No files match “nothing-matches”.")).toBeTruthy();
  });
});

describe("ComposerMenu phone sheet height", () => {
  it("fits the sheet to the visible space above the composer", async () => {
    vi.spyOn(window, "innerWidth", "get").mockReturnValue(390);
    vi.spyOn(window, "innerHeight", "get").mockReturnValue(600);
    const visual = {
      height: 500,
      offsetTop: 20,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    };
    vi.stubGlobal("visualViewport", visual);
    vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.classList.contains("q-ph-sheet") ? document.body : null;
    });
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (
      this: Element,
    ) {
      if (this === document.body) {
        return {
          left: 0,
          top: 140,
          width: 390,
          height: 400,
          right: 390,
          bottom: 540,
          x: 0,
          y: 140,
          toJSON: () => ({}),
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

    const { menu } = renderMenu(
      { left: 0, bottom: 0 },
      { isPhone: true, indexedCount: 5, truncated: true },
    );

    // 140 px of composer top - 20 px visible-viewport offset - 8 px gap.
    await waitFor(() => expect(menu.style.maxHeight).toBe("112px"));
    const done = within(menu).getByRole("button", { name: "Done" });
    expect(menu.contains(done)).toBe(true);
    const list = within(menu).getByRole("listbox");
    expect(list.contains(within(menu).getByText("Showing the first 5 files."))).toBe(false);
  });
});
