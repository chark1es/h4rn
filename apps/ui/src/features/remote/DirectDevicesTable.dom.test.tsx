// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DirectDevice } from "@/gen";
import { DirectDevicesTable, directDeviceName } from "./NetworkAccess";

afterEach(cleanup);

const device = (key: string): DirectDevice => ({
  key,
  scopes: ["view"],
  paired_at: null,
  connected: false,
});

const KEYS = ["a1b2c3d4e5f6a7b8", "f0e1d2c3b4a59687"];

function Table({
  onRevoke,
  revoking = null,
}: {
  onRevoke: (key: string) => Promise<boolean>;
  revoking?: string | null;
}) {
  const [devices, setDevices] = useState(KEYS.map(device));
  return (
    <DirectDevicesTable
      devices={devices}
      now={Date.now()}
      revoking={revoking}
      onRevoke={async (key) => {
        const ok = await onRevoke(key);
        if (ok) setDevices((list) => list.filter((d) => d.key !== key));
        return ok;
      }}
    />
  );
}

describe("DirectDevicesTable", () => {
  it("asks before revoking and returns focus to the button on Cancel", async () => {
    const onRevoke = vi.fn(async () => true);
    render(<Table onRevoke={onRevoke} />);
    const name = directDeviceName(KEYS[0] as string);
    const trigger = screen.getByRole("button", { name: `Revoke ${name}` });
    act(() => trigger.focus());
    fireEvent.click(trigger);

    const dialog = await screen.findByRole("alertdialog", { name: `Revoke ${name}?` });
    expect(dialog.textContent).toContain("Pair it again");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(onRevoke).not.toHaveBeenCalled();
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  it("revokes on confirm and keeps focus on the next row when the row goes", async () => {
    const onRevoke = vi.fn(async () => true);
    render(<Table onRevoke={onRevoke} />);
    const first = directDeviceName(KEYS[0] as string);
    const second = directDeviceName(KEYS[1] as string);
    const trigger = screen.getByRole("button", { name: `Revoke ${first}` });
    act(() => trigger.focus());
    fireEvent.click(trigger);

    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(
        Array.from(dialog.querySelectorAll("button")).find(
          (b) => b.textContent === "Revoke",
        ) as HTMLElement,
      );
    });
    expect(onRevoke).toHaveBeenCalledWith(KEYS[0]);
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("button", { name: `Revoke ${second}` })),
    );
  });

  it("disables a row's Revoke while its request runs", () => {
    render(<Table onRevoke={async () => true} revoking={KEYS[0] as string} />);
    const first = directDeviceName(KEYS[0] as string);
    const second = directDeviceName(KEYS[1] as string);
    expect(screen.getByRole("button", { name: `Revoke ${first}` }).hasAttribute("disabled")).toBe(
      true,
    );
    expect(screen.getByRole("button", { name: `Revoke ${second}` }).hasAttribute("disabled")).toBe(
      false,
    );
  });
});
