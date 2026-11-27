// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { backdropClassName } from "./dialog";
import { Sheet, SheetBody, SheetClose, SheetContent, SheetFooter, SheetTitle } from "./sheet";

afterEach(cleanup);

function Example({ onClosed, onApprove }: { onClosed: () => void; onApprove: () => void }) {
  return (
    <Sheet defaultOpen onOpenChangeComplete={(open) => (open ? undefined : onClosed())}>
      <SheetContent>
        <SheetTitle>Run npm test on studio</SheetTitle>
        <SheetBody>
          <p>npm test</p>
        </SheetBody>
        <SheetFooter>
          <SheetClose>Deny</SheetClose>
          <SheetClose action={onApprove}>Approve</SheetClose>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

describe("Sheet", () => {
  it("opens as a modal dialog named by its title", async () => {
    render(<Example onClosed={() => undefined} onApprove={() => undefined} />);
    const dialog = await screen.findByRole("dialog", { name: "Run npm test on studio" });
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(dialog.querySelector('[data-slot="sheet-grabber"]')).not.toBeNull();
  });

  it("fades its backdrop like a dialog's", async () => {
    render(<Example onClosed={() => undefined} onApprove={() => undefined} />);
    await screen.findByRole("dialog");
    const backdrop = document.querySelector('[data-slot="drawer-backdrop"]');
    expect(backdrop?.className).toBe(backdropClassName);
  });

  it("closes on Escape and reports it once the exit finishes", async () => {
    const onClosed = vi.fn();
    render(<Example onClosed={onClosed} onApprove={() => undefined} />);
    const dialog = await screen.findByRole("dialog");
    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(onClosed).toHaveBeenCalledTimes(1));
  });

  it("runs a Close part's action before it closes", async () => {
    const onClosed = vi.fn();
    const onApprove = vi.fn();
    render(<Example onClosed={onClosed} onApprove={onApprove} />);
    fireEvent.click(await screen.findByRole("button", { name: "Approve" }));
    await waitFor(() => expect(onClosed).toHaveBeenCalledTimes(1));
    expect(onApprove).toHaveBeenCalledTimes(1);
  });
});
