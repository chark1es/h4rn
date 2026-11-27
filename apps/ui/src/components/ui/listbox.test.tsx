// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Listbox, ListboxItem } from "./listbox";

afterEach(cleanup);

describe("Listbox", () => {
  it("moves focus with arrows and chooses only on Enter", async () => {
    const onValueChange = vi.fn();
    render(
      <Listbox aria-label="Mode" activation="manual" value="ask" onValueChange={onValueChange}>
        <ListboxItem value="ask">Ask</ListboxItem>
        <ListboxItem value="plan">Plan</ListboxItem>
      </Listbox>,
    );
    const ask = screen.getByRole("option", { name: "Ask" });
    expect(ask.getAttribute("aria-selected")).toBe("true");
    ask.focus();
    await act(async () => {
      fireEvent.keyDown(ask, { key: "ArrowDown" });
    });
    expect(document.activeElement?.textContent).toBe("Plan");
    expect(onValueChange).not.toHaveBeenCalled();
    await act(async () => {
      fireEvent.keyDown(document.activeElement as Element, { key: "Enter" });
    });
    expect(onValueChange).toHaveBeenCalledWith("plan", expect.anything());
  });
});
