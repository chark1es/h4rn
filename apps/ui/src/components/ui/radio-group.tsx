import {
  RadioGroup as RadioGroupPrimitive,
  RadioIndicator,
  Radio as RadioPrimitive,
} from "@ui-sdk/react/radio";
import type * as React from "react";

import { cn } from "@/lib/utils";

type RadioGroupProps = Omit<React.ComponentProps<typeof RadioGroupPrimitive>, "className"> & {
  className?: string;
};

function RadioGroup({ className, ...props }: RadioGroupProps) {
  return <RadioGroupPrimitive className={cn("flex flex-col gap-2", className)} {...props} />;
}

type RadioProps = Omit<React.ComponentProps<typeof RadioPrimitive>, "className" | "children"> & {
  className?: string;
};

function Radio({ className, ...props }: RadioProps) {
  return (
    <RadioPrimitive
      className={cn(
        "hit-area relative inline-grid size-4 shrink-0 cursor-default place-items-center rounded-full bg-card shadow-[inset_0_0_0_1px_var(--input)]",
        "transition-[background-color,box-shadow] duration-micro ease-enter",
        "data-checked:bg-primary data-checked:shadow-none",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        "data-disabled:opacity-45",
        className,
      )}
      {...props}
    >
      <RadioIndicator className="pointer-events-none size-1.5 rounded-full bg-primary-foreground" />
    </RadioPrimitive>
  );
}

export { Radio, RadioGroup, type RadioGroupProps, type RadioProps };
