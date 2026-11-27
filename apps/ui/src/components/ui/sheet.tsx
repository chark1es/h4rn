import {
  DrawerBackdrop as DrawerBackdropPrimitive,
  DrawerClose as DrawerClosePrimitive,
  DrawerContent as DrawerContentPrimitive,
  DrawerDescription as DrawerDescriptionPrimitive,
  Drawer as DrawerPrimitive,
  DrawerTitle as DrawerTitlePrimitive,
  DrawerTrigger as DrawerTriggerPrimitive,
} from "@ui-sdk/react/drawer";
import type * as React from "react";

import { backdropClassName, modalSurfaceClassName, SheetGrabber } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

/** A bottom sheet for phone-only surfaces. Dialogs become sheets on their own. */
const Sheet = DrawerPrimitive;
const SheetTrigger = DrawerTriggerPrimitive;
const SheetClose = DrawerClosePrimitive;

type SheetContentProps = Omit<React.ComponentProps<typeof DrawerContentPrimitive>, "className"> & {
  className?: string;
  size?: "auto" | "large";
};

function SheetContent({ className, size = "auto", children, ...props }: SheetContentProps) {
  return (
    <>
      <DrawerBackdropPrimitive className={backdropClassName} />
      <DrawerContentPrimitive
        data-size={size}
        className={cn(
          modalSurfaceClassName,
          "w-full rounded-t-xl border-x-0 border-b-0 pb-safe",
          size === "large" ? "h-[calc(100dvh-env(safe-area-inset-top)-0.5rem)]" : "max-h-[92dvh]",
          className,
        )}
        {...props}
      >
        <SheetGrabber />
        {children as React.ReactNode}
      </DrawerContentPrimitive>
    </>
  );
}

function SheetHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="sheet-header"
      className={cn("flex shrink-0 items-start gap-3 px-4 pt-3 pb-3", className)}
      {...props}
    />
  );
}

function SheetBody({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="sheet-body"
      className={cn(
        "flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto overscroll-contain px-4 pb-4",
        className,
      )}
      {...props}
    />
  );
}

function SheetFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="sheet-footer"
      className={cn(
        "flex shrink-0 items-center gap-3 px-4 pt-3 [&>button]:h-11 [&>button]:flex-1 [&>button]:justify-center",
        className,
      )}
      {...props}
    />
  );
}

function SheetTitle({
  className,
  ...props
}: Omit<React.ComponentProps<typeof DrawerTitlePrimitive>, "className"> & { className?: string }) {
  return (
    <DrawerTitlePrimitive
      className={cn("m-0 text-lg font-semibold text-foreground", className)}
      {...props}
    />
  );
}

function SheetDescription({
  className,
  ...props
}: Omit<React.ComponentProps<typeof DrawerDescriptionPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <DrawerDescriptionPrimitive
      className={cn("m-0 text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}

export {
  Sheet,
  SheetBody,
  SheetClose,
  SheetContent,
  type SheetContentProps,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
};
