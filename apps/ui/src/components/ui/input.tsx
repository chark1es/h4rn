import { Input as InputPrimitive } from "@ui-sdk/react/input";
import type * as React from "react";

import { cn } from "@/lib/utils";

const fieldClasses = [
  "w-full min-w-0 rounded-md border border-input bg-card text-sm text-foreground",
  "transition-[border-color,box-shadow] duration-micro ease-enter",
  "outline-none selection:bg-sel placeholder:text-fg-3",
  "focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/25",
  "aria-invalid:border-destructive aria-invalid:focus-visible:ring-destructive/20",
  "data-disabled:cursor-not-allowed data-disabled:opacity-45 data-readonly:bg-muted",
  "phone:text-lg",
];

type InputProps = Omit<React.ComponentProps<typeof InputPrimitive>, "className"> & {
  className?: string;
};

function Input({ className, ...props }: InputProps) {
  return (
    <InputPrimitive
      className={cn(fieldClasses, "h-7 px-2.5 phone:h-11 phone:px-3", className)}
      {...props}
    />
  );
}

export { fieldClasses, Input, type InputProps };
