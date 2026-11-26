/** How the Snoozed list says when a thread comes back. */

import { formatAge, i18n } from "@/lib/i18n";

/**
 * "wakes in 5m", or "wakes soon" once less than a minute is left (or the
 * daemon has not woken it yet), so a row never reads "wakes in now".
 */
export function snoozeWakeLabel(until: string | null | undefined, now: number): string {
  const time = until ? Date.parse(until) : Number.NaN;
  if (!Number.isFinite(time) || time - now < 60_000) return i18n.t("sidebar:snoozed.wakesSoon");
  return i18n.t("sidebar:snoozed.wakesIn", { age: formatAge(time, now) });
}
