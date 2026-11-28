// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { ScrollArea } from "./scroll-area";

afterEach(cleanup);

describe("ScrollArea", () => {
  it("names its scrolling region and keeps the content inside the viewport", () => {
    render(
      <ScrollArea label="Changed files" className="h-40">
        <p>src/auth/session.ts</p>
      </ScrollArea>,
    );
    const region = screen.getByRole("region", { name: "Changed files" });
    expect(region.getAttribute("data-slot")).toBe("scroll-area-viewport");
    expect(region.textContent).toContain("src/auth/session.ts");
    expect(document.querySelector('[data-slot="scroll-area-scrollbar"]')).not.toBeNull();
  });
});
