import {
  ScrollAreaContent,
  ScrollArea as ScrollAreaPrimitive,
  ScrollAreaScrollbar,
  ScrollAreaThumb,
  ScrollAreaViewport,
} from "@ui-sdk/react/scroll-area";
import type * as React from "react";

import { cn } from "@/lib/utils";

type ScrollAreaProps = Omit<React.ComponentProps<typeof ScrollAreaPrimitive>, "className"> & {
  className?: string;
  /** Names the scrolling region, so keyboard users know what they are in. */
  label?: string;
};

/** Native scrolling with a thin scrollbar that shows while the pointer is over it. */
function ScrollArea({ className, label, children, ...props }: ScrollAreaProps) {
  return (
    <ScrollAreaPrimitive className={cn("relative overflow-hidden", className)} {...props}>
      <ScrollAreaViewport
        {...(label ? { role: "region", "aria-label": label } : {})}
        className="size-full overscroll-contain rounded-[inherit] outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring"
      >
        <ScrollAreaContent>{children as React.ReactNode}</ScrollAreaContent>
      </ScrollAreaViewport>
      <ScrollBar />
    </ScrollAreaPrimitive>
  );
}

function ScrollBar({
  className,
  orientation = "vertical",
  ...props
}: Omit<React.ComponentProps<typeof ScrollAreaScrollbar>, "className"> & { className?: string }) {
  return (
    <ScrollAreaScrollbar
      orientation={orientation}
      className={cn(
        "absolute flex touch-none p-0.5 opacity-0 transition-opacity duration-popup ease-enter select-none",
        "data-visible:opacity-100 data-visible:duration-micro",
        orientation === "vertical"
          ? "inset-y-0 right-0 w-2.5"
          : "inset-x-0 bottom-0 h-2.5 flex-col",
        className,
      )}
      {...props}
    >
      <ScrollAreaThumb className="relative flex-1 rounded-full bg-fill-2 hover:bg-fg-3 data-dragging:bg-fg-3" />
    </ScrollAreaScrollbar>
  );
}

export { ScrollArea, type ScrollAreaProps, ScrollBar };
