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
/**
 * Characters that keep a mention inside a word (`me@example.com`, `foo@bar`).
 * Unicode letters and numbers count, so `café@src` is a word too.
 */
const IDENTIFIER = /[\p{L}\p{N}_]/u;

function isWhitespace(char: string | undefined): boolean {
  return char !== undefined && WHITESPACE.test(char);
}

/** True when `@` at `index` starts a token rather than sitting inside a word. */
function isWordBoundary(value: string, index: number): boolean {
  if (index <= 0) return true;
  const before = value[index - 1];
  if (before === undefined) return true;
  return isWhitespace(before) || !IDENTIFIER.test(before);
}

/** End of the non-whitespace token that contains `caret`. */
function tokenEnd(value: string, caret: number): number {
  let index = caret;
  while (index < value.length && !isWhitespace(value[index])) index += 1;
  return index;
}

/** True when the quote at `index` is escaped (`\"`), as `fileReference` writes it. */
function isEscaped(value: string, index: number): boolean {
  let backslashes = 0;
  for (let cursor = index - 1; cursor >= 0 && value[cursor] === "\\"; cursor -= 1) {
    backslashes += 1;
  }
  return backslashes % 2 === 1;
}

/** End of the line that holds `index`; a quoted mention never spans lines. */
function lineEnd(value: string, index: number): number {
  const newline = value.indexOf("\n", index);
  return newline < 0 ? value.length : newline;
}

/**
 * The quoted mention (`@"…"`) around the caret, or `null`.
 *
 * A quoted mention is one token even though it contains spaces. While its
 * closing quote has not been typed the picker filters on the text inside the
 * quotes; once the quote is closed the text is a finished mention, so no
 * trigger fires anywhere inside it. Quotes never pair across a newline: an
 * unfinished `@"` on one line must not swallow the opening quote of a finished
 * mention on the next.
 */
function quotedTrigger(value: string, caret: number): ComposerTrigger | null | "complete" {
  let index = 0;
  while (index < value.length - 1) {
    if (value[index] !== "@" || value[index + 1] !== '"' || !isWordBoundary(value, index)) {
      index += 1;
      continue;
    }
    const end = lineEnd(value, index + 2);
    let close = -1;
    for (let cursor = index + 2; cursor < end; cursor += 1) {
      if (value[cursor] === '"' && !isEscaped(value, cursor)) {
        close = cursor;
        break;
      }
    }
    if (close >= 0) {
      // A closed mention is inert, even when the caret sits inside it or
      // directly after its closing quote.
      if (caret > index && caret <= close + 1) return "complete";
      index = close + 1;
      continue;
    }
    if (caret > index + 1 && caret <= end) {
      return { kind: "file", query: value.slice(index + 2, caret), start: index, end };
    }
    // This mention ends with the line. Keep scanning the lines that follow
    // rather than treating the rest of the prompt as part of it.
    index = end;
  }
  return null;
}

/**
 * The trigger the caret is inside, or `null`.
 *
 * `@` opens after any non-identifier character — start of input, whitespace,
 * `(`, `[`, `"`, `,`, and so on — but not inside a word, so `me@example.com`
 * is left alone. The query runs from the `@` to the caret, so path text may
 * contain `-` and `/`. `/` only opens as the first character of the composer;
 * a slash later in the prompt is path text, not a command.
 */
export function detectComposerTrigger(value: string, caretInput: number): ComposerTrigger | null {
  const caret = Math.max(0, Math.min(caretInput, value.length));

  // A quoted mention is one token: `@"src/with space.ts"` keeps the filter
  // reading the text inside the quotes, and a finished mention fires nothing.
  const quoted = quotedTrigger(value, caret);
  if (quoted === "complete") return null;
  if (quoted) return quoted;

  const end = tokenEnd(value, caret);

  // The nearest `@` in the current token, provided nothing but the token
  // itself sits in front of it.
  let at = caret - 1;
  while (at >= 0 && !isWhitespace(value[at]) && value[at] !== "@") at -= 1;
  if (at >= 0 && value[at] === "@" && isWordBoundary(value, at)) {
    return { kind: "file", query: value.slice(at + 1, caret), start: at, end };
  }

  if (value.startsWith("/") && caret > 0) {
    // Only the first token is a command: once whitespace ends it, the slash
    // text is part of the prompt.
    const firstTokenEnd = tokenEnd(value, 0);
    if (caret <= firstTokenEnd) {
      return { kind: "command", query: value.slice(1, caret), start: 0, end: firstTokenEnd };
    }
  }

  return null;
}

/**
 * Where the caret-anchored popover should sit, relative to its positioned
 * parent. The caller measures the popover first: a width-agnostic guess
 * overflows the viewport for long paths.
 */
