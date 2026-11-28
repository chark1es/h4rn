// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it } from "vitest";

import { Input } from "./input";
import { Textarea } from "./textarea";

afterEach(cleanup);

function Controlled({ multiline }: { multiline?: boolean }) {
  const [value, setValue] = useState("atlas");
  return (
    <>
      {multiline ? (
        <Textarea
          aria-label="Task"
          value={value}
          onChange={(event) => setValue(event.target.value)}
        />
      ) : (
        <Input aria-label="Host" value={value} onChange={(event) => setValue(event.target.value)} />
      )}
      <output aria-label="Value">{value}</output>
    </>
  );
}

describe("Input", () => {
  it("is a named native text field that follows a controlled value", () => {
    render(<Controlled />);
    const input = screen.getByRole("textbox", { name: "Host" }) as HTMLInputElement;
    expect(input.tagName).toBe("INPUT");
    expect(input.dataset.slot).toBe("input");
    expect(input.hasAttribute("data-filled")).toBe(true);

    fireEvent.change(input, { target: { value: "orion" } });
    expect(input.value).toBe("orion");
    expect(screen.getByRole("status", { name: "Value" }).textContent).toBe("orion");
  });

  it("marks itself disabled and invalid for styling", () => {
    render(<Input aria-label="Branch" disabled aria-invalid />);
    const input = screen.getByRole("textbox", { name: "Branch" }) as HTMLInputElement;
    expect(input.disabled).toBe(true);
    expect(input.hasAttribute("data-disabled")).toBe(true);
    expect(input.getAttribute("aria-invalid")).toBe("true");
  });
});

describe("Textarea", () => {
  it("is a named multi-line field that follows a controlled value", () => {
    render(<Controlled multiline />);
    const textarea = screen.getByRole("textbox", { name: "Task" }) as HTMLTextAreaElement;
    expect(textarea.tagName).toBe("TEXTAREA");
    expect(textarea.dataset.slot).toBe("textarea");

    fireEvent.change(textarea, { target: { value: "Fix the flaky test" } });
    expect(textarea.value).toBe("Fix the flaky test");
    expect(screen.getByRole("status", { name: "Value" }).textContent).toBe("Fix the flaky test");
  });
});
