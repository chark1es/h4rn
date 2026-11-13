import type { FsTurnChanges } from "@/gen";

/** Compact `+18 −4` summary for a turn's change set. */
export function diffStatsLabel(changes: FsTurnChanges | null): string {
  if (!changes) return "+0 −0";
  let added = 0;
  let removed = 0;
  for (const file of changes.files) {
    added += file.added;
    removed += file.removed;
  }
  return `+${added} −${removed}`;
}
