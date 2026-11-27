// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it } from "vitest";

import { suggestionsOf } from "@/test/popups";
import { SuggestInput } from "./autocomplete";

afterEach(cleanup);

function Model({ initial = "" }: { initial?: string }) {
  const [value, setValue] = useState(initial);
  return (
    <SuggestInput
      aria-label="Model"
      value={value}
      onValueChange={setValue}
      suggestions={["claude-opus", "claude-sonnet", "gpt-5"]}
      showLabel="Show suggestions"
      mono
    />
  );
}

describe("SuggestInput", () => {
  it("keeps text that matches no suggestion", async () => {
    render(<Model />);
    const input = screen.getByRole("combobox", { name: "Model" }) as HTMLInputElement;
    await act(async () => {
      fireEvent.change(input, { target: { value: "my-own-model" } });
    });
    expect(input.value).toBe("my-own-model");
  });

  it("narrows the suggestions while typing and fills one in with the keyboard", async () => {
    render(<Model />);
    const input = screen.getByRole("combobox", { name: "Model" }) as HTMLInputElement;
    input.focus();
    await act(async () => {
      fireEvent.change(input, { target: { value: "claude" } });
    });
    expect(screen.getAllByRole("option").map((option) => option.textContent)).toEqual([
      "claude-opus",
      "claude-sonnet",
    ]);
    await act(async () => {
      fireEvent.keyDown(input, { key: "ArrowDown" });
      fireEvent.keyDown(input, { key: "ArrowDown" });
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect(input.value).toBe("claude-sonnet");
  });

  it("lists every suggestion while the text is one of them", async () => {
    render(<Model initial="gpt-5" />);
    expect(await suggestionsOf(screen.getByRole("combobox", { name: "Model" }))).toEqual([
      "claude-opus",
      "claude-sonnet",
      "gpt-5",
    ]);
  });
});