export type PopoverPlacement = {
  /** Distance from the parent's left edge. */
  left: number;
  /** Set when the popover sits above the caret line, measured from the parent's bottom. */
  bottom: number | null;
  /** Set when there is no room above and the popover flips below the caret line. */
  top: number | null;
  /** True when the popover had to move up or sideways to stay on screen. */
  flipped: boolean;
};

export function placePopover({
  anchor,
  size,
  parent,
  viewport,
  margin = 8,
  lineHeight = 21,
}: {
  anchor: { left: number; bottom: number };
  size: { width: number; height: number };
  parent: { left: number; top: number; width: number; height: number };
  viewport: { width: number; height: number };
  margin?: number;
  lineHeight?: number;
}): PopoverPlacement {
  const minLeft = margin - parent.left;
  const maxLeft = viewport.width - margin - size.width - parent.left;
  // A popover wider than the screen keeps its left edge on screen.
  const left = maxLeft < minLeft ? minLeft : Math.max(minLeft, Math.min(anchor.left, maxLeft));

  const caretTop = parent.top + parent.height - anchor.bottom + 6;
  const aboveTop = caretTop - size.height;
  if (aboveTop >= margin) {
    return { left, bottom: anchor.bottom, top: null, flipped: left !== anchor.left };
  }

  // No room above the caret (a tall menu near the top of the screen): open
  // downwards and pull it back up if even that would leave the viewport.
  const desiredTop = caretTop + lineHeight + 4 - parent.top;
  const maxTop = viewport.height - margin - size.height - parent.top;
  const top = Math.max(margin - parent.top, Math.min(desiredTop, maxTop));
  return { left, bottom: null, top, flipped: true };
}

/** Stable identity for one trigger, used to keep Escape dismissed. */
export function triggerKey(trigger: ComposerTrigger): string {
  return `${trigger.kind}:${trigger.start}`;
}

/**
 * Geometry for the phone sheet, in the coordinates its containing block uses.
 *
 * The sheet grows upwards from the composer, but only inside what the person
 * can actually see: when the keyboard is open the composer can itself sit
 * below the visible viewport, and a sheet pinned to its top would put rows
 * under the keyboard. So its bottom edge is whichever is higher, the
 * composer's top or the visible viewport's bottom. When that leaves less room
 * than the header plus one row, the sheet covers the composer inside the
 * visible viewport instead of collapsing to nothing; it is a modal with Done.
 */
export type PhoneSheetGeometry = {
  /** Inline `bottom`, measured from the containing block's bottom edge. */
  bottom: number;
  /** Inline `max-height`. */
  maxHeight: number;
  /** True when the sheet covers the composer because nothing fits above it. */
  coversComposer: boolean;
};

/**
 * The smallest useful sheet: its header (about 36 px), one 44 px row and the
 * sheet's 16 px padding. Below this there is no room to open a list, so the
 * sheet covers the composer instead.
 */
export const PHONE_SHEET_MIN_HEIGHT = 96;

export function phoneSheetGeometry({
  composerTop,
  containerBottom,
  viewportTop = 0,
  viewportHeight,
  gap = 8,
  minHeight = PHONE_SHEET_MIN_HEIGHT,
}: {
  /** Top of the composer, in viewport coordinates. */
  composerTop: number;
  /** Bottom of the sheet's containing block, in viewport coordinates. */
  containerBottom: number;
  /** `visualViewport.offsetTop`. */
  viewportTop?: number;
  /** `visualViewport.height`. */
  viewportHeight: number;
  gap?: number;
  minHeight?: number;
}): PhoneSheetGeometry {
  const viewportBottom = viewportTop + viewportHeight;
  // End at the composer, or at the keyboard when the composer is under it.
  const edge = Math.min(composerTop, viewportBottom) - gap;
  const available = edge - viewportTop - gap;
  if (available < minHeight) {
    return {
      bottom: containerBottom - viewportBottom,
      maxHeight: viewportHeight,
      coversComposer: true,
    };
  }
  return { bottom: containerBottom - edge, maxHeight: available, coversComposer: false };
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
 * Insert the `@` the button adds, separated from the words on either side.
 *
 * A mention glued to the text before it never opens (it looks like an email
 * address), and one glued to the word after it leaves the following word
 * inside the query. Both sides get a space when they need one; the caret
 * stays directly after the `@` so the menu opens on an empty token.
 */
export function insertMentionToken(
  value: string,
  startInput: number,
  endInput: number = startInput,
): { value: string; caret: number } {
  const start = Math.max(0, Math.min(startInput, value.length));
  const end = Math.max(start, Math.min(endInput, value.length));
  const lead = start > 0 && !isWhitespace(value[start - 1]) ? " " : "";
  const trail = end < value.length && !isWhitespace(value[end]) ? " " : "";
  return {
    value: `${value.slice(0, start)}${lead}@${trail}${value.slice(end)}`,
    caret: start + lead.length + 1,
  };
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
