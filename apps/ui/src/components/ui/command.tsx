import { Command as CommandPrimitive } from "cmdk";
import { SearchIcon } from "lucide-react";
import type * as React from "react";
import { Children } from "react";

import { KeyGlyphs } from "@/components/common/Kbd";
import { bindingFor, type CommandId, chordKeys, useKeybindingsVersion } from "@/lib/keybindings";
import { cn } from "@/lib/utils";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "./dialog";

function Command({ className, ...props }: React.ComponentProps<typeof CommandPrimitive>) {
  return (
    <CommandPrimitive
      data-slot="command"
      className={cn("flex h-full w-full flex-col overflow-hidden bg-bg-raised text-fg", className)}
      {...props}
    />
  );
}

function CommandDialog({
  title = "Command palette",
  description = "Search for a command to run.",
  children,
  className,
  showCloseButton = false,
  onOpenAutoFocus,
  onCloseAutoFocus,
  filter,
  ...props
}: React.ComponentProps<typeof Dialog> & {
  filter?: React.ComponentProps<typeof CommandPrimitive>["filter"];
  title?: string;
  description?: string;
  className?: string;
  showCloseButton?: boolean;
  onOpenAutoFocus?: React.ComponentProps<typeof DialogContent>["onOpenAutoFocus"];
  onCloseAutoFocus?: React.ComponentProps<typeof DialogContent>["onCloseAutoFocus"];
}) {
  return (
    <Dialog {...props}>
      <DialogHeader className="sr-only">
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>{description}</DialogDescription>
      </DialogHeader>
      <DialogContent
        className={cn("q-palette block gap-0 overflow-hidden p-0", className)}
        showCloseButton={showCloseButton}
        onOpenAutoFocus={onOpenAutoFocus}
        onCloseAutoFocus={onCloseAutoFocus}
      >
        {/* Vim-style Ctrl+J/K/N/P list movement would swallow app shortcuts such as
            Ctrl+K to close the palette; arrows move the selection instead. */}
        <Command vimBindings={false} {...(filter ? { filter } : {})}>
          {children}
        </Command>
      </DialogContent>
    </Dialog>
  );
}

function CommandInput({
  className,
  trailing,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Input> & { trailing?: React.ReactNode }) {
  return (
    <div data-slot="command-input-wrapper" className="q-pal-in">
      <SearchIcon className="q-icon s18 q-faint" strokeWidth={1.5} aria-hidden />
      <CommandPrimitive.Input
        data-slot="command-input"
        className={cn(
          "h-full w-full min-w-0 border-0 bg-transparent text-[15px] text-fg outline-none placeholder:text-fg-3 disabled:cursor-not-allowed disabled:opacity-50",
          className,
        )}
        {...props}
      />
      {trailing}
    </div>
  );
}

function CommandList({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.List>) {
  return (
    <CommandPrimitive.List
      data-slot="command-list"
      className={cn("q-pal-list", className)}
      {...props}
    />
  );
}

function CommandEmpty({ ...props }: React.ComponentProps<typeof CommandPrimitive.Empty>) {
  return (
    <CommandPrimitive.Empty
      data-slot="command-empty"
      className="py-6 text-center text-[12.5px] text-fg-2"
      {...props}
    />
  );
}

function CommandGroup({
  className,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Group>) {
  return (
    <CommandPrimitive.Group
      data-slot="command-group"
      className={cn("overflow-hidden text-fg", className)}
      {...props}
    />
  );
}

function CommandSeparator({
  className,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Separator>) {
  return (
    <CommandPrimitive.Separator
      data-slot="command-separator"
      className={cn("mx-2 my-1 h-px bg-line", className)}
      {...props}
    />
  );
}

function CommandItem({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.Item>) {
  return (
    <CommandPrimitive.Item
      data-slot="command-item"
      className={cn(
        "q-pal-i cursor-default outline-none select-none data-[disabled=true]:pointer-events-none data-[disabled=true]:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4 [&_svg]:shrink-0",
        className,
      )}
      {...props}
    />
  );
}

function CommandShortcut({
  className,
  children,
  command,
  ...props
}: React.ComponentProps<"span"> & { command?: CommandId }) {
  useKeybindingsVersion();
  const text = command
    ? chordKeys(bindingFor(command)).join(" ")
    : Children.toArray(children)
        .map((child) => (typeof child === "string" ? child : ""))
        .join("");
  const keys = text.trim().split(/\s+/).filter(Boolean);
  const seen = new Map<string, number>();
  const chips = keys.map((key) => {
    const ordinal = seen.get(key) ?? 0;
    seen.set(key, ordinal + 1);
    return { key, id: `${key}-${ordinal}` };
  });

  return (
    <span data-slot="command-shortcut" className={cn("q-pal-kbd", className)} {...props}>
      {chips.length > 0
        ? chips.map((chip) => (
            <span className="q-kbd" key={chip.id}>
              <KeyGlyphs value={chip.key} />
            </span>
          ))
        : children}
    </span>
  );
}

function CommandFooter({ className, ...props }: React.ComponentProps<"div">) {
  return <div data-slot="command-footer" className={cn("q-pal-foot", className)} {...props} />;
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
  CommandShortcut,
};
