/** jsdom lacks the top-layer and layout APIs UI SDK popups use; call before anything opens. */
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
