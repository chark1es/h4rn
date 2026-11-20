/**
 * The way back to the daemon on this machine.
 *
 * The boot transport follows the stored `ActiveRemote`, but the machine list
 * comes from the relay. Offline, that list is empty, so any affordance that
 * read the machine name from `machines` disappeared exactly when it was
 * needed. The stored descriptor always has the name; this action reads it and
 * forgets the remote using the same path as Disconnect.
 */

import { cn } from "@/lib/utils";

import { forgetMachine, shellKind, useRemote } from "./remoteStore";

/** The stored remote's name, independent of whether `machines` has loaded. */
export function useStoredRemoteName(): string | null {
  return useRemote((state) => state.active?.machineName ?? null);
}

export function UseThisMachineButton({
  machineName,
  verb = "use",
  compact = false,
  onUseLocal = forgetMachine,
  className,
}: {
  /** Defaults to the stored active remote; renders nothing when neither exists. */
  machineName?: string | null;
  /** `disconnect` names the machine in the label, as the hosts menu does. */
  verb?: "use" | "disconnect";
  /** Terser label for the narrow offline strip; the full sentence stays in the title. */
  compact?: boolean;
  onUseLocal?: () => void;
  className?: string;
}) {
  const stored = useStoredRemoteName();
  const name = machineName ?? stored;
  if (!name) return null;
  // The mobile shell has no local daemon (apps/mobile/src-tauri/src/lib.rs),
  // so "use this machine" would point at nothing. Offer only the honest
  // disconnect: drop the remote and stay offline until another machine is
  // picked.
  const mobile = shellKind() === "mobile";
  const disconnect = mobile || verb === "disconnect";
  const label = mobile
    ? "Disconnect"
    : verb === "disconnect"
      ? `Disconnect from ${name}`
      : compact
        ? "Use local"
        : "Use this machine";
  // WCAG 2.5.3: the accessible name must contain the visible label. The longer
  // explanation stays in the title, which screen readers expose as a
  // description rather than replacing the name.
  const ariaLabel = disconnect
    ? `Disconnect from ${name}`
    : compact
      ? `Use local — disconnect from ${name}`
      : `Use this machine — disconnect from ${name}`;
  return (
    <button
      type="button"
      className={cn("q-btn ghost", className)}
      style={{ height: 24 }}
      title={
        mobile
          ? `Disconnect from ${name}. This app stays offline until you pick another machine.`
          : `Disconnect from ${name} and use the daemon on this machine`
      }
      aria-label={ariaLabel}
      onClick={onUseLocal}
    >
      {label}
    </button>
  );
}
