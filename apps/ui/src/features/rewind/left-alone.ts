import { formatList, i18n } from "@/lib/i18n";

/**
 * The ignored paths a rewind leaves alone: the first four by name, then a
 * count. The Edit from here dialog and the rewind toast both use it, so the
 * list reads the same before and after.
 */
export function leftAloneList(paths: readonly string[] | undefined): string | null {
  if (!paths || paths.length === 0) return null;
  const shown = formatList(paths.slice(0, 4), "conjunction", "narrow");
  const rest = paths.length - 4;
  return rest > 0 ? i18n.t("dialogs:rewind.ignoredMore", { paths: shown, count: rest }) : shown;
}
