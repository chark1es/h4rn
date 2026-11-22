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

/** jsdom has no layout: build the box a browser would report. */
function box(left: number, top: number, width: number, height: number): DOMRect {
  return {
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    x: left,
    y: top,
    toJSON: () => ({}),
  } as DOMRect;
}

/**
 * jsdom has no layout, so the sheet's inline `bottom`/`max-height` are turned
 * back into the boxes a browser would give it and its header.
 */
function stubPhoneLayout({
  viewportTop,
  viewportHeight,
  composer,
}: {
  viewportTop: number;
  viewportHeight: number;
  composer: { top: number; height: number };
}): void {
  vi.stubGlobal("visualViewport", {
    height: viewportHeight,
    offsetTop: viewportTop,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains("q-ph-sheet") ? document.body : null;
  });
  const composerBox = box(0, composer.top, 390, composer.height);
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this === document.body) return composerBox;
    const element = this as HTMLElement;
    const sheet = element.classList.contains("q-ph-sheet")
      ? element
      : element.closest<HTMLElement>(".q-ph-sheet");
    if (!sheet) return box(0, 0, 0, 0);
    const bottom = composerBox.bottom - (Number.parseFloat(sheet.style.bottom) || 0);
    const height = Number.parseFloat(sheet.style.maxHeight) || 0;
    const top = bottom - height;
    if (element === sheet) return box(0, top, 390, height);
    // Done sits in the sheet's fixed header, 8 px below its top edge.
    return box(16, top + 8, 60, 22);
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
  it("keeps the sheet and Done on screen when the composer sits below the keyboard", async () => {
    stubPhoneLayout({ viewportTop: 0, viewportHeight: 540, composer: { top: 700, height: 96 } });

    const { menu } = renderMenu(
      { left: 0, bottom: 0 },
      { isPhone: true, indexedCount: 5, truncated: true },
    );

    // The visible viewport ends at 540, not at the composer's top: the sheet's
    // bottom edge stops 8 px above the keyboard (796 - 532 = 264 px from its
    // container's bottom).
    await waitFor(() => expect(menu.style.bottom).toBe("264px"));
    expect(menu.style.maxHeight).toBe("524px");

    const sheet = menu.getBoundingClientRect();
    expect(sheet.bottom).toBe(532);
    expect(sheet.top).toBe(8);
    expect(menu.getAttribute("data-covers")).toBeNull();
    const done = within(menu).getByRole("button", { name: "Done" }).getBoundingClientRect();
    expect(done.top).toBeGreaterThanOrEqual(0);
    expect(done.bottom).toBeLessThanOrEqual(540);

    const list = within(menu).getByRole("listbox");
    expect(list.contains(within(menu).getByText("Showing the first 5 files."))).toBe(false);
  });

  it("covers the composer when only 9 px is left above it", async () => {
    stubPhoneLayout({ viewportTop: 0, viewportHeight: 540, composer: { top: 9, height: 96 } });

    const { menu } = renderMenu({ left: 0, bottom: 0 }, { isPhone: true });

    // No room for the header and a row, so the sheet spans the visible
    // viewport instead of collapsing to zero height: 105 - 540 = -435.
    await waitFor(() => expect(menu.style.bottom).toBe("-435px"));
    expect(menu.style.maxHeight).toBe("540px");

    const sheet = menu.getBoundingClientRect();
    expect(sheet.top).toBe(0);
    expect(sheet.bottom).toBe(540);
    expect(menu.getAttribute("data-covers")).toBe("composer");
    const done = within(menu).getByRole("button", { name: "Done" }).getBoundingClientRect();
    expect(done.top).toBeGreaterThanOrEqual(0);
    expect(done.bottom).toBeLessThanOrEqual(540);
    expect(screen.getByRole("listbox", { name: "Files" })).toBeTruthy();
  });
});
