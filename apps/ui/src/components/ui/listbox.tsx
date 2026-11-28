import {
  ListboxItemIndicator,
  ListboxItem as ListboxItemPrimitive,
  Listbox as ListboxPrimitive,
  type ListboxProps,
} from "@ui-sdk/react/listbox";
import { CheckIcon } from "lucide-react";
import type * as React from "react";

import { itemRow } from "@/components/ui/menu";
import { cn } from "@/lib/utils";

function Listbox<T = string>({ className, ...props }: ListboxProps<T, false>) {
  return (
    <ListboxPrimitive<T, false>
      className={cn("flex flex-col outline-none", className as string | undefined)}
      {...props}
    />
  );
}

type ListboxItemProps<T> = Omit<
  React.ComponentProps<typeof ListboxItemPrimitive<T>>,
  "className" | "children" | "disabled"
> & {
  className?: string | undefined;
  children?: React.ReactNode;
  disabled?: boolean | undefined;
  description?: React.ReactNode;
};

/** A row with the check in a reserved leading column, like a select's. */
function ListboxItem<T = string>({
  className,
  description,
  disabled,
  children,
  ...props
}: ListboxItemProps<T>) {
  return (
    <ListboxItemPrimitive<T>
      disabled={disabled ?? false}
      data-has-description={description ? "" : undefined}
      className={cn(itemRow, "hover:bg-accent", className)}
      {...props}
    >
      <span className="inline-flex size-3.5 shrink-0 items-center justify-center text-primary">
        <ListboxItemIndicator keepMounted className="inline-flex not-data-selected:invisible">
          <CheckIcon strokeWidth={2} aria-hidden="true" />
        </ListboxItemIndicator>
      </span>
      {description ? (
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate">{children}</span>
          <span className="truncate text-xs text-muted-foreground">{description}</span>
        </span>
      ) : (
        <span className="min-w-0 flex-1 truncate">{children}</span>
      )}
    </ListboxItemPrimitive>
  );
}

export { Listbox, ListboxItem };
