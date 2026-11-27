import {
  PopoverClose as PopoverClosePrimitive,
  PopoverContent as PopoverContentPrimitive,
  PopoverDescription as PopoverDescriptionPrimitive,
  Popover as PopoverPrimitive,
  PopoverTitle as PopoverTitlePrimitive,
  PopoverTrigger as PopoverTriggerPrimitive,
} from "@ui-sdk/react/popover";
import type * as React from "react";

import { cn } from "@/lib/utils";

/** Motion every anchored popup shares: fade and scale from the trigger, faster out. */
const popupMotionClassName = cn(
  "origin-(--transform-origin) transition-[opacity,scale] duration-popup ease-enter",
  "data-starting-style:scale-96 data-starting-style:opacity-0",
  "data-ending-style:scale-96 data-ending-style:opacity-0 data-ending-style:duration-popup-exit data-ending-style:ease-exit",
  "motion-reduce:transition-opacity",
);

const popupSurfaceClassName = cn(
  "rounded-lg border border-border bg-popover text-sm text-popover-foreground shadow-popup outline-none",
  popupMotionClassName,
);

const Popover = PopoverPrimitive;
const PopoverTrigger = PopoverTriggerPrimitive;
const PopoverClose = PopoverClosePrimitive;

type PopoverContentProps = Omit<
  React.ComponentProps<typeof PopoverContentPrimitive>,
  "className"
> & {
  className?: string;
};

function PopoverContent({
  className,
  sideOffset = 6,
  collisionPadding = 8,
  ...props
}: PopoverContentProps) {
  return (
    <PopoverContentPrimitive
      sideOffset={sideOffset}
      collisionPadding={collisionPadding}
      className={cn(
        popupSurfaceClassName,
        "max-h-(--available-height) w-80 max-w-[calc(100vw-1rem)] overflow-y-auto p-3.5",
        className,
      )}
      {...props}
    />
  );
}

function PopoverTitle({
  className,
  ...props
}: Omit<React.ComponentProps<typeof PopoverTitlePrimitive>, "className"> & { className?: string }) {
  return (
    <PopoverTitlePrimitive
      className={cn("m-0 text-sm font-semibold text-foreground", className)}
      {...props}
    />
  );
}

function PopoverDescription({
  className,
  ...props
}: Omit<React.ComponentProps<typeof PopoverDescriptionPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <PopoverDescriptionPrimitive
      className={cn("m-0 text-xs text-muted-foreground", className)}
      {...props}
    />
  );
}

export {
  Popover,
  PopoverClose,
  PopoverContent,
  type PopoverContentProps,
  PopoverDescription,
  PopoverTitle,
  PopoverTrigger,
  popupMotionClassName,
  popupSurfaceClassName,
};
