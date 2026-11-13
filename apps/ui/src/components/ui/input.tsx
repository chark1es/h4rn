import type * as React from "react";

import { cn } from "@/lib/utils";

function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-slot="input"
      className={cn(
        "h-7 w-full min-w-0 rounded-[6px] border-0 bg-raised px-2 text-[12.5px] text-ink shadow-[inset_0_0_0_1px_var(--line-2)] outline-none transition-[box-shadow] duration-[var(--dur-1)] selection:bg-[var(--sel)] placeholder:text-ink-faint focus-visible:shadow-[inset_0_0_0_1px_var(--accent)] disabled:pointer-events-none disabled:opacity-45",
        "aria-invalid:shadow-[inset_0_0_0_1px_var(--danger)]",
        className,
      )}
      {...props}
    />
  );
}

export { Input };
