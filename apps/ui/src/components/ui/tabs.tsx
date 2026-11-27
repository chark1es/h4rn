import {
  TabsIndicator,
  TabsList as TabsListPrimitive,
  TabsPanel as TabsPanelPrimitive,
  Tabs as TabsPrimitive,
  TabsTab as TabsTabPrimitive,
} from "@ui-sdk/react/tabs";
import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

type ClassName = { className?: string };

function Tabs({
  className,
  ...props
}: Omit<React.ComponentProps<typeof TabsPrimitive>, "className"> & ClassName) {
  return <TabsPrimitive className={cn("flex min-h-0 flex-col", className)} {...props} />;
}

const tabsListVariants = cva("group/tabs relative isolate flex min-w-0 shrink-0 items-stretch", {
  variants: {
    variant: {
      underline: "h-9 gap-4",
      segmented: "inline-flex h-7 w-fit rounded-md bg-muted p-0.5 phone:h-9",
    },
  },
  defaultVariants: { variant: "underline" },
});

const tabsIndicatorVariants = cva(
  [
    "pointer-events-none absolute left-(--active-tab-left) w-(--active-tab-width)",
    "transition-[left,width] duration-popup ease-enter motion-reduce:transition-none data-[activation-direction=none]:transition-none",
  ],
  {
    variants: {
      variant: {
        underline: "bottom-0 h-0.5 rounded-full bg-foreground",
        segmented:
          "top-(--active-tab-top) -z-10 h-(--active-tab-height) rounded-sm bg-card shadow-[inset_0_0_0_1px_var(--input)]",
      },
    },
    defaultVariants: { variant: "underline" },
  },
);

type TabsListProps = Omit<React.ComponentProps<typeof TabsListPrimitive>, "className" | "children"> &
  VariantProps<typeof tabsListVariants> &
  ClassName & { children?: React.ReactNode };

function TabsList({ className, variant, children, ...props }: TabsListProps) {
  return (
    <TabsListPrimitive
      data-variant={variant ?? "underline"}
      className={cn(tabsListVariants({ variant }), className)}
      {...props}
    >
      {children}
      <TabsIndicator className={tabsIndicatorVariants({ variant })} />
    </TabsListPrimitive>
  );
}

type TabsTabProps = Omit<React.ComponentProps<typeof TabsTabPrimitive>, "className"> & ClassName;

function TabsTab({ className, ...props }: TabsTabProps) {
  return (
    <TabsTabPrimitive
      className={cn(
        "relative inline-flex min-w-0 shrink-0 cursor-default items-center justify-center gap-1.5 text-sm font-medium whitespace-nowrap text-muted-foreground select-none",
        "transition-colors duration-micro ease-enter hover:text-foreground data-selected:text-foreground",
        "outline-none focus-visible:outline-2 focus-visible:outline-ring",
        "data-disabled:pointer-events-none data-disabled:opacity-45",
        "group-data-[variant=underline]/tabs:focus-visible:-outline-offset-2 group-data-[variant=underline]/tabs:rounded-sm",
        "group-data-[variant=segmented]/tabs:rounded-sm group-data-[variant=segmented]/tabs:px-2.5 group-data-[variant=segmented]/tabs:focus-visible:outline-offset-1 phone:group-data-[variant=segmented]/tabs:px-3",
        "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-3.5",
        className,
      )}
      {...props}
    />
  );
}

function TabsCount({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      data-slot="tabs-count"
      className={cn("shrink-0 font-normal text-fg-3 tabular-nums", className)}
      {...props}
    />
  );
}

type TabsPanelProps = Omit<React.ComponentProps<typeof TabsPanelPrimitive>, "className"> &
  ClassName;

function TabsPanel({ className, ...props }: TabsPanelProps) {
  return (
    <TabsPanelPrimitive
      className={cn("flex min-h-0 flex-1 flex-col outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring", className)}
      {...props}
    />
  );
}

export { Tabs, TabsCount, TabsList, type TabsListProps, TabsPanel, TabsTab, tabsListVariants };
