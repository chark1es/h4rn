import type * as React from "react";

import { cn } from "@/lib/utils";

function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "flex min-h-20 w-full rounded-[6px] border-0 bg-bg-raised px-2 py-1.5 text-[12.5px] leading-[18px] text-fg shadow-[inset_0_0_0_1px_var(--line-2)] outline-none transition-[box-shadow] duration-micro selection:bg-[var(--sel)] placeholder:text-fg-3 focus-visible:shadow-[inset_0_0_0_1px_var(--accent)] disabled:cursor-not-allowed disabled:opacity-45",
        "aria-invalid:shadow-[inset_0_0_0_1px_var(--danger)]",
        className,
      )}
      {...props}
    />
  );
}

export { Textarea };
