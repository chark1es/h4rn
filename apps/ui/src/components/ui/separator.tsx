import { Separator as SeparatorPrimitive } from "@ui-sdk/react/separator";
import type * as React from "react";

import { cn } from "@/lib/utils";

type SeparatorProps = Omit<React.ComponentProps<typeof SeparatorPrimitive>, "className"> & {
  className?: string;
};

function Separator({ className, decorative = true, ...props }: SeparatorProps) {
  return (
    <SeparatorPrimitive
      decorative={decorative}
      className={cn(
        "shrink-0 bg-border data-[orientation=horizontal]:h-px data-[orientation=horizontal]:w-full data-[orientation=vertical]:w-px data-[orientation=vertical]:self-stretch",
        className,
      )}
      {...props}
    />
  );
}

export { Separator, type SeparatorProps };
