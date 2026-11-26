/** How a rewind confirmation names the ignored paths it leaves alone. */
export function leftAloneSummary(paths: string[] | undefined): string | null {
  if (!paths || paths.length === 0) return null;
  const shown = paths.slice(0, 4);
  const rest = paths.length - shown.length;
  const list = shown.join(", ");
  return rest > 0 ? `${list} and ${rest} more` : list;
}
