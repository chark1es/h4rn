import { type ReactNode, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

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
  const { t } = useTranslation("common");
  const groupRef = useRef<HTMLDivElement | null>(null);
  const [groupWidth, setGroupWidth] = useState(0);
  const open = pane !== null && pane !== false && pane !== undefined;

  useLayoutEffect(() => {
    const group = groupRef.current;
    if (!group) return;
    const update = () => setGroupWidth(group.getBoundingClientRect().width);
    update();
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(update);
    observer?.observe(group);
    return () => observer?.disconnect();
  }, []);

  const clamped = Math.min(Math.max(width, min), max);
  // UI SDK reports the share as a percentage; say the width people set.
  const valueText = t("pixelWidth", { width: Math.round(clamped) });
  const share = groupWidth > 0 ? (clamped / groupWidth) * 100 : 0;
  const layout =
    groupWidth <= 0
      ? undefined
      : !open
        ? [100]
        : side === "start"
          ? [share, 100 - share]
          : [100 - share, share];

  const paneCell = open
    ? [
        side === "end" ? (
          <ResizableHandle key="handle" aria-label={label} aria-valuetext={valueText} />
        ) : null,
        <ResizablePanel
          key={PANE}
          id={PANE}
          defaultSize={`${width}px`}
          minSize={`${min}px`}
          maxSize={`${max}px`}
        >
          {pane}
        </ResizablePanel>,
        side === "start" ? (
          <ResizableHandle key="handle" aria-label={label} aria-valuetext={valueText} />
        ) : null,
      ]
    : null;

  return (
    <Resizable
      ref={groupRef}
      className="flex-1"
      {...(layout ? { layout } : {})}
      {...(groupWidth > 0 ? { step: (STEP_PX / groupWidth) * 100 } : {})}
      onLayoutChange={(next, details) => {
        if (details.reason === "imperative" || details.reason === "resize") return;
        const paneShare = side === "start" ? next[0] : next.at(-1);
        if (paneShare === undefined || groupWidth <= 0 || next.length < 2) return;
        onWidth((paneShare / 100) * groupWidth);
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
