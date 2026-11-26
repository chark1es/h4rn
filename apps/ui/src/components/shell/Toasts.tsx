import { CircleAlertIcon, InfoIcon, XIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { dismissToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

export function Toasts() {
  const toasts = useApp((state) => state.toasts);
  const { t } = useTranslation("shell");
  if (toasts.length === 0) return null;

  return (
    <section
      aria-live="polite"
      aria-label={t("toasts.region")}
      className="pointer-events-none fixed right-3 bottom-10 z-50 flex w-[min(380px,calc(100vw-1.5rem))] flex-col gap-2"
    >
      {toasts.slice(-3).map((toast) => (
        <div
          key={toast.id}
          role={toast.variant === "error" ? "alert" : "status"}
          className={cn(
            "pointer-events-auto flex items-start gap-2.5 rounded-[10px] bg-[var(--bg-raised)] px-3 py-2.5 shadow-[var(--sh-pop),inset_0_0_0_1px_var(--line)]",
            toast.variant === "error" &&
              "shadow-[var(--sh-pop),inset_0_0_0_1px_color-mix(in_oklch,var(--danger)_35%,var(--line))]",
          )}
        >
          {toast.variant === "error" ? (
            <CircleAlertIcon
              className="mt-0.5 size-[15px] shrink-0 text-[var(--danger)]"
              strokeWidth={1.5}
              aria-hidden
            />
          ) : (
            <InfoIcon
              className="mt-0.5 size-[15px] shrink-0 text-[var(--fg-2)]"
              strokeWidth={1.5}
              aria-hidden
            />
          )}
          <div className="min-w-0 flex-1">
            <p
              className={cn(
                "text-[13px] font-medium",
                toast.variant === "error" ? "text-[var(--danger)]" : "text-fg",
              )}
            >
              {toast.title}
            </p>
            {toast.description ? (
              <p className="mt-0.5 text-[12px] leading-[17px] text-[var(--fg-2)]">
                {toast.description}
              </p>
            ) : null}
          </div>
          <button
            type="button"
            aria-label={t("toasts.dismiss")}
            className="q-ibtn"
            style={{ width: 22, height: 22 }}
            onClick={() => dismissToast(toast.id)}
          >
            <XIcon className="q-icon s14" strokeWidth={1.5} />
          </button>
        </div>
      ))}
    </section>
  );
}
