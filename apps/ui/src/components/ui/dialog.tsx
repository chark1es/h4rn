import {
  DialogBackdrop as DialogBackdropPrimitive,
  DialogClose as DialogClosePrimitive,
  DialogContent as DialogContentPrimitive,
  DialogDescription as DialogDescriptionPrimitive,
  Dialog as DialogPrimitive,
  type DialogProps as DialogPrimitiveProps,
  DialogTitle as DialogTitlePrimitive,
  DialogTrigger as DialogTriggerPrimitive,
} from "@ui-sdk/react/dialog";
import { cva, type VariantProps } from "class-variance-authority";
import { XIcon } from "lucide-react";
import * as React from "react";
import { useTranslation } from "react-i18next";

import { useIsPhone } from "@/hooks/use-media-query";
import { skipReducedSheetMotion } from "@/lib/sheet-motion";
import { cn } from "@/lib/utils";

const DialogSheetContext = React.createContext(false);
const DialogCloseButtonContext = React.createContext(true);

/** True inside a dialog that presents as a phone bottom sheet. */
function useDialogSheet(): boolean {
  return React.useContext(DialogSheetContext);
}

type DialogProps = DialogPrimitiveProps;

/** A modal task. On the phone layout it presents as a bottom sheet. */
function Dialog({ presentation, ...props }: DialogProps) {
  const phone = useIsPhone();
  const sheet = presentation === "sheet" || (presentation !== "dialog" && phone);
  return (
    <DialogSheetContext value={sheet}>
      <DialogPrimitive presentation={sheet ? "sheet" : "dialog"} {...props} />
    </DialogSheetContext>
  );
}

const DialogTrigger = DialogTriggerPrimitive;

const backdropClassName = cn(
  "fixed inset-0 bg-scrim",
  "transition-opacity duration-modal ease-enter data-starting-style:opacity-0",
  "data-ending-style:opacity-0 data-ending-style:duration-modal-exit data-ending-style:ease-exit",
  "motion-reduce:transition-none",
);

/** The modal surface both dialogs and alert dialogs share. */
const modalSurfaceClassName = cn(
  "flex flex-col overflow-hidden bg-popover text-sm text-popover-foreground outline-none",
  "border border-border shadow-modal",
);

const dialogContentVariants = cva(
  [
    modalSurfaceClassName,
    "fixed inset-0 m-auto h-fit max-h-[calc(100dvh-4rem)] w-[calc(100%-2rem)] rounded-xl",
    "transition-[opacity,translate] duration-modal ease-enter",
    "data-starting-style:translate-y-2 data-starting-style:opacity-0",
    "data-ending-style:translate-y-1 data-ending-style:opacity-0 data-ending-style:duration-modal-exit data-ending-style:ease-exit",
    "motion-reduce:transition-opacity",
  ],
  {
    variants: {
      size: {
        sm: "max-w-100",
        md: "max-w-130",
        lg: "max-w-160",
        xl: "max-w-200",
      },
    },
    defaultVariants: { size: "md" },
  },
);

const sheetContentClassName = cn(
  modalSurfaceClassName,
  "max-h-[92dvh] w-full rounded-t-xl border-x-0 border-b-0 pb-safe [--sheet-backdrop:var(--scrim)]",
);

type DialogContentProps = Omit<React.ComponentProps<typeof DialogContentPrimitive>, "className"> &
  VariantProps<typeof dialogContentVariants> & {
    className?: string;
    showCloseButton?: boolean;
  };

const FIRST_FIELD = [
  "[data-autofocus]",
  '[data-slot="dialog-body"] :is(input, textarea, select, [contenteditable="true"]):not(:disabled, [type="hidden"], [tabindex="-1"])',
  '[data-slot="dialog-footer"] button:not(:disabled)',
];

