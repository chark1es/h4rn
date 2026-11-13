import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex w-fit shrink-0 items-center gap-1 rounded-sm px-1.5 py-0.5 text-micro font-medium whitespace-nowrap [&>svg]:size-3",
  {
    variants: {
      variant: {
        default: "bg-panel text-ink-muted",
        cue: "bg-cue-tint text-cue-ink",
        standby: "bg-standby-tint text-standby-ink",
        strike: "bg-strike-tint text-strike-ink",
        prompt: "bg-prompt-tint text-prompt",
        outline: "border border-line text-ink-muted",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

function Badge({
  className,
  variant,
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return (
    <span data-slot="badge" className={cn(badgeVariants({ variant }), className)} {...props} />
  );
}

export { Badge, badgeVariants };
