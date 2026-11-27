import { createResizableController } from "@ui-sdk/react/resizable";
import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";

import { Resizable, ResizableHandle, ResizablePanel } from "@/components/ui/resizable";

const PANE = "pane";
const STEP_PX = 16;

/**
 * A row with one column whose width is kept in pixels: the sidebar before the
 * rest of the app, or the right panel after the main view. Dragging or the
 * arrow keys report the new width; window resizes keep the pixel width.
 */
export function ShellSplit({
  side,
  pane,
  width,
  min,
  max,
  label,
  onWidth,
  children,
}: {
  side: "start" | "end";
  pane: ReactNode;
  width: number;
  min: number;
  max: number;
  label: string;
  onWidth: (width: number) => void;
  children: ReactNode;
}) {
  const controller = useMemo(() => createResizableController(), []);
  const groupRef = useRef<HTMLDivElement | null>(null);
  const widthRef = useRef(width);
  widthRef.current = width;
  const [groupWidth, setGroupWidth] = useState(0);
  const groupWidthRef = useRef(0);
  const open = pane !== null && pane !== false && pane !== undefined;

  useLayoutEffect(() => {
    const group = groupRef.current;
    if (!group) return;
    const update = () => {
      const next = group.getBoundingClientRect().width;
      groupWidthRef.current = next;
      setGroupWidth(next);
      controller.resize(PANE, `${widthRef.current}px`);
    };
    update();
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(update);
    observer?.observe(group);
    return () => observer?.disconnect();
  }, [controller]);

  useEffect(() => {
    if (open) controller.resize(PANE, `${width}px`);
  }, [controller, open, width]);

  const paneCell = open
    ? [
        side === "end" ? <ResizableHandle key="handle" aria-label={label} /> : null,
        <ResizablePanel
          key={PANE}
          id={PANE}
          defaultSize={`${width}px`}
          minSize={`${min}px`}
          maxSize={`${max}px`}
        >
          {pane}
        </ResizablePanel>,
        side === "start" ? <ResizableHandle key="handle" aria-label={label} /> : null,
      ]
    : null;

  return (
    <Resizable
      ref={groupRef}
      controller={controller}
      className="flex-1"
      {...(groupWidth > 0 ? { step: (STEP_PX / groupWidth) * 100 } : {})}
      onLayoutChange={(layout, details) => {
        if (details.reason === "imperative" || details.reason === "resize") return;
        const share = side === "start" ? layout[0] : layout.at(-1);
        if (share === undefined || groupWidthRef.current <= 0 || layout.length < 2) return;
        onWidth((share / 100) * groupWidthRef.current);
      }}
    >
      {side === "start" ? paneCell : null}
      <ResizablePanel key="main" className="flex-col">
        {children}
      </ResizablePanel>
      {side === "end" ? paneCell : null}
    </Resizable>
  );
}
