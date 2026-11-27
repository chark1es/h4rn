import {
  MenuCheckboxItem as MenuCheckboxItemPrimitive,
  MenuContent as MenuContentPrimitive,
  MenuContextTrigger as MenuContextTriggerPrimitive,
  MenuGroupLabel as MenuGroupLabelPrimitive,
  MenuGroup as MenuGroupPrimitive,
  MenuItemIcon as MenuItemIconPrimitive,
  MenuItemIndicator as MenuItemIndicatorPrimitive,
  MenuItem as MenuItemPrimitive,
  Menu as MenuPrimitive,
  MenuRadioGroup,
  MenuRadioItem as MenuRadioItemPrimitive,
  MenuSeparator as MenuSeparatorPrimitive,
  MenuSub,
  MenuSubContent as MenuSubContentPrimitive,
  MenuSubTrigger as MenuSubTriggerPrimitive,
  MenuTrigger,
} from "@ui-sdk/react/menu";
import { CheckIcon, ChevronRightIcon } from "lucide-react";
import type * as React from "react";

import { KeyGlyphs } from "@/components/ui/kbd";
import {
  ariaKeyShortcut,
  bindingFor,
  type CommandId,
  chordText,
  useKeybindingsVersion,
} from "@/lib/keybindings";
import { cn } from "@/lib/utils";

type WithClassName<P> = Omit<P, "className" | "children" | "disabled"> & {
  className?: string | undefined;
  children?: React.ReactNode;
  disabled?: boolean | undefined;
};

/**
 * The surface every list popup shares: menus, selects, comboboxes. One popup
 * shadow and hairline, scaled from the trigger side, with a faster exit.
 */
const popupSurface = [
  "min-w-40 max-w-80 overflow-y-auto overscroll-contain rounded-lg border border-border bg-popover p-1 text-sm text-popover-foreground shadow-popup outline-none",
  "max-h-[min(var(--available-height),--spacing(120))]",
  "origin-(--transform-origin) transition-[opacity,scale] duration-popup ease-enter",
  "data-starting-style:scale-96 data-starting-style:opacity-0",
  "data-ending-style:scale-96 data-ending-style:opacity-0 data-ending-style:duration-popup-exit data-ending-style:ease-exit",
  "motion-reduce:transition-opacity",
];

/**
 * A phone bottom sheet: full width, top corners, 44 px rows, clear of the
 * home indicator, the modal shadow, and a slide from the bottom edge in place
 * of the popup's scale and fade.
 */
const sheetSurface = [
  "data-[presentation=sheet]:max-h-[80dvh] data-[presentation=sheet]:w-full data-[presentation=sheet]:max-w-none",
  "data-[presentation=sheet]:rounded-none data-[presentation=sheet]:rounded-t-xl data-[presentation=sheet]:border-x-0 data-[presentation=sheet]:border-b-0",
  "data-[presentation=sheet]:p-2 data-[presentation=sheet]:pb-[max(--spacing(2),env(safe-area-inset-bottom))] data-[presentation=sheet]:shadow-modal",
  "data-[presentation=sheet]:transition-[translate] data-[presentation=sheet]:duration-modal",
  "data-[presentation=sheet]:data-starting-style:translate-y-full data-[presentation=sheet]:data-starting-style:scale-100 data-[presentation=sheet]:data-starting-style:opacity-100",
  "data-[presentation=sheet]:data-ending-style:translate-y-full data-[presentation=sheet]:data-ending-style:scale-100 data-[presentation=sheet]:data-ending-style:opacity-100 data-[presentation=sheet]:data-ending-style:duration-modal-exit",
  "motion-reduce:data-[presentation=sheet]:transition-none",
];

/**
 * Rows are 28 px with a 14 px leading column. When any row in the popup has
 * an icon or a check, rows without one indent to the same label edge.
 */
