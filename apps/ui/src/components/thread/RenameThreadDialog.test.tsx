// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
// Starts i18n with the English catalogue, as main.tsx does in the app.
import "@/lib/i18n";

const renameThread = vi.hoisted(() => vi.fn(async () => true));

vi.mock("@/lib/store/appStore", () => ({ renameThread }));

import { RenameThreadDialog } from "./RenameThreadDialog";

const THREAD = "th_1" as never;

function renderDialog(initial: string, open = true) {
  const onOpenChange = vi.fn();
  const view = render(
    <RenameThreadDialog
      threadId={THREAD}
      initial={initial}
      open={open}
      onOpenChange={onOpenChange}
    />,
  );
  return { ...view, onOpenChange };
}

describe("RenameThreadDialog", () => {
  beforeEach(() => {
    renameThread.mockClear();
    renameThread.mockResolvedValue(true);
  });

  afterEach(cleanup);

  it("keeps the draft when a generated title arrives while it is open", () => {
    const { rerender, onOpenChange } = renderDialog("Fix the build");
    const input = screen.getByLabelText("Thread title") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "My own name" } });

    rerender(
      <RenameThreadDialog
        threadId={THREAD}
        initial="Generated title"
        open
        onOpenChange={onOpenChange}
      />,
    );
    expect((screen.getByLabelText("Thread title") as HTMLInputElement).value).toBe("My own name");
  });

  it("records an untouched draft when the title changed underneath", async () => {
    const { rerender, onOpenChange } = renderDialog("Fix the build");
    rerender(
      <RenameThreadDialog
        threadId={THREAD}
        initial="Generated title"
        open
        onOpenChange={onOpenChange}
      />,
    );
    // The dialog still shows the name it opened with.
    expect((screen.getByLabelText("Thread title") as HTMLInputElement).value).toBe("Fix the build");

    fireEvent.click(screen.getByRole("button", { name: "Save name" }));
    await waitFor(() => expect(renameThread).toHaveBeenCalledWith(THREAD, "Fix the build"));
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("closes without a request when nothing changed", async () => {
    const { onOpenChange } = renderDialog("Fix the build");
    fireEvent.click(screen.getByRole("button", { name: "Save name" }));
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(renameThread).not.toHaveBeenCalled();
  });
});
