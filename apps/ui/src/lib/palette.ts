/**
 * Ranks a palette item against what was typed. cmdk hides items that score 0
 * and sorts the rest by score, so a prefix or a word start outranks letters
 * that merely appear in order ("deny" must not lead with "Audit dependency").
 */
export function paletteScore(value: string, search: string, keywords?: string[]): number {
  const needle = search.trim().toLowerCase();
  if (!needle) return 1;
  const haystack = [value, ...(keywords ?? [])].join(" ").toLowerCase();
  const words = haystack.split(/[^a-z0-9]+/).filter(Boolean);
  if (haystack.startsWith(needle)) return 1;
  if (words.some((word) => word.startsWith(needle))) return 0.8;
  const phrase = needle.split(/\s+/).filter(Boolean);
  if (phrase.length > 1 && phrase.every((part) => words.some((word) => word.startsWith(part)))) {
    return 0.7;
  }
  if (haystack.includes(needle)) return 0.5;
  // Letters in order are a last resort, and only for a few characters in
  // one word, so a short query cannot match a long, unrelated title.
  if (needle.length >= 3 && words.some((word) => isSubsequence(needle, word))) return 0.1;
  return 0;
}

function isSubsequence(needle: string, word: string): boolean {
  let index = 0;
  for (const char of word) {
    if (char === needle[index]) index += 1;
    if (index === needle.length) return true;
  }
  return false;
}
