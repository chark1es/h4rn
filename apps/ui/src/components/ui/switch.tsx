import { Switch as SwitchPrimitive, SwitchThumb } from "@ui-sdk/react/switch";
import type * as React from "react";

import { cn } from "@/lib/utils";

type SwitchProps = Omit<React.ComponentProps<typeof SwitchPrimitive>, "className" | "children"> & {
  className?: string;
};

function Switch({ className, ...props }: SwitchProps) {
  return (
    <SwitchPrimitive
      className={cn(
        "hit-area group/switch relative inline-flex h-4.5 w-8 shrink-0 cursor-default items-center rounded-full p-0.5 align-middle",
        "transition-[background-color,box-shadow] duration-micro ease-enter",
        "data-unchecked:bg-input data-checked:bg-foreground",
        "outline-none focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-2 focus-visible:outline-ring",
        "data-disabled:opacity-45 data-readonly:cursor-not-allowed",
        "data-invalid:shadow-[0_0_0_1px_var(--danger)]",
        className,
      )}
      {...props}
    >
      <SwitchThumb
        className={cn(
          "pointer-events-none block size-3.5 rounded-full bg-bg-raised shadow-[0_1px_2px_oklch(0_0_0/0.25)]",
          "transition-transform duration-micro ease-enter motion-reduce:transition-none",
          "data-checked:translate-x-3.5 rtl:data-checked:-translate-x-3.5",
        )}
      />
    </SwitchPrimitive>
  );
}

export { Switch, type SwitchProps };
