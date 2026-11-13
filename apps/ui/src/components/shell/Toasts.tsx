import { CircleAlertIcon, InfoIcon, XIcon } from "lucide-react";

import { dismissToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

export function Toasts() {
  const toasts = useApp((state) => state.toasts);
  if (toasts.length === 0) return null;

  return (
    <section
      aria-live="polite"
      aria-label="Notifications"
      className="pointer-events-none fixed right-3 bottom-10 z-50 flex w-[min(360px,calc(100vw-1.5rem))] flex-col gap-2"
    >
      {toasts.slice(-3).map((toast) => (
        <div
          key={toast.id}
          role={toast.variant === "error" ? "alert" : "status"}
          className={cn(
            "pointer-events-auto flex items-start gap-2 rounded-md border bg-raised px-3 py-2 shadow-[var(--shadow-overlay-low)]",
            toast.variant === "error" ? "border-strike/40" : "border-line",
          )}
        >
          {toast.variant === "error" ? (
            <CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-strike-ink" aria-hidden />
          ) : (
            <InfoIcon className="mt-0.5 size-4 shrink-0 text-ink-muted" aria-hidden />
          )}
          <div className="min-w-0 flex-1">
            <p
              className={cn(
                "text-compact font-medium",
                toast.variant === "error" ? "text-strike-ink" : "text-ink",
              )}
            >
              {toast.title}
            </p>
            {toast.description ? (
              <p className="mt-0.5 text-micro leading-relaxed text-ink-muted">
                {toast.description}
              </p>
            ) : null}
          </div>
          <button
            type="button"
            aria-label="Dismiss"
            className="flex size-5 shrink-0 items-center justify-center rounded-sm text-ink-faint hover:text-ink"
            onClick={() => dismissToast(toast.id)}
          >
            <XIcon className="size-3.5" />
          </button>
        </div>
      ))}
    </section>
  );
}
