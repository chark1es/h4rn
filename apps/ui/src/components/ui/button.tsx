import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-[6px] text-[13px] font-medium transition-[background,color,box-shadow] duration-micro ease-enter outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent-soft)] disabled:pointer-events-none disabled:opacity-45 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4",
  {
    variants: {
      variant: {
        default: "bg-[var(--accent)] text-[var(--accent-fg)] hover:bg-[var(--accent-hi)]",
        secondary:
          "bg-[var(--bg-raised)] text-fg shadow-[inset_0_0_0_1px_var(--line-2),0_1px_0_oklch(0.2_0.01_260/0.04)] hover:bg-[var(--hover)]",
        ghost: "text-[var(--fg-2)] hover:bg-[var(--hover)] hover:text-fg",
        destructive:
          "bg-[var(--bg-raised)] text-[var(--danger)] shadow-[inset_0_0_0_1px_var(--line-2)] hover:bg-[var(--danger-soft)]",
        link: "text-[var(--accent-tx)] underline-offset-4 hover:underline",
      },
      size: {
        default: "h-7 px-2.5",
        sm: "h-6 px-2 text-[12px]",
        lg: "h-8 px-3",
        icon: "size-7",
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
