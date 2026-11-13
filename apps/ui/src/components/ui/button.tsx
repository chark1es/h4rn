import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-sm text-compact font-medium transition-colors duration-[var(--dur-instant)] ease-[var(--ease-out)] outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-primary/90",
        secondary:
          "border border-control-border bg-raised text-ink hover:bg-panel hover:[box-shadow:inset_0_0_0_1px_var(--ink)]",
        ghost: "text-ink-muted hover:bg-panel hover:text-ink",
        destructive: "text-strike-ink hover:bg-strike-tint",
        link: "text-prompt underline-offset-4 hover:underline",
      },
      size: {
        default: "h-11 px-3 md:h-[30px]",
        sm: "h-9 px-2.5 text-micro md:h-[26px]",
        lg: "h-11 px-4 md:h-[34px]",
        icon: "size-11 md:size-[30px]",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

function Button({
  className,
  variant,
  size,
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean;
  }) {
  const Comp = asChild ? Slot : "button";

  return (
    <Comp
      data-slot="button"
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  );
}

export { Button, buttonVariants };
