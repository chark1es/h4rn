import { CheckboxIndicator, Checkbox as CheckboxPrimitive } from "@ui-sdk/react/checkbox";
import { CheckIcon, MinusIcon } from "lucide-react";
import type * as React from "react";

import { cn } from "@/lib/utils";

type CheckboxProps = Omit<
  React.ComponentProps<typeof CheckboxPrimitive>,
  "className" | "children"
> & { className?: string };

function Checkbox({ className, ...props }: CheckboxProps) {
  return (
    <CheckboxPrimitive
      className={cn(
        "hit-area peer relative inline-grid size-4 shrink-0 cursor-default place-items-center rounded-sm bg-card text-primary-foreground shadow-[inset_0_0_0_1px_var(--input)]",
        "transition-[background-color,box-shadow] duration-micro ease-enter",
        "data-checked:bg-primary data-checked:shadow-none data-indeterminate:bg-primary data-indeterminate:shadow-none",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        "data-disabled:opacity-45",
        className,
      )}
      {...props}
    >
      <CheckboxIndicator className="pointer-events-none grid place-items-center [&_svg]:size-3">
        {(state: { indeterminate?: boolean }) =>
          state.indeterminate ? (
            <MinusIcon strokeWidth={2.5} aria-hidden />
          ) : (
            <CheckIcon strokeWidth={2.5} aria-hidden />
          )
        }
      </CheckboxIndicator>
    </CheckboxPrimitive>
  );
}

export { Checkbox, type CheckboxProps };
