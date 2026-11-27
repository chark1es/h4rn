/**
 * jsdom lacks the popover, animation and layout APIs UI SDK popups call.
 * Call once per test file, before anything opens.
 */
export function installPopupStubs(): void {
  HTMLElement.prototype.showPopover = () => {};
  HTMLElement.prototype.hidePopover = () => {};
  Element.prototype.getAnimations = () => [];
  Element.prototype.scrollIntoView = () => {};
  Element.prototype.checkVisibility = () => true;
  Element.prototype.getClientRects = () => [] as unknown as DOMRectList;
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
  document.elementFromPoint = () => null;
  document.elementsFromPoint = () => [];
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  if (typeof window.matchMedia !== "function") {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      configurable: true,
      value: (query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addListener: () => undefined,
        removeListener: () => undefined,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
        dispatchEvent: () => false,
      }),
    });
  }
  if (!document.head.querySelector("style[data-popup-stubs]")) {
    const style = document.createElement("style");
    style.dataset.popupStubs = "";
    // i18n-ignore-next-line: CSS, not copy
    style.textContent = "[popover] { display: block !important }";
    document.head.append(style);
  }
}

/** Opens a SuggestInput's list from its button and reads the suggestions it offers. */
export async function suggestionsOf(input: HTMLElement): Promise<string[]> {
  const { act, fireEvent } = await import("@testing-library/react");
  const show = input.parentElement?.querySelector<HTMLElement>(
    '[data-slot="autocomplete-trigger"]',
  );
  // i18n-ignore-next-line: a test failure, not copy
  if (!show) throw new Error("the field has no suggestions button");
  await act(async () => {
    fireEvent.click(show);
  });
  const list = document.getElementById(input.getAttribute("aria-controls") ?? "");
  const options = [...(list ?? document).querySelectorAll('[role="option"]')].map(
    (option) => option.textContent ?? "",
  );
  await act(async () => {
    fireEvent.keyDown(input, { key: "Escape" });
  });
  return options;
}
