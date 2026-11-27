// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ChipSelect } from "@/components/common/ChipSelect";

afterEach(cleanup);

async function press(key: string, target: Element | null = document.activeElement) {
  if (!target) throw new Error("nothing is focused");
  await act(async () => {
    fireEvent.keyDown(target, { key });
  });
}

const fruits = [
  { value: "apple", label: "Apple" },
  { value: "banana", label: "Banana", hint: "Yellow" },
  { value: "cherry", label: "Cherry", disabled: true },
  { value: "date", label: "Date" },
];

function Picker({ onChange = vi.fn() }: { onChange?: (value: string) => void }) {
  const [value, setValue] = useState("apple");
  return (
    <ChipSelect
      label="Fruit"
      value={value}
      options={fruits}
      onChange={(next) => {
        setValue(next);
        onChange(next);
      }}
    />
  );
}

describe("ChipSelect", () => {
  it("names the trigger with its label and choice, and checks the chosen option", async () => {
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Fruit: Apple" });
    await act(async () => {
      fireEvent.click(trigger);
    });
    const apple = screen.getByRole("option", { name: "Apple" });
    expect(apple.getAttribute("aria-selected")).toBe("true");
    expect(apple.querySelector('[data-slot="select-item-indicator"]')).not.toBeNull();
    // Unchosen rows keep the check's column, so labels line up.
    const date = screen.getByRole("option", { name: "Date" });
    expect(date.querySelector('[data-slot="select-item-indicator"]')).not.toBeNull();
    expect(screen.getByRole("option", { name: /Banana/ }).textContent).toContain("Yellow");
  });

  it("moves with arrows past disabled options, chooses with Enter and returns focus", async () => {
    const onChange = vi.fn();
    render(<Picker onChange={onChange} />);
    const trigger = screen.getByRole("combobox", { name: "Fruit: Apple" });
    trigger.focus();
    await press("ArrowDown", trigger);
    expect(screen.getByRole("listbox")).toBeTruthy();
    await press("ArrowDown");
    await press("ArrowDown");
    expect(document.activeElement?.textContent).toContain("Date");
    await press("Enter");
    expect(onChange).toHaveBeenCalledWith("date");
    expect(document.activeElement).toBe(trigger);
    expect(trigger.getAttribute("aria-label")).toBe("Fruit: Date");
  });

  it("finds an option by typing and closes on Escape without choosing", async () => {
    const onChange = vi.fn();
    render(<Picker onChange={onChange} />);
    const trigger = screen.getByRole("combobox", { name: "Fruit: Apple" });
    trigger.focus();
    await press("ArrowDown", trigger);
    await press("b");
    expect(document.activeElement?.textContent).toContain("Banana");
    await press("Escape");
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("reports a pick of the option already chosen", async () => {
    const onChange = vi.fn();
    render(<Picker onChange={onChange} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("combobox", { name: "Fruit: Apple" }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("option", { name: "Apple" }));
    });
    expect(onChange).toHaveBeenCalledWith("apple");
  });
});