const itemRow = [
  "relative flex h-7 min-w-0 cursor-default items-center gap-2 rounded-sm px-2 outline-none select-none",
  "data-highlighted:bg-accent data-highlighted:text-accent-foreground",
  "data-disabled:pointer-events-none data-disabled:opacity-45 data-pending:cursor-progress",
  "data-destructive:text-destructive data-destructive:data-highlighted:bg-danger-soft",
  "in-data-has-icons:not-data-has-icon:ps-7.5",
  "data-has-description:h-auto data-has-description:min-h-7 data-has-description:py-1",
  "phone:h-11 phone:text-base in-data-[presentation=sheet]:h-11 in-data-[presentation=sheet]:text-base",
  "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-3.5",
];

function Menu({ presentation = "auto", ...props }: React.ComponentProps<typeof MenuPrimitive>) {
  return <MenuPrimitive presentation={presentation} {...props} />;
}

function MenuContent({
  className,
  sideOffset = 4,
  collisionPadding = 8,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuContentPrimitive>>) {
  return (
    <MenuContentPrimitive
      sideOffset={sideOffset}
      collisionPadding={collisionPadding}
      className={cn(popupSurface, sheetSurface, className)}
      {...props}
    />
  );
}

function MenuSubContent({
  className,
  collisionPadding = 8,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuSubContentPrimitive>>) {
  return (
    <MenuSubContentPrimitive
      collisionPadding={collisionPadding}
      className={cn(popupSurface, sheetSurface, className)}
      {...props}
    />
  );
}

type MenuItemProps = WithClassName<React.ComponentProps<typeof MenuItemPrimitive>> & {
  icon?: React.ReactNode;
  /** A muted second line under the label. */
  description?: React.ReactNode;
  /** Muted metadata at the end of the row, like a state or a count. */
  trailing?: React.ReactNode;
  /** Shows this command's current binding and announces it. */
  command?: CommandId | undefined;
};

function MenuItem({
  className,
  icon,
  description,
  trailing,
  command,
  disabled,
  children,
  ...props
}: MenuItemProps) {
  useKeybindingsVersion();
  return (
    <MenuItemPrimitive
      disabled={disabled ?? false}
      aria-keyshortcuts={command ? ariaKeyShortcut(bindingFor(command)) : undefined}
      data-has-description={description ? "" : undefined}
      className={cn(itemRow, className)}
      {...props}
    >
      {icon ? <MenuItemIcon>{icon}</MenuItemIcon> : null}
      <MenuItemLabel description={description}>{children}</MenuItemLabel>
      {trailing ? <MenuItemTrailing>{trailing}</MenuItemTrailing> : null}
      {command ? <MenuShortcut command={command} /> : null}
    </MenuItemPrimitive>
  );
}

function MenuItemLabel({
  children,
  description,
}: {
  children: React.ReactNode;
  description?: React.ReactNode;
}) {
  if (!description) return <span className="min-w-0 flex-1 truncate">{children}</span>;
  return (
    <span className="flex min-w-0 flex-1 flex-col">
      <span className="truncate">{children}</span>
      <span className="truncate text-xs text-muted-foreground in-data-highlighted:text-current/70">
        {description}
      </span>
    </span>
  );
}

function MenuItemTrailing({ children }: { children: React.ReactNode }) {
  return (
    <span className="ms-auto inline-flex shrink-0 items-center gap-1.5 ps-4 text-xs text-muted-foreground tabular-nums">
      {children}
    </span>
  );
}

function MenuItemIcon({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuItemIconPrimitive>>) {
  return (
    <MenuItemIconPrimitive
      className={cn(
        "inline-flex size-3.5 shrink-0 items-center justify-center text-muted-foreground in-data-highlighted:text-current",
        className,
      )}
      {...props}
    />
  );
}

/** The check in the leading column, kept mounted so the column holds its width. */
function MenuCheck() {
  return (
    <MenuItemIcon>
      <MenuItemIndicatorPrimitive keepMounted className="inline-flex data-unchecked:invisible">
        <CheckIcon strokeWidth={2} aria-hidden="true" />
      </MenuItemIndicatorPrimitive>
    </MenuItemIcon>
  );
}

function MenuCheckboxItem({
  className,
  children,
  disabled,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuCheckboxItemPrimitive>>) {
  return (
    <MenuCheckboxItemPrimitive
      disabled={disabled ?? false}
      className={cn(itemRow, className)}
      {...props}
    >
      <MenuCheck />
      <span className="min-w-0 flex-1 truncate">{children}</span>
    </MenuCheckboxItemPrimitive>
  );
}

function MenuRadioItem({
  className,
  children,
  disabled,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuRadioItemPrimitive>>) {
  return (
    <MenuRadioItemPrimitive
      disabled={disabled ?? false}
      className={cn(itemRow, className)}
      {...props}
    >
      <MenuCheck />
      <span className="min-w-0 flex-1 truncate">{children}</span>
    </MenuRadioItemPrimitive>
  );
}

function MenuSubTrigger({
  className,
  icon,
  disabled,
  children,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuSubTriggerPrimitive>> & {
  icon?: React.ReactNode;
}) {
  return (
    <MenuSubTriggerPrimitive
      disabled={disabled ?? false}
      className={cn(itemRow, "data-open:bg-accent", className)}
      {...props}
    >
      {icon ? <MenuItemIcon>{icon}</MenuItemIcon> : null}
      <span className="min-w-0 flex-1 truncate">{children}</span>
      <ChevronRightIcon className="text-muted-foreground rtl:rotate-180" aria-hidden="true" />
    </MenuSubTriggerPrimitive>
  );
}

function MenuGroup({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuGroupPrimitive>>) {
  return <MenuGroupPrimitive className={cn(className)} {...props} />;
}

function MenuGroupLabel({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuGroupLabelPrimitive>>) {
  return (
    <MenuGroupLabelPrimitive
      className={cn(
        "flex h-6 min-w-0 items-center truncate px-2 text-xs font-medium text-muted-foreground phone:h-8",
        className,
      )}
      {...props}
    />
  );
}

function MenuSeparator({
  className,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuSeparatorPrimitive>>) {
  return (
    <MenuSeparatorPrimitive className={cn("-mx-1 my-1 h-px bg-border", className)} {...props} />
  );
}

/** A command's keys as plain muted text, right-aligned; follows rebinding live. */
function MenuShortcut({ command, className }: { command: CommandId; className?: string }) {
  useKeybindingsVersion();
  return (
    <span
      aria-hidden="true"
      data-slot="menu-shortcut"
      className={cn(
        "ms-auto inline-flex items-center gap-px ps-4 text-xs text-muted-foreground tabular-nums phone:hidden [&_svg]:size-3",
        className,
      )}
    >
      <KeyGlyphs value={chordText(bindingFor(command))} />
    </span>
  );
}

/** Non-interactive text at the foot of a menu, like when a thread started. */
function MenuFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="menu-footer"
      className={cn("px-2 pt-1 pb-0.5 text-xs text-muted-foreground tabular-nums", className)}
      {...props}
    />
  );
}

/** Non-interactive text inside a group, lined up with the item labels. */
function MenuNote({
  className,
  tone,
  ...props
}: React.ComponentProps<"p"> & { tone?: "danger" | undefined }) {
  return (
    <p
      data-slot="menu-note"
      className={cn(
        "m-0 px-2 pt-0.5 pb-1.5 text-xs text-muted-foreground in-data-has-icons:ps-7.5",
        tone === "danger" && "text-destructive",
        className,
      )}
      {...props}
    />
  );
}

function MenuContextTrigger({
  className,
  disabled,
  ...props
}: WithClassName<React.ComponentProps<typeof MenuContextTriggerPrimitive>>) {
  return (
    <MenuContextTriggerPrimitive
      disabled={disabled ?? false}
      className={cn(className)}
      {...props}
    />
  );
}

export {
  itemRow,
  Menu,
  MenuCheckboxItem,
  MenuContent,
  MenuContextTrigger,
  MenuFooter,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuItemIcon,
  MenuNote,
  MenuRadioGroup,
  MenuRadioItem,
  MenuSeparator,
  MenuShortcut,
  MenuSub,
  MenuSubContent,
  MenuSubTrigger,
  MenuTrigger,
  popupSurface,
  sheetSurface,
};
