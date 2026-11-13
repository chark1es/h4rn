import type * as React from "react";

import { cn } from "@/lib/utils";

function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-slot="input"
      className={cn(
        "h-11 w-full min-w-0 rounded-sm border border-control-border bg-raised px-2.5 text-compact text-ink outline-none transition-[border-color,box-shadow] duration-[var(--dur-fast)] selection:bg-cue-tint placeholder:text-ink-faint focus-visible:border-cue focus-visible:ring-2 focus-visible:ring-cue/40 disabled:pointer-events-none disabled:bg-panel disabled:text-ink-faint md:h-[30px]",
        "aria-invalid:border-strike aria-invalid:ring-strike/30",
        className,
      )}
      {...props}
    />
  );
}

export { Input };
