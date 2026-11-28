// @vitest-environment jsdom

import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Kbd, KeyCaps } from "./kbd";

afterEach(cleanup);

describe("Kbd", () => {
  it("draws glyph keys as icons even when the shortcut is built from several children", () => {
    const { container } = render(<Kbd>Ctrl {"↵"}</Kbd>);
    const cap = container.querySelector('[data-slot="kbd"]');
    expect(cap?.getAttribute("aria-label")).toBe("Ctrl Return");
    expect(cap?.textContent).toBe("Ctrl ");
    expect(cap?.querySelector("svg")?.getAttribute("aria-hidden")).toBe("true");
  });

  it("renders one keycap per key, keeping repeated keys", () => {
    const { container } = render(<KeyCaps value="G G ⇧" />);
    const caps = [...container.querySelectorAll('[data-slot="kbd"]')];
    expect(caps.map((cap) => cap.getAttribute("aria-label"))).toEqual(["G", "G", "Shift"]);
    expect(container.querySelector('[data-slot="kbd-group"]')).toBeTruthy();
  });
});
