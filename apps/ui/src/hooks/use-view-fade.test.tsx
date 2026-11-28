// @vitest-environment jsdom

import { cleanup, render } from "@testing-library/react";
import { useRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useViewFade, viewKey } from "./use-view-fade";

function View({ view }: { view: string }) {
  const ref = useRef<HTMLDivElement>(null);
  useViewFade(ref, view);
  return (
    <div ref={ref}>
      <main>{view}</main>
    </div>
  );
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("useViewFade", () => {
  it("fades the new view in by opacity only, and not on the first render", () => {
    const animate = vi.fn();
    HTMLElement.prototype.animate = animate as unknown as HTMLElement["animate"];
    const { rerender } = render(<View view="a" />);
    expect(animate).not.toHaveBeenCalled();
    rerender(<View view="b" />);
    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate.mock.calls[0]?.[0]).toEqual([{ opacity: 0 }, { opacity: 1 }]);
    rerender(<View view="b" />);
    expect(animate).toHaveBeenCalledTimes(1);
  });

  it("skips the fade under reduced motion", () => {
    const animate = vi.fn();
    HTMLElement.prototype.animate = animate as unknown as HTMLElement["animate"];
    vi.spyOn(window, "matchMedia").mockImplementation(
      (query: string) => ({ matches: query.includes("reduce") }) as MediaQueryList,
    );
    const { rerender } = render(<View view="a" />);
    rerender(<View view="b" />);
    expect(animate).not.toHaveBeenCalled();
  });
});

describe("viewKey", () => {
  it("changes with the settings section but not with a selection inside a page", () => {
    const settings = (path: string) =>
      viewKey({ kind: "placeholder", surface: "settings", path }, "main");
    expect(settings("/settings/hosts")).not.toBe(settings("/settings/forges"));
    const pulls = (path: string) =>
      viewKey({ kind: "placeholder", surface: "pulls", path }, "main");
    expect(pulls("/pulls/482")).toBe(pulls("/pulls/479"));
    expect(viewKey({ kind: "thread", threadId: "t1" }, "main")).not.toBe(
      viewKey({ kind: "thread", threadId: "t1" }, "ide"),
    );
  });
});
