/**
 * Composer trigger detection and ranking for the `@` file picker and the `/`
 * command menu.
 *
 * Everything here is pure so the rules (word boundary, first-character `/`,
 * path-aware fuzzy matching, quoting, range replacement) can be tested without
 * a DOM. The composer owns the textarea and the menu owns its own state.
 */

export type ComposerTrigger =
  | { kind: "file"; query: string; start: number; end: number }
  | { kind: "command"; query: string; start: number; end: number };

const WHITESPACE = /\s/;

function isWhitespace(char: string | undefined): boolean {
  return char !== undefined && WHITESPACE.test(char);
}

/** Start of the non-whitespace token that contains `caret`. */
function tokenStart(value: string, caret: number): number {
  let index = caret - 1;
  while (index >= 0 && !isWhitespace(value[index])) index -= 1;
  return index + 1;
}

/** End of the non-whitespace token that contains `caret`. */
function tokenEnd(value: string, caret: number): number {
  let index = caret;
  while (index < value.length && !isWhitespace(value[index])) index += 1;
  return index;
}

/**
 * The trigger the caret is inside, or `null`.
 *
 * `@` only opens at a word boundary, so `me@example.com` is left alone. `/`
 * only opens as the first character of the composer; a slash later in the
 * prompt is path text, not a command.
 */
export function detectComposerTrigger(value: string, caretInput: number): ComposerTrigger | null {
  const caret = Math.max(0, Math.min(caretInput, value.length));
  const start = tokenStart(value, caret);
  const end = tokenEnd(value, caret);
  const token = value.slice(start, end);

  if (token.startsWith("@")) {
    return { kind: "file", query: token.slice(1, caret - start), start, end };
  }

  if (start === 0 && token.startsWith("/")) {
    return { kind: "command", query: token.slice(1, caret - start), start, end };
  }

  return null;
}

/** Stable identity for one trigger, used to keep Escape dismissed. */
export function triggerKey(trigger: ComposerTrigger): string {
  return `${trigger.kind}:${trigger.start}`;
}

/**
 * Path-aware fuzzy score, or `null` when the query does not match in order.
 *
 * Exact substrings score highest, then characters that start a path segment,
 * then runs of adjacent characters. `src/comp` therefore finds
 * `apps/ui/src/components/...` even though the query skips `apps/ui/`.
 */
export function fuzzyScore(query: string, value: string): number | null {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) return 0;
  const haystack = value.toLowerCase();
  if (needle.length > haystack.length) return null;

  const exact = haystack.indexOf(needle);
  if (exact >= 0) {
    const boundary = exact === 0 || isBoundary(haystack[exact - 1]);
    const basename = haystack.lastIndexOf("/") + 1;
    let score = 1_000 - exact;
    if (boundary) score += 120;
    if (exact >= basename) score += 80;
    if (exact + needle.length === haystack.length) score += 40;
    return score;
  }

  let score = 0;
  let run = 0;
  let previous = -2;
  let first = -1;
  let cursor = 0;

  for (const char of needle) {
    const found = haystack.indexOf(char, cursor);
    if (found < 0) return null;
    if (first < 0) first = found;
    if (found === previous + 1) {
      run += 1;
    } else {
      run = 0;
    }
    score += 10 + Math.min(run, 6) * 4;
    if (found === 0 || isBoundary(haystack[found - 1])) score += 30;
    if (found === haystack.lastIndexOf("/") + 1) score += 24;
    previous = found;
    cursor = found + 1;
  }

  // Prefer a compact match over one scattered across the whole path.
  score -= Math.min(60, Math.max(0, previous - first - needle.length) * 2);
  return score;
}

function isBoundary(char: string | undefined): boolean {
  return char === "/" || char === "\\" || char === "." || char === "-" || char === "_";
}

/** Rank workspace paths for a query and cap the list. */
export function rankFilePaths(paths: readonly string[], query: string, limit = 50): string[] {
  const scored: Array<{ path: string; score: number }> = [];
  for (const path of paths) {
    const score = fuzzyScore(query, path);
    if (score !== null) scored.push({ path, score });
  }
  scored.sort(
    (left, right) =>
      right.score - left.score ||
      left.path.length - right.path.length ||
      left.path.localeCompare(right.path),
  );
  return scored.slice(0, limit).map((entry) => entry.path);
}

/** The `@` reference for a path, quoting paths that contain spaces. */
export function fileReference(path: string): string {
  return /\s/.test(path) ? `@"${path.replace(/"/g, '\\"')}"` : `@${path}`;
}

/**
 * Replace the trigger token with a completion. A trailing space already after
 * the token is reused so completing before existing text does not double it.
 */
export function applyCompletion(
  value: string,
  range: { start: number; end: number },
  insertion: string,
): { value: string; caret: number } {
  const start = Math.max(0, Math.min(range.start, value.length));
  let end = Math.max(start, Math.min(range.end, value.length));
  const withSpace = insertion.length > 0 && !insertion.endsWith(" ");
  const replacement = withSpace ? `${insertion} ` : insertion;
  if (replacement.endsWith(" ") && value[end] === " ") end += 1;
  return {
    value: `${value.slice(0, start)}${replacement}${value.slice(end)}`,
    caret: start + replacement.length,
  };
}

/** Wrap the highlighted row in a menu with `count` rows. */
export function stepSelection(index: number, count: number, delta: 1 | -1): number {
  if (count <= 0) return 0;
  return (index + delta + count) % count;
}

export function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/** A command row the composer can rank without knowing what running it does. */
export type CommandSearchItem = {
  label: string;
  keywords?: string;
  hint?: string;
};

/** Rank commands by label, then keywords and hint. */
export function rankCommands<T extends CommandSearchItem>(
  commands: readonly T[],
  query: string,
): T[] {
  const needle = query.trim();
  if (needle.length === 0) return [...commands];
  const scored: Array<{ item: T; score: number }> = [];
  for (const item of commands) {
    const labelScore = fuzzyScore(needle, item.label);
    const keywordsScore = item.keywords ? fuzzyScore(needle, item.keywords) : null;
    const hintScore = item.hint ? fuzzyScore(needle, item.hint) : null;
    const best = Math.max(
      labelScore === null ? Number.NEGATIVE_INFINITY : labelScore + 40,
      keywordsScore ?? Number.NEGATIVE_INFINITY,
      hintScore ?? Number.NEGATIVE_INFINITY,
    );
    if (best !== Number.NEGATIVE_INFINITY) scored.push({ item, score: best });
  }
  scored.sort((left, right) => right.score - left.score);
  return scored.map((entry) => entry.item);
}
