import {
  SelectContent as SelectContentPrimitive,
  SelectGroup,
  SelectGroupLabel as SelectGroupLabelPrimitive,
  SelectItemIcon as SelectItemIconPrimitive,
  SelectItemIndicator,
  SelectItem as SelectItemPrimitive,
  Select as SelectPrimitive,
  type SelectProps,
  SelectSeparator as SelectSeparatorPrimitive,
  SelectTrigger as SelectTriggerPrimitive,
  SelectValue,
  SelectValueIcon,
} from "@ui-sdk/react/select";
import { cva, type VariantProps } from "class-variance-authority";
import { CheckIcon, ChevronDownIcon } from "lucide-react";
import type * as React from "react";

import { itemRow, popupSurface, sheetSurface } from "@/components/ui/menu";
import { cn } from "@/lib/utils";

type WithClassName<P> = Omit<P, "className" | "children" | "disabled"> & {
  className?: string | undefined;
  children?: React.ReactNode;
  disabled?: boolean | undefined;
};

function Select<T = string>({ presentation = "auto", ...props }: SelectProps<T, false>) {
  return <SelectPrimitive<T, false> presentation={presentation} {...props} />;
}

const selectTriggerVariants = cva(
  [
    "inline-flex min-w-0 cursor-default items-center gap-2 rounded-md border border-input bg-card text-start text-foreground outline-none",
    "transition-[border-color,box-shadow,background-color] duration-micro ease-enter",
    "hover:bg-hover focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/25",
    "data-open:border-ring data-placeholder:text-fg-3",
    "data-disabled:cursor-not-allowed data-disabled:opacity-45",
    "aria-invalid:border-destructive",
    "[&_svg]:pointer-events-none [&_svg]:shrink-0",
  ],
  {
    variants: {
      size: {
        default: "h-7 w-full px-2.5 text-sm phone:h-11 phone:px-3 phone:text-lg",
        chip: "h-6 max-w-56 gap-1.5 px-1.5 text-xs font-medium phone:h-11 phone:px-3 phone:text-base",
      },
    },
    defaultVariants: { size: "default" },
  },
);

type SelectTriggerProps = WithClassName<React.ComponentProps<typeof SelectTriggerPrimitive>> &
  VariantProps<typeof selectTriggerVariants> & {
    placeholder?: string | undefined;
    /** Shows the selected item's icon before its label. */
    withIcon?: boolean | undefined;
  };

function SelectTrigger({
  className,
  size,
  placeholder,
  withIcon,
  disabled,
  children,
  ...props
}: SelectTriggerProps) {
  return (
    <SelectTriggerPrimitive
      data-slot="select-trigger"
      disabled={disabled ?? false}
      className={cn(selectTriggerVariants({ size }), className)}
      {...props}
    >
      {children ?? (
        <>
          {withIcon ? (
            <SelectValueIcon className="inline-flex size-3.5 items-center justify-center text-muted-foreground [&_svg]:size-3.5" />
          ) : null}
          <SelectValue
            className="min-w-0 flex-1 truncate"
            {...(placeholder ? { placeholder } : {})}
          />
        </>
      )}
      <ChevronDownIcon className="size-3.5 text-muted-foreground" aria-hidden="true" />
    </SelectTriggerPrimitive>
  );
}

function SelectContent({
  className,
  sideOffset = 4,
  collisionPadding = 8,
  ...props
}: WithClassName<React.ComponentProps<typeof SelectContentPrimitive>>) {
  return (
    <SelectContentPrimitive
      sideOffset={sideOffset}
      collisionPadding={collisionPadding}
      className={cn(popupSurface, "min-w-(--anchor-width)", sheetSurface, className)}
      {...props}
    />
  );
}

type SelectItemProps<T> = WithClassName<React.ComponentProps<typeof SelectItemPrimitive<T>>> & {
  icon?: React.ReactNode;
  /** A muted second line, like a path or a hint. */
  description?: React.ReactNode;
};

/** A row with the check in a reserved leading column, so labels line up. */
function SelectItem<T = string>({
  className,
  icon,
  description,
  disabled,
  children,
  ...props
}: SelectItemProps<T>) {
  return (
    <SelectItemPrimitive<T>
      disabled={disabled ?? false}
      data-has-description={description ? "" : undefined}
      className={cn(itemRow, "ps-2", className)}
      {...props}
    >
      <span className="inline-flex size-3.5 shrink-0 items-center justify-center">
        <SelectItemIndicator keepMounted className="inline-flex not-data-selected:invisible">
          <CheckIcon strokeWidth={2} aria-hidden="true" />
        </SelectItemIndicator>
      </span>
      {icon ? (
        <SelectItemIconPrimitive className="inline-flex size-3.5 shrink-0 items-center justify-center text-muted-foreground in-data-highlighted:text-current">
          {icon}
        </SelectItemIconPrimitive>
      ) : null}
      {description ? (
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate">{children}</span>
          <span className="truncate text-xs text-muted-foreground in-data-highlighted:text-current/70">
            {description}
          </span>
        </span>
      ) : (
        <span className="min-w-0 flex-1 truncate">{children}</span>
      )}
    </SelectItemPrimitive>
  );
}

function SelectGroupLabel({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof SelectGroupLabelPrimitive>>) {
  return (
    <SelectGroupLabelPrimitive
      className={cn(
        "flex h-6 min-w-0 items-center truncate px-2 text-xs font-medium text-muted-foreground phone:h-8",
        className,
      )}
      {...props}
    />
  );
}

function SelectSeparator({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof SelectSeparatorPrimitive>>) {
  return (
    <SelectSeparatorPrimitive className={cn("-mx-1 my-1 h-px bg-border", className)} {...props} />
  );
}

export {
  Select,
  SelectContent,
  SelectGroup,
  SelectGroupLabel,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
  selectTriggerVariants,
};
