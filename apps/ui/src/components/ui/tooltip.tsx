import {
  TooltipContent as TooltipContentPrimitive,
  Tooltip as TooltipPrimitive,
  TooltipTrigger as TooltipTriggerPrimitive,
} from "@ui-sdk/react/tooltip";
import * as React from "react";

import { KeyCaps, ShortcutCaps } from "@/components/ui/kbd";
import { popupMotionClassName } from "@/components/ui/popover";
import type { CommandId } from "@/lib/keybindings";
import { cn } from "@/lib/utils";

const TOOLTIP_DELAY = 300;

function Tooltip({
  delay = TOOLTIP_DELAY,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive>) {
  return <TooltipPrimitive delay={delay} {...props} />;
}

const TooltipTrigger = TooltipTriggerPrimitive;

function TooltipContent({
  className,
  sideOffset = 6,
  collisionPadding = 8,
  ...props
}: Omit<React.ComponentProps<typeof TooltipContentPrimitive>, "className"> & {
  className?: string;
}) {
  return (
    <TooltipContentPrimitive
      sideOffset={sideOffset}
      collisionPadding={collisionPadding}
      className={cn(
        "pointer-events-none inline-flex max-w-72 items-center gap-2 rounded-md border border-border bg-popover px-2 py-1 text-xs text-popover-foreground shadow-popup",
        popupMotionClassName,
        "data-instant:transition-none",
        className,
      )}
      {...props}
    />
  );
}

type TipProps = {
  /** What the control does, in a few words. */
  label: React.ReactNode;
  /** A rebindable command whose current keys to show. */
  command?: CommandId | undefined;
  /** Fixed keys to show, space-separated, when there is no command. */
  keys?: string | undefined;
  side?: React.ComponentProps<typeof TooltipContentPrimitive>["side"];
  /** Whether the tooltip names the control or only adds to its own name. */
  type?: "label" | "description";
  disabled?: boolean;
  /** The control. It keeps its own element, props and children. */
  children: React.ReactElement;
};

/** A tooltip on an existing control, with its shortcut when it has one. */
function Tip({ label, command, keys, side = "bottom", type, disabled, children }: TipProps) {
  return (
    <Tooltip {...(type ? { type } : {})} {...(disabled ? { disabled } : {})}>
      <TooltipTrigger render={children} />
      <TooltipContent side={side}>
        <span className="min-w-0">{label}</span>
        {command ? <ShortcutCaps command={command} /> : keys ? <KeyCaps value={keys} /> : null}
      </TooltipContent>
    </Tooltip>
  );
}

/** Text that ellipsizes, with a tooltip of the full value only while it is cut off. */
function TruncatedText({
  children,
  className,
  render,
}: {
  children: string;
  className?: string;
  render?: React.ReactElement;
}) {
  const ref = React.useRef<HTMLElement | null>(null);
  return (
    <Tooltip
      type="label"
      onOpenChange={(open, details) => {
        const node = ref.current;
        if (open && node && node.scrollWidth <= node.clientWidth) details.cancel();
      }}
    >
      <TooltipTrigger
        ref={ref as React.Ref<HTMLButtonElement>}
        render={render ?? <span />}
        className={cn("block min-w-0 truncate", className)}
      >
        {children}
      </TooltipTrigger>
      <TooltipContent side="bottom" align="start">
        {children}
      </TooltipContent>
    </Tooltip>
  );
}

export { Tip, type TipProps, Tooltip, TooltipContent, TooltipTrigger, TruncatedText };
