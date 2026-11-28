import {
  CollapsiblePanel as CollapsiblePanelPrimitive,
  Collapsible as CollapsiblePrimitive,
  CollapsibleTrigger as CollapsibleTriggerPrimitive,
} from "@ui-sdk/react/collapsible";
import { ChevronRightIcon } from "lucide-react";
import type * as React from "react";

import { cn } from "@/lib/utils";

type ClassName = { className?: string };

function Collapsible({
  className,
  ...props
}: Omit<React.ComponentProps<typeof CollapsiblePrimitive>, "className"> & ClassName) {
  return <CollapsiblePrimitive className={cn("group/collapsible", className)} {...props} />;
}

function CollapsibleTrigger({
  className,
  children,
  ...props
}: Omit<React.ComponentProps<typeof CollapsibleTriggerPrimitive>, "className" | "children"> &
  ClassName & { children?: React.ReactNode }) {
  return (
    <CollapsibleTriggerPrimitive
      className={cn(
        "hit-area relative inline-flex cursor-default items-center gap-1.5 rounded-sm text-sm font-medium text-muted-foreground select-none",
        "transition-colors duration-micro ease-enter hover:text-foreground data-open:text-foreground",
        "outline-none focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-2 focus-visible:outline-ring",
        "data-disabled:opacity-45",
        className,
      )}
      {...props}
    >
      <ChevronRightIcon
        className="size-3.5 shrink-0 transition-transform duration-micro ease-enter group-data-open/collapsible:rotate-90 motion-reduce:transition-none rtl:-scale-x-100"
        strokeWidth={1.75}
        aria-hidden
      />
      {children}
    </CollapsibleTriggerPrimitive>
  );
}

function CollapsiblePanel({
  className,
  ...props
}: Omit<React.ComponentProps<typeof CollapsiblePanelPrimitive>, "className"> & ClassName) {
  return (
    <CollapsiblePanelPrimitive
      className={cn(
        "transition-opacity duration-micro ease-enter data-ending-style:opacity-0 data-ending-style:duration-micro-exit data-starting-style:opacity-0 motion-reduce:transition-none",
        className,
      )}
      {...props}
    />
  );
}

export { Collapsible, CollapsiblePanel, CollapsibleTrigger };
