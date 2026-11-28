import { type CSSProperties, type ReactNode, useState } from "react";

import {
  PopoverContent,
  Popover as PopoverRoot,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";

/**
 * A small anchored panel for a short decision, like "Resolve…" or the thread
 * controls: not a menu (no arrow-key list) and not a dialog (nothing is blocked).
 */
export function Popover({
  label,
  trigger,
  children,
  align = "start",
  width = 320,
  placement = "below",
  open: controlledOpen,
  onOpenChange,
  triggerClassName = "q-btn secondary",
}: {
  /** Accessible name of the panel. */
  label: string;
  /** Renders the trigger button's content. */
  trigger: (state: { open: boolean }) => ReactNode;
  /** The panel body. Call `close` to dismiss after an action. */
  children: (api: { close: () => void }) => ReactNode;
  align?: "start" | "end";
  width?: number;
  /** Open below the trigger (default) or above it. */
  placement?: "below" | "above";
  /** Controlled open state; omit to let the popover own it. */
  open?: boolean;
  /** Called when the popover asks to open or close. */
  onOpenChange?: (open: boolean) => void;
  /** Classes for the trigger button; defaults to the secondary button. */
  triggerClassName?: string;
}) {
  const [uncontrolledOpen, setUncontrolledOpen] = useState(false);
  const open = controlledOpen ?? uncontrolledOpen;
  const setOpen = (next: boolean) => {
    if (onOpenChange) onOpenChange(next);
    else setUncontrolledOpen(next);
  };

  return (
    <PopoverRoot open={open} onOpenChange={setOpen}>
      <PopoverTrigger className={triggerClassName}>{trigger({ open })}</PopoverTrigger>
      <PopoverContent
        side={placement === "above" ? "top" : "bottom"}
        align={align}
        className="w-(--popover-width)"
        style={{ "--popover-width": `${width}px` } as CSSProperties}
      >
        <PopoverTitle className="sr-only">{label}</PopoverTitle>
        {children({ close: () => setOpen(false) })}
      </PopoverContent>
    </PopoverRoot>
  );
}
