import {
  CommandEmpty as CommandEmptyPrimitive,
  CommandGroupLabel,
  CommandGroup as CommandGroupPrimitive,
  CommandInput as CommandInputPrimitive,
  CommandItemIcon,
  CommandItem as CommandItemPrimitive,
  CommandList as CommandListPrimitive,
  Command as CommandPrimitive,
  CommandSeparator as CommandSeparatorPrimitive,
} from "@ui-sdk/react/command";
import {
  DialogBackdrop,
  DialogContent,
  DialogDescription,
  Dialog as DialogPrimitive,
  DialogTitle,
} from "@ui-sdk/react/dialog";
import { SearchIcon } from "lucide-react";
import type * as React from "react";

import { ShortcutCaps } from "@/components/ui/kbd";
import {
  ariaKeyShortcut,
  bindingFor,
  type CommandId,
  useKeybindingsVersion,
} from "@/lib/keybindings";
import { cn } from "@/lib/utils";

type WithClassName<P> = Omit<P, "className" | "children" | "disabled"> & {
  className?: string | undefined;
  children?: React.ReactNode;
  disabled?: boolean | undefined;
};

function Command({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandPrimitive>>) {
  return (
    <CommandPrimitive
      className={cn("flex min-h-0 flex-1 flex-col text-foreground", className)}
      {...props}
    />
  );
}

/**
 * A palette in a modal: centred high in the window on desktop, the whole
 * screen on a phone. The title and description are for screen readers.
 */
function CommandDialog({
  open,
  onOpenChange,
  title,
  description,
  className,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: string;
  className?: string | undefined;
  children: React.ReactNode;
}) {
  return (
    <DialogPrimitive open={open} onOpenChange={(next) => onOpenChange(next)}>
      <DialogBackdrop
        className={cn(
          "fixed inset-0 bg-scrim transition-opacity duration-modal ease-enter",
          "data-starting-style:opacity-0 data-ending-style:opacity-0 data-ending-style:duration-modal-exit data-ending-style:ease-exit",
        )}
      />
      <DialogContent
        className={cn(
          "fixed inset-x-0 top-[clamp(--spacing(4),12vh,--spacing(24))] mx-auto flex max-h-[calc(100dvh-clamp(--spacing(4),12vh,--spacing(24))-var(--spacing)*4)] w-[min(--spacing(155),calc(100vw-var(--spacing)*8))] flex-col overflow-hidden rounded-xl border border-border bg-popover p-0 text-popover-foreground shadow-modal outline-none",
          "transition-[opacity,translate] duration-modal ease-enter data-starting-style:-translate-y-1 data-starting-style:opacity-0",
          "data-ending-style:opacity-0 data-ending-style:duration-modal-exit data-ending-style:ease-exit",
          "motion-reduce:transition-opacity",
          "phone:inset-0 phone:top-0 phone:h-full phone:max-h-none phone:w-full phone:rounded-none phone:border-0",
          className,
        )}
      >
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <DialogDescription className="sr-only">{description}</DialogDescription>
        {children}
      </DialogContent>
    </DialogPrimitive>
  );
}

function CommandInput({
  className,
  trailing,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandInputPrimitive>> & {
  trailing?: React.ReactNode;
}) {
  return (
    <div
      data-slot="command-input-wrapper"
      className="flex h-13 shrink-0 items-center gap-2.5 border-b border-border px-4"
    >
      <SearchIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
      <CommandInputPrimitive
        className={cn(
          "h-full min-w-0 flex-1 border-0 bg-transparent text-base text-foreground outline-none placeholder:text-fg-3 phone:text-lg",
          className,
        )}
        {...props}
      />
      {trailing}
    </div>
  );
}

function CommandList({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandListPrimitive>>) {
  return (
    <CommandListPrimitive
      className={cn(
        "min-h-0 flex-1 overflow-y-auto overscroll-contain p-1.5 outline-none",
        // Whole 32 px rows: ten rows and three headings fill the resting list exactly.
        "max-h-[calc(var(--spacing)*8*13+var(--spacing)*3)] phone:max-h-none",
        className,
      )}
      {...props}
    />
  );
}

function CommandEmpty({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandEmptyPrimitive>>) {
  return (
    <CommandEmptyPrimitive
      className={cn(
        "hidden py-8 text-center text-sm text-muted-foreground data-empty:block",
        className,
      )}
      {...props}
    />
  );
}

function CommandGroup({
  heading,
  className,
  children,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandGroupPrimitive>> & {
  heading: React.ReactNode;
}) {
  return (
    <CommandGroupPrimitive className={cn(className)} {...props}>
      <CommandGroupLabel className="flex h-8 items-center px-2.5 text-xs font-medium text-muted-foreground">
        {heading}
      </CommandGroupLabel>
      {children}
    </CommandGroupPrimitive>
  );
}

function CommandSeparator({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof CommandSeparatorPrimitive>>) {
  return (
    <CommandSeparatorPrimitive
      className={cn("-mx-1.5 my-1 h-px bg-border", className)}
      {...props}
    />
  );
}

type CommandItemProps = WithClassName<React.ComponentProps<typeof CommandItemPrimitive>> & {
  icon?: React.ReactNode;
  /** Muted metadata before the shortcut, like a thread's host. */
  trailing?: React.ReactNode;
  /** Shows this command's current binding and announces it. */
  command?: CommandId | undefined;
};

/** A 32 px row: a 14 px icon column, the label, muted metadata and the keys. */
function CommandItem({
  className,
  icon,
  trailing,
  command,
  disabled,
  children,
  ...props
}: CommandItemProps) {
  useKeybindingsVersion();
  return (
    <CommandItemPrimitive
      disabled={disabled ?? false}
      aria-keyshortcuts={command ? ariaKeyShortcut(bindingFor(command)) : undefined}
      className={cn(
        "relative flex h-8 min-w-0 cursor-default items-center gap-2.5 rounded-md px-2.5 text-sm outline-none select-none",
        "data-highlighted:bg-sel data-disabled:opacity-45 data-pending:cursor-progress",
        "in-data-has-icons:not-data-has-icon:ps-8.5 phone:h-11 phone:text-base",
        "[&_svg]:pointer-events-none [&_svg]:shrink-0",
        className,
      )}
      {...props}
    >
      {icon ? (
        <CommandItemIcon className="inline-flex size-3.5 shrink-0 items-center justify-center text-muted-foreground in-data-highlighted:text-foreground [&_svg:not([class*='size-'])]:size-3.5">
          {icon}
        </CommandItemIcon>
      ) : null}
      <span className="min-w-0 flex-1 truncate">{children}</span>
      {trailing ? (
        <span className="ms-auto shrink-0 truncate text-xs text-muted-foreground">{trailing}</span>
      ) : null}
      {command ? (
        <span aria-hidden="true" className="ms-auto inline-flex shrink-0 ps-2 phone:hidden">
          <ShortcutCaps command={command} />
        </span>
      ) : null}
    </CommandItemPrimitive>
  );
}

function CommandFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="command-footer"
      className={cn(
        "flex h-9 shrink-0 items-center gap-4 border-t border-border px-4 text-xs text-muted-foreground [word-break:keep-all]",
        "phone:h-auto phone:flex-wrap phone:gap-x-4 phone:gap-y-1 phone:py-2",
        className,
      )}
      {...props}
    />
  );
}

export {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandFooter,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
};
