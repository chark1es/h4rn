import { Textarea as TextareaPrimitive } from "@ui-sdk/react/textarea";
import type * as React from "react";

import { cn } from "@/lib/utils";

import { fieldClasses } from "./input";

type TextareaProps = Omit<React.ComponentProps<typeof TextareaPrimitive>, "className"> & {
  className?: string;
};

function Textarea({ className, ...props }: TextareaProps) {
  return (
    <TextareaPrimitive
      className={cn(fieldClasses, "flex min-h-20 px-2.5 py-1.5 phone:px-3 phone:py-2.5", className)}
      {...props}
    />
  );
}

export { Textarea, type TextareaProps };
