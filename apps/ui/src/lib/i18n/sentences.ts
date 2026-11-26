/**
 * Joins sentences into one paragraph, such as a toast's description.
 *
 * Empty parts are dropped. Sentences are separated by a space, except after
 * a full-width stop (。！？), where Chinese and Japanese put none. A daemon's
 * English sentence followed by a translated one still gets its space.
 */
export function joinSentences(parts: ReadonlyArray<string | null | undefined>): string {
  let text = "";
  for (const part of parts) {
    const sentence = part?.trim();
    if (!sentence) continue;
    if (text && !/[。！？]$/.test(text)) text += " ";
    text += sentence;
  }
  return text;
}
