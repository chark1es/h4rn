import {
  ResizableHandle as ResizableHandlePrimitive,
  ResizablePanel as ResizablePanelPrimitive,
  Resizable as ResizablePrimitive,
} from "@ui-sdk/react/resizable";
import type * as React from "react";

import { cn } from "@/lib/utils";

type ClassName = { className?: string };

function Resizable({
  className,
  ...props
}: Omit<React.ComponentProps<typeof ResizablePrimitive>, "className"> & ClassName) {
  return <ResizablePrimitive className={cn("min-h-0 min-w-0", className)} {...props} />;
}

function ResizablePanel({
  className,
  ...props
}: Omit<React.ComponentProps<typeof ResizablePanelPrimitive>, "className"> & ClassName) {
  return <ResizablePanelPrimitive className={cn("flex min-h-0 min-w-0", className)} {...props} />;
}

function ResizableHandle({
  className,
  ...props
}: Omit<React.ComponentProps<typeof ResizableHandlePrimitive>, "className"> & ClassName) {
  return (
    <ResizableHandlePrimitive
      className={cn(
        "relative z-10 shrink-0 outline-none",
        "data-[orientation=horizontal]:w-0 data-[orientation=vertical]:h-0",
        "after:absolute after:bg-transparent after:transition-colors after:duration-micro after:ease-enter",
        "data-[orientation=horizontal]:after:inset-y-0 data-[orientation=horizontal]:after:-start-px data-[orientation=horizontal]:after:w-0.5",
        "data-[orientation=vertical]:after:inset-x-0 data-[orientation=vertical]:after:-top-px data-[orientation=vertical]:after:h-0.5",
        "data-hovering:after:bg-focus data-dragging:after:bg-focus focus-visible:after:bg-focus",
        "data-disabled:pointer-events-none",
        className,
      )}
      {...props}
    />
  );
}

export { Resizable, ResizableHandle, ResizablePanel };
