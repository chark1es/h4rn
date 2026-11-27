import {
  SegmentedControlIndicator,
  SegmentedControlItem as SegmentedControlItemPrimitive,
  SegmentedControl as SegmentedControlPrimitive,
} from "@ui-sdk/react/segmented-control";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const segmentedControlVariants = cva(
  "group/seg relative isolate inline-flex max-w-full shrink-0 items-stretch gap-0.5 rounded-md bg-muted p-0.5 data-disabled:opacity-45",
  {
    variants: {
      size: {
        default: "h-7 phone:h-9",
        sm: "h-6",
      },
      width: {
        auto: "",
        full: "flex w-full [&>[data-slot=segmented-control-item]]:min-w-0 [&>[data-slot=segmented-control-item]]:flex-1",
      },
    },
    defaultVariants: { size: "default", width: "auto" },
  },
);

type SegmentedControlProps = Omit<
  React.ComponentProps<typeof SegmentedControlPrimitive>,
  "className" | "children"
> &
  VariantProps<typeof segmentedControlVariants> & {
    className?: string;
    children?: React.ReactNode;
  };

function SegmentedControl({ className, size, width, children, ...props }: SegmentedControlProps) {
  return (
    <SegmentedControlPrimitive
      data-size={size ?? "default"}
      className={cn(segmentedControlVariants({ size, width }), className)}
      {...props}
    >
      <SegmentedControlIndicator
        className={cn(
          "absolute -z-10 rounded-sm bg-card shadow-[inset_0_0_0_1px_var(--input)]",
          "top-(--segmented-control-indicator-top) left-(--segmented-control-indicator-left) h-(--segmented-control-indicator-height) w-(--segmented-control-indicator-width)",
          "transition-[left,width] duration-popup ease-enter motion-reduce:transition-none data-[activation-direction=none]:transition-none",
        )}
      />
      {children}
    </SegmentedControlPrimitive>
  );
}

type SegmentedControlItemProps = Omit<
  React.ComponentProps<typeof SegmentedControlItemPrimitive>,
  "className"
> & { className?: string };

function SegmentedControlItem({ className, ...props }: SegmentedControlItemProps) {
  return (
    <SegmentedControlItemPrimitive
      className={cn(
        "relative inline-flex shrink-0 cursor-default items-center justify-center gap-1.5 rounded-sm px-2.5 text-sm font-medium whitespace-nowrap text-muted-foreground select-none",
        "transition-colors duration-micro ease-enter",
        "hover:text-foreground data-checked:text-foreground",
        "outline-none focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-1 focus-visible:outline-ring",
        "data-disabled:pointer-events-none data-disabled:opacity-45",
        "phone:px-3",
        "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-3.5",
        "group-data-[size=sm]/seg:text-xs",
        className,
      )}
      {...props}
    />
  );
}

function SegmentedControlCount({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      data-slot="segmented-control-count"
      className={cn("shrink-0 font-normal text-fg-3 tabular-nums", className)}
      {...props}
    />
  );
}

export {
  SegmentedControl,
  SegmentedControlCount,
  SegmentedControlItem,
  type SegmentedControlProps,
  segmentedControlVariants,
};
