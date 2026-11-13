import type * as React from "react";

import { cn } from "@/lib/utils";

function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "flex min-h-20 w-full rounded-sm border border-control-border bg-raised px-2.5 py-2 text-compact text-ink outline-none transition-[border-color,box-shadow] duration-[var(--dur-fast)] selection:bg-cue-tint placeholder:text-ink-faint focus-visible:border-cue focus-visible:ring-2 focus-visible:ring-cue/40 disabled:cursor-not-allowed disabled:bg-panel disabled:text-ink-faint",
        "aria-invalid:border-strike aria-invalid:ring-strike/30",
        className,
      )}
      {...props}
    />
  );
}

export { Textarea };