function DialogContent({
  className,
  size,
  showCloseButton = true,
  initialFocus,
  children,
  ref,
  ...props
}: DialogContentProps) {
  const sheet = useDialogSheet();
  const content = React.useRef<HTMLDivElement>(null);
  const contentRef = React.useCallback(
    (node: HTMLDivElement | null) => {
      content.current = node;
      if (typeof ref === "function") ref(node);
      else if (ref) ref.current = node;
      // React 19 calls a ref's returned cleanup instead of calling it with null.
      const stop = skipReducedSheetMotion(node);
      return () => {
        content.current = null;
        if (typeof ref === "function") ref(null);
        else if (ref) ref.current = null;
        stop?.();
      };
    },
    [ref],
  );
  // The close button comes first in the header, so the default "first focusable"
  // would land there; start on the task instead, as React's autoFocus intends.
  const firstField = React.useCallback(() => {
    const node = content.current;
    if (!node) return null;
    for (const selector of FIRST_FIELD) {
      const found = node.querySelector<HTMLElement>(selector);
      if (found) return found;
    }
    return node;
  }, []);
  return (
    <>
      {sheet ? null : <DialogBackdropPrimitive className={backdropClassName} />}
      <DialogContentPrimitive
        ref={contentRef}
        initialFocus={initialFocus ?? firstField}
        className={cn(sheet ? sheetContentClassName : dialogContentVariants({ size }), className)}
        data-size={size ?? "md"}
        {...props}
      >
        {sheet ? <SheetGrabber /> : null}
        <DialogCloseButtonContext value={showCloseButton}>
          {children as React.ReactNode}
        </DialogCloseButtonContext>
      </DialogContentPrimitive>
    </>
  );
}

/** The sheet's grab handle. Swiping is the sheet's own; this only shows it can move. */
function SheetGrabber() {
  return (
    <div data-slot="sheet-grabber" aria-hidden="true" className="flex shrink-0 justify-center pt-2">
      <div className="h-1 w-9 rounded-full bg-fill-2" />
    </div>
  );
}

function DialogCloseIcon({ className }: { className?: string }) {
  const { t } = useTranslation("common");
  return (
    <DialogClosePrimitive
      aria-label={t("close")}
      className={cn(
        "hit-area relative -mt-1 -mr-1.5 inline-flex size-8 shrink-0 cursor-default items-center justify-center rounded-md text-muted-foreground",
        "transition-colors duration-micro ease-enter hover:bg-accent hover:text-foreground",
        "outline-none focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-2 focus-visible:outline-ring",
        className,
      )}
    >
      <XIcon className="size-4" aria-hidden="true" />
    </DialogClosePrimitive>
  );
}

/** Title row: the title and description on the left, the close button on the title's line. */
function DialogHeader({ className, children, ...props }: React.ComponentProps<"div">) {
  const showClose = React.useContext(DialogCloseButtonContext);
  const sheet = useDialogSheet();
  return (
    <div
      data-slot="dialog-header"
      className={cn(
        "flex shrink-0 items-start gap-3 px-5 pt-5 pb-3",
        sheet && "px-4 pt-3",
        className,
      )}
      {...props}
    >
      <div className="flex min-w-0 flex-1 flex-col gap-1">{children}</div>
      {showClose ? <DialogCloseIcon /> : null}
    </div>
  );
}

/** The part between header and footer; it scrolls when the dialog is taller than the window. */
function DialogBody({ className, ...props }: React.ComponentProps<"div">) {
  const sheet = useDialogSheet();
  return (
    <div
      data-slot="dialog-body"
      className={cn(
        "flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto overscroll-contain px-5 pt-1 pb-5",
        sheet && "px-4 pb-4",
        className,
      )}
      {...props}
    />
  );
}

/** Actions, secondary before primary so the primary sits rightmost. Full-width 44 px on a sheet. */
function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  const sheet = useDialogSheet();
  return (
    <div
      data-slot="dialog-footer"
      className={cn(
        "flex shrink-0 items-center justify-end gap-2 border-t border-border px-5 py-3",
        sheet && "gap-3 px-4 [&>button]:h-11 [&>button]:flex-1 [&>button]:justify-center",
        className,
      )}
      {...props}
    />
  );
}

function DialogTitle({
  className,
  ...props
}: Omit<React.ComponentProps<typeof DialogTitlePrimitive>, "className"> & { className?: string }) {
  return (
    <DialogTitlePrimitive
      className={cn("m-0 text-lg font-semibold text-foreground", className)}
      {...props}
    />
  );
}

function DialogDescription({
  className,
  ...props
}: Omit<React.ComponentProps<typeof DialogDescriptionPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <DialogDescriptionPrimitive
      className={cn("m-0 text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}

const DialogClose = DialogClosePrimitive;

export {
  backdropClassName,
  Dialog,
  DialogBody,
  DialogClose,
  DialogContent,
  type DialogContentProps,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  type DialogProps,
  DialogTitle,
  DialogTrigger,
  modalSurfaceClassName,
  SheetGrabber,
  useDialogSheet,
};
