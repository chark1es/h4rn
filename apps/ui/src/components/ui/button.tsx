import { Button as ButtonPrimitive } from "@ui-sdk/react/button";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const buttonVariants = cva(
  [
    "hit-area relative inline-flex shrink-0 cursor-default items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium select-none",
    "transition-[background-color,color,box-shadow,opacity] duration-micro ease-enter",
    "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
    "data-disabled:pointer-events-none data-disabled:opacity-45 data-pending:cursor-progress",
    "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
  ],
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-accent-hi active:bg-accent-hi",
        secondary: "border border-line-2 bg-card text-foreground hover:bg-hover active:bg-fill",
        outline:
          "border border-line-2 bg-transparent text-foreground hover:bg-accent active:bg-fill-2",
        ghost: "text-muted-foreground hover:bg-accent hover:text-foreground active:bg-fill-2",
        destructive:
          "border border-line-2 bg-card text-destructive hover:bg-danger-soft active:bg-danger-soft",
        link: "text-accent-tx underline-offset-4 hover:underline",
      },
      size: {
        sm: "h-6 gap-1.5 px-2 text-xs [&_svg:not([class*='size-'])]:size-3.5",
        default: "h-7 px-2.5 phone:h-11 phone:rounded-xl phone:px-4.5 phone:text-lg",
        lg: "h-8 px-3 phone:h-11 phone:rounded-xl phone:px-4.5 phone:text-lg",
        icon: "size-7",
        "icon-sm": "size-6 [&_svg:not([class*='size-'])]:size-3.5",
      },
    },
    compoundVariants: [{ variant: "link", className: "h-auto px-0 phone:h-auto phone:px-0" }],
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

type ButtonProps = Omit<React.ComponentProps<typeof ButtonPrimitive>, "className"> &
  VariantProps<typeof buttonVariants> & { className?: string };

function Button({ className, variant, size, ...props }: ButtonProps) {
  return (
    <ButtonPrimitive
      data-variant={variant ?? "default"}
      data-size={size ?? "default"}
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  );
}

export { Button, type ButtonProps, buttonVariants };
