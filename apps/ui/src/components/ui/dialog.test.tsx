// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import "@/lib/i18n";
import { stubPhoneLayout, stubPopupApis } from "@/test/popups";
import { Button } from "./button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "./dialog";

beforeEach(() => {
  stubPopupApis();
  stubPhoneLayout(false);
});

afterEach(cleanup);

function Example({ onSave = () => undefined }: { onSave?: () => void }) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger>Rename</DialogTrigger>
      <DialogContent size="sm">
        <DialogHeader>
          <DialogTitle>Rename thread</DialogTitle>
          <DialogDescription>Shown in the sidebar.</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <input aria-label="Title" defaultValue="Fix the build" />
        </DialogBody>
        <DialogFooter>
          <Button variant="secondary" onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button
            onClick={() => {
              onSave();
              setOpen(false);
            }}
          >
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

describe("Dialog", () => {
  it("opens from its trigger, named and described, with the close button on the title row", async () => {
    render(<Example />);
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    const dialog = await screen.findByRole("dialog", { name: "Rename thread" });
    expect(screen.getByText("Shown in the sidebar.").closest("[role=dialog]")).toBe(dialog);
    expect(dialog.dataset.presentation).toBe("dialog");
    expect(dialog.dataset.size).toBe("sm");
    const header = dialog.querySelector("[data-slot=dialog-header]");
    expect(header?.querySelector("button[aria-label=Close]")).toBeTruthy();
    const footer = dialog.querySelector("[data-slot=dialog-footer]");
    const names = [...(footer?.querySelectorAll("button") ?? [])].map((b) => b.textContent);
    expect(names).toEqual(["Cancel", "Save"]);
  });

  it("moves focus in on open, closes on Escape and gives focus back to the trigger", async () => {
    render(<Example />);
    const trigger = screen.getByRole("button", { name: "Rename" });
    trigger.focus();
    fireEvent.click(trigger);
    const dialog = await screen.findByRole("dialog", { name: "Rename thread" });
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    fireEvent.keyDown(document.activeElement as Element, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(document.activeElement).toBe(trigger);
  });

  it("closes from the close button and from Cancel, and runs Save", async () => {
    const onSave = vi.fn();
    render(<Example onSave={onSave} />);
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    fireEvent.click(await screen.findByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(onSave).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    fireEvent.click(await screen.findByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(onSave).toHaveBeenCalledTimes(1);
  });

  it("presents as a bottom sheet with a grab handle on the phone layout", async () => {
    stubPhoneLayout(true);
    render(<Example />);
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    const dialog = await screen.findByRole("dialog", { name: "Rename thread" });
    expect(dialog.dataset.presentation).toBe("sheet");
    expect(dialog.querySelector("[data-slot=sheet-grabber]")?.getAttribute("aria-hidden")).toBe(
      "true",
    );
  });
});

function Confirm({ onConfirm }: { onConfirm: () => void }) {
  const [open, setOpen] = useState(true);
  return (
    <ConfirmDialog
      open={open}
      title="Remove the worktree?"
      description="Its folder is deleted."
      confirmLabel="Remove"
      onOpenChange={setOpen}
      onConfirm={onConfirm}
    />
  );
}

describe("ConfirmDialog", () => {
  it("asks with an alert dialog and starts on Cancel", async () => {
    render(<Confirm onConfirm={() => undefined} />);
    const dialog = await screen.findByRole("alertdialog", { name: "Remove the worktree?" });
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("button", { name: "Cancel" })),
    );
    expect(
      dialog.querySelector("[data-slot=alert-dialog-footer]")?.lastElementChild?.textContent,
    ).toBe("Remove");
  });

  it("runs the action only on the confirming answer", async () => {
    const onConfirm = vi.fn();
    render(<Confirm onConfirm={onConfirm} />);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(onConfirm).not.toHaveBeenCalled();

    cleanup();
    render(<Confirm onConfirm={onConfirm} />);
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Remove" }));
    });
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });
});
