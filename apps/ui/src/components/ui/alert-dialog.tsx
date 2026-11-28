import {
  AlertDialogBackdrop as AlertDialogBackdropPrimitive,
  AlertDialogClose as AlertDialogClosePrimitive,
  AlertDialogContent as AlertDialogContentPrimitive,
  AlertDialogDescription as AlertDialogDescriptionPrimitive,
  AlertDialog as AlertDialogPrimitive,
  AlertDialogTitle as AlertDialogTitlePrimitive,
  AlertDialogTrigger as AlertDialogTriggerPrimitive,
} from "@ui-sdk/react/alert-dialog";
import type * as React from "react";

import { backdropClassName, modalSurfaceClassName } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

const AlertDialog = AlertDialogPrimitive;
const AlertDialogTrigger = AlertDialogTriggerPrimitive;
const AlertDialogClose = AlertDialogClosePrimitive;

/** A question that needs an answer. Stays centred on the phone, with full-width answers. */
function AlertDialogContent({
  className,
  children,
  ...props
}: Omit<React.ComponentProps<typeof AlertDialogContentPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <>
      <AlertDialogBackdropPrimitive className={backdropClassName} />
      <AlertDialogContentPrimitive
        className={cn(
          modalSurfaceClassName,
          "fixed inset-0 m-auto h-fit max-h-[calc(100dvh-4rem)] w-[calc(100%-2rem)] max-w-100 rounded-xl",
          "transition-[opacity,scale] duration-modal ease-enter",
          "data-starting-style:scale-96 data-starting-style:opacity-0",
          "data-ending-style:scale-98 data-ending-style:opacity-0 data-ending-style:duration-modal-exit data-ending-style:ease-exit",
          "motion-reduce:transition-opacity",
          className,
        )}
        {...props}
      >
        {children as React.ReactNode}
      </AlertDialogContentPrimitive>
    </>
  );
}

function AlertDialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-dialog-header"
      className={cn("flex flex-col gap-1 px-5 pt-5 pb-4", className)}
      {...props}
    />
  );
}

function AlertDialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-dialog-footer"
      className={cn(
        "flex items-center justify-end gap-2 border-t border-border px-5 py-3",
        "phone:gap-3 phone:px-4 phone:[&>button]:h-11 phone:[&>button]:flex-1",
        className,
      )}
      {...props}
    />
  );
}

function AlertDialogTitle({
  className,
  ...props
}: Omit<React.ComponentProps<typeof AlertDialogTitlePrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <AlertDialogTitlePrimitive
      className={cn("m-0 text-lg font-semibold text-foreground", className)}
      {...props}
    />
  );
}

function AlertDialogDescription({
  className,
  ...props
}: Omit<React.ComponentProps<typeof AlertDialogDescriptionPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <AlertDialogDescriptionPrimitive
      className={cn("m-0 text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}

export {
  AlertDialog,
  AlertDialogClose,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
};
