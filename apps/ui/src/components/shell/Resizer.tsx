import { useRef, useState } from "react";

import { cn } from "@/lib/utils";

/**
 * Vertical drag handle for the two resizable columns. Pointer capture keeps
 * the drag alive over the thread text, and arrow keys satisfy the separator
 * keyboard contract.
 */
export function Resizer({
  side,
  width,
  min,
  max,
  label,
  onChange,
}: {
  side: "side" | "panel";
  width: number;
  min: number;
  max: number;
  label: string;
  onChange: (width: number) => void;
}) {
  const [drag, setDrag] = useState(false);
  const start = useRef({ x: 0, width });

  const clamp = (value: number) => Math.max(min, Math.min(max, Math.round(value)));

  return (
    <hr
      className={cn("q-resizer", side, drag && "drag")}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      onKeyDown={(event) => {
        if (event.key === "ArrowLeft") {
          event.preventDefault();
          onChange(clamp(width + (side === "side" ? -16 : 16)));
        } else if (event.key === "ArrowRight") {
          event.preventDefault();
          onChange(clamp(width + (side === "side" ? 16 : -16)));
        }
      }}
      onPointerDown={(event) => {
        event.preventDefault();
        start.current = { x: event.clientX, width };
        setDrag(true);
        const move = (moveEvent: PointerEvent) => {
          const delta = moveEvent.clientX - start.current.x;
          onChange(clamp(start.current.width + (side === "side" ? delta : -delta)));
        };
        const up = () => {
          setDrag(false);
          window.removeEventListener("pointermove", move);
          window.removeEventListener("pointerup", up);
        };
        window.addEventListener("pointermove", move);
        window.addEventListener("pointerup", up);
      }}
    />
  );
}
