import {
  CircleAlertIcon,
  CircleCheckIcon,
  InfoIcon,
  LoaderCircleIcon,
  TriangleAlertIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
import { Toaster as Sonner, type ToastClassnames, type ToasterProps } from "sonner";

type ToastIcons = NonNullable<ToasterProps["icons"]>;

import { buttonVariants } from "@/components/ui/button";
import { useIsPhone } from "@/hooks/use-media-query";
import { tabbables } from "@/lib/focus";
import { cn } from "@/lib/utils";

type Theme = "light" | "dark";

function subscribeTheme(listener: () => void): () => void {
  const observer = new MutationObserver(listener);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
  return () => observer.disconnect();
}

function documentTheme(): Theme {
  return document.documentElement.classList.contains("dark") ? "dark" : "light";
}

/** The app's resolved theme, which can differ from the system's. */
function useDocumentTheme(): Theme {
  return useSyncExternalStore(subscribeTheme, documentTheme, () => "light");
}

const ICONS: ToastIcons = {
  success: <CircleCheckIcon className="size-4 text-ok" aria-hidden="true" />,
  error: <CircleAlertIcon className="size-4 text-danger" aria-hidden="true" />,
  info: <InfoIcon className="size-4 text-muted-foreground" aria-hidden="true" />,
  warning: <TriangleAlertIcon className="size-4 text-warn" aria-hidden="true" />,
  loading: (
    <LoaderCircleIcon
      className="size-4 animate-spin text-muted-foreground motion-reduce:animate-none"
      aria-hidden="true"
    />
  ),
  close: <XIcon className="size-3.5" aria-hidden="true" />,
};

const CLASS_NAMES: ToastClassnames = {
  toast: cn(
    "group/toast flex w-(--width) items-start gap-2.5 rounded-lg border border-border bg-popover py-3 pr-10 pl-3 font-sans text-sm text-popover-foreground shadow-popup",
    "focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-2 focus-visible:outline-ring",
  ),
  icon: "relative flex h-4.5 w-4 shrink-0 items-center justify-center",
  content: "flex min-w-0 flex-1 flex-col gap-0.5",
  title: cn("font-medium text-foreground"),
  description: cn("text-xs text-muted-foreground"),
  actionButton: cn(buttonVariants({ variant: "secondary", size: "sm" }), "self-center"),
  cancelButton: cn(buttonVariants({ variant: "ghost", size: "sm" }), "self-center"),
  closeButton: cn(
    buttonVariants({ variant: "ghost", size: "icon-sm" }),
    "absolute top-2.5 right-2.5",
  ),
};

/**
 * A ref callback for a bottom dock, like the thread's approval card and composer:
 * desktop toasts sit above it while it is on screen.
 */
function toastClearance(node: HTMLElement | null): (() => void) | undefined {
  if (!node) return undefined;
  const root = document.documentElement;
  const update = () => root.style.setProperty("--toast-dock", `${node.offsetHeight}px`);
  update();
  const observer = new ResizeObserver(update);
  observer.observe(node);
  return () => {
    observer.disconnect();
    root.style.removeProperty("--toast-dock");
  };
}

function toasterOf(node: EventTarget | null): HTMLElement | null {
  return node instanceof Element ? node.closest<HTMLElement>("[data-sonner-toaster]") : null;
}

/**
 * Sonner sends focus back to wherever it came from as soon as it leaves the
 * stack, so Tab past the last toast loops back into it. When the stack was
 * reached by Tab, leaving it by Tab continues in document order instead. Entry
 * by the hotkey (which focuses the list itself) keeps Sonner's return.
 */
function useTabThroughToasts(): void {
  useEffect(() => {
    let entry: { from: HTMLElement | null; byHotkey: boolean } | null = null;

    const onFocusIn = (event: FocusEvent) => {
      const list = toasterOf(event.target);
      if (!list) {
        entry = null;
        return;
      }
      if (toasterOf(event.relatedTarget) === list) return;
      entry = {
        from: event.relatedTarget instanceof HTMLElement ? event.relatedTarget : null,
        byHotkey: event.target === list,
      };
    };

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Tab" || event.defaultPrevented || event.altKey || event.ctrlKey) return;
      if (event.metaKey || !entry || entry.byHotkey) return;
      const active = document.activeElement;
      const list = toasterOf(active);
      if (!list || !(active instanceof HTMLElement)) return;
      const inside = tabbables(list);
      const edge = event.shiftKey ? inside[0] : inside[inside.length - 1];
      if (inside.length > 0 && active !== edge) return;

      const outside = tabbables(document.body).filter((el) => !list.contains(el));
      if (outside.length === 0) return;
      const after = outside.findIndex(
        (el) => list.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING,
      );
      const next = after === -1 ? 0 : after;
      const target = event.shiftKey
        ? outside[(next - 1 + outside.length) % outside.length]
        : outside[next];
      if (!target) return;

      event.preventDefault();
      const { from } = entry;
      entry = null;
      // Sonner's blur handler calls from.focus(); skip that one call.
      if (from && from !== target) {
        Object.defineProperty(from, "focus", { configurable: true, value: () => undefined });
      }
      try {
        HTMLElement.prototype.focus.call(target);
      } finally {
        if (from && from !== target) delete (from as { focus?: unknown }).focus;
      }
    };

    document.addEventListener("focusin", onFocusIn);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("focusin", onFocusIn);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, []);
}

/**
 * The app's one toast stack, on Sonner. Bottom right on desktop, above the
 * composer when there is one; top centre on the phone, below the safe area.
 */
function Toaster(props: ToasterProps) {
  const { t } = useTranslation("shell");
  const theme = useDocumentTheme();
  const phone = useIsPhone();
  useTabThroughToasts();
  return (
    <Sonner
      theme={theme}
      position={phone ? "top-center" : "bottom-right"}
      offset={{ bottom: "calc(var(--toast-dock, 1.5rem) + 0.75rem)", right: "1rem" }}
      mobileOffset={{
        top: "calc(env(safe-area-inset-top) + 3.25rem)",
        left: "0.5rem",
        right: "0.5rem",
      }}
      visibleToasts={3}
      duration={4500}
      gap={8}
      closeButton
      icons={ICONS}
      customAriaLabel={t("toasts.region")}
      toastOptions={{
        unstyled: true,
        closeButtonAriaLabel: t("toasts.dismiss"),
        classNames: CLASS_NAMES,
      }}
      {...props}
    />
  );
}

export { Toaster, toastClearance };
