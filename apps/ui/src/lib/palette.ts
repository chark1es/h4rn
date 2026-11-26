/**
 * Lowercase, with accents dropped from Latin letters, so "configuracion"
 * finds "Configuración". Other scripts keep their marks: Japanese voiced
 * kana (が) must not match their plain form (か).
 */
function fold(text: string): string {
  return text
    .toLowerCase()
    .normalize("NFD")
    .replace(/(?<=\p{Script=Latin})\p{M}+/gu, "")
    .normalize("NFC");
}

/**
 * Ranks a palette item against what was typed. cmdk hides items that score 0
 * and sorts the rest by score, so a prefix or a word start outranks letters
 * that merely appear in order ("deny" must not lead with "Audit dependency").
 */
export function paletteScore(value: string, search: string, keywords?: string[]): number {
  const needle = fold(search.trim());
  if (!needle) return 1;
  const haystack = fold([value, ...(keywords ?? [])].join(" "));
  // Words in any script: "Mostrar «En curso»" and "显示“进行中”" split on their
  // spaces and punctuation, not only on ASCII.
  const words = haystack.split(/[^\p{L}\p{N}]+/u).filter(Boolean);
  if (haystack.startsWith(needle)) return 1;
  if (words.some((word) => word.startsWith(needle))) return 0.8;
  const phrase = needle.split(/\s+/).filter(Boolean);
  if (phrase.length > 1 && phrase.every((part) => words.some((word) => word.startsWith(part)))) {
    return 0.7;
  }
  if (haystack.includes(needle)) return 0.5;
  // Letters in order are a last resort, and only for a few characters in
  // one word, so a short query cannot match a long, unrelated title.
  if ([...needle].length >= 3 && words.some((word) => isSubsequence(needle, word))) return 0.1;
  return 0;
}

function isSubsequence(needle: string, word: string): boolean {
  // Both sides by code point, so an astral character (a CJK Extension B
  // ideograph or an emoji) counts once, as the length guard above does.
  const chars = [...needle];
  let index = 0;
  for (const char of word) {
    if (char === chars[index]) index += 1;
    if (index === chars.length) return true;
  }
  return false;
}
