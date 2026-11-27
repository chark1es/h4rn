import {
  CircleAlertIcon,
  CircleCheckIcon,
  InfoIcon,
  LoaderCircleIcon,
  TriangleAlertIcon,
  XIcon,
} from "lucide-react";
import { useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
import { Toaster as Sonner, type ToastClassnames, type ToasterProps } from "sonner";

type ToastIcons = NonNullable<ToasterProps["icons"]>;

import { buttonVariants } from "@/components/ui/button";
import { useIsPhone } from "@/hooks/use-media-query";
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
    "group/toast flex w-(--width) items-start gap-2.5 rounded-xl border border-border bg-popover py-3 pr-10 pl-3 font-sans text-sm text-popover-foreground shadow-popup",
    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
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

/**
 * The app's one toast stack, on Sonner. Bottom right on desktop, above the
 * composer when there is one; top centre on the phone, below the safe area.
 */
function Toaster(props: ToasterProps) {
  const { t } = useTranslation("shell");
  const theme = useDocumentTheme();
  const phone = useIsPhone();
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
