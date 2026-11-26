import { formatList, i18n } from "@/lib/i18n";

/**
 * The ignored paths a rewind leaves alone: the first four by name, then a
 * count. The Edit from here dialog and the rewind toast both use it, so the
 * list reads the same before and after.
 *
 * The count is the list's last item, so the language joins it once: "a, b, c,
 * d, and 2 more", "a, b, c, d y 2 más". Gluing it after an already joined
 * list would say "and" twice in languages whose short lists end in one
 * ("a, b, c y d y 2 más").
 */
export function leftAloneList(paths: readonly string[] | undefined): string | null {
  if (!paths || paths.length === 0) return null;
  const shown = paths.slice(0, 4);
  const rest = paths.length - shown.length;
  if (rest <= 0) return formatList(shown, "conjunction", "narrow");
  const more = i18n.t("dialogs:rewind.ignoredMore", { count: rest });
  return formatList([...shown, more], "conjunction", "long");
}
