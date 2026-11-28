/** jsdom lacks the top-layer, animation and layout APIs UI SDK popups use; call before anything opens. */
export function stubPopupApis(): void {
  HTMLElement.prototype.showPopover = () => {};
  HTMLElement.prototype.hidePopover = () => {};
  Element.prototype.getAnimations = () => [];
  Element.prototype.animate = () => {
    const noop = () => undefined;
    return {
      finished: Promise.resolve(),
      currentTime: 0,
      playbackRate: 1,
      cancel: noop,
      finish: noop,
      pause: noop,
      play: noop,
      reverse: noop,
      commitStyles: noop,
      persist: noop,
      updatePlaybackRate: noop,
      addEventListener: noop,
      removeEventListener: noop,
    } as unknown as Animation;
  };
  Element.prototype.scrollIntoView = () => {};
  Element.prototype.checkVisibility = () => true;
  Element.prototype.getClientRects = () => [] as unknown as DOMRectList;
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
  document.elementFromPoint = () => null;
  document.elementsFromPoint = () => [];
  globalThis.ResizeObserver ??= class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
  if (!document.getElementById("popover-stub")) {
    document.head.append(
      Object.assign(document.createElement("style"), {
        id: "popover-stub",
        textContent: "[popover] { display: block !important }",
      }),
    );
  }
}

/** Popup stubs plus a desktop `matchMedia` when jsdom has none. */
export function installPopupStubs(): void {
  if (typeof window.matchMedia !== "function") stubPhoneLayout(false);
  stubPopupApis();
}

/** Pretend the window is (or isn't) the phone layout. */
export function stubPhoneLayout(phone: boolean): void {
  window.matchMedia = ((query: string) => ({
    matches: query.includes("max-width: 639px") ? phone : false,
    media: query,
    onchange: null,
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
    addListener: () => undefined,
    removeListener: () => undefined,
    dispatchEvent: () => false,
  })) as typeof window.matchMedia;
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
