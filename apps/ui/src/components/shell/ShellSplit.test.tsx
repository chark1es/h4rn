// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";

import "@/lib/i18n";
import { installPopupStubs } from "@/test/popups";
import { ShellSplit } from "./ShellSplit";

beforeAll(installPopupStubs);
afterEach(cleanup);

describe("ShellSplit", () => {
  it("says the pane's width in pixels on its separator", () => {
    render(
      <ShellSplit
        side="start"
        pane={<nav>Sidebar</nav>}
        width={288.4}
        min={240}
        max={420}
        label="Resize sidebar"
        onWidth={() => undefined}
      >
        <main>Main</main>
      </ShellSplit>,
    );
    const handle = screen.getByRole("separator", { name: "Resize sidebar" });
    expect(handle.getAttribute("aria-valuetext")).toBe("288 px");
  });
});
