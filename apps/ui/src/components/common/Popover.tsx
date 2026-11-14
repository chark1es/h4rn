import { type ReactNode, useCallback, useEffect, useId, useRef, useState } from "react";

/**
 * A small anchored panel for a short decision: a trigger button, a raised
 * `.q-pop` card, Esc or an outside press to dismiss, focus back on the trigger.
 *
 * It is not a menu (no arrow-key list) and not a dialog (nothing is blocked),
 * which is what the design's "Resolve…" and "Hand to agent" panels are. A
 * control inside that opens its own portaled menu would count as an outside
 * press, so the panel's contents use plain buttons and inputs.
 */
export function Popover({
  label,
  trigger,
  children,
  align = "start",
  width = 320,
}: {
  /** Accessible name of the panel. */
  label: string;
  /** Renders the trigger button's content. */
  trigger: (state: { open: boolean }) => ReactNode;
  /** The panel body. Call `close` to dismiss after an action. */
  children: (api: { close: () => void }) => ReactNode;
  align?: "start" | "end";
  width?: number;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelId = useId();

  const close = useCallback(() => {
    setOpen(false);
    triggerRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!open) return;
    const onPointer = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        close();
      }
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey, true);
    };
  }, [open, close]);

  // Focus the first control when the panel opens.
  useEffect(() => {
    if (!open) return;
    const panel = document.getElementById(panelId);
    panel
      ?.querySelector<HTMLElement>(
        "input:checked, [role='radio'][aria-checked='true'], button, input",
      )
      ?.focus();
  }, [open, panelId]);

  return (
    <div ref={root} style={{ position: "relative", display: "inline-block" }}>
      <button
        ref={triggerRef}
        type="button"
        className="q-btn secondary"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        onClick={() => setOpen((value) => !value)}
      >
        {trigger({ open })}
      </button>
      {open ? (
        <div
          id={panelId}
          role="dialog"
          aria-label={label}
          className="q-pop"
          style={{ top: "calc(100% + 6px)", [align === "end" ? "right" : "left"]: 0, width }}
        >
          {children({ close })}
        </div>
      ) : null}
    </div>
  );
}
