/**
 * A tiny unified diff for the IDE review pane.
 *
 * The daemon sends both sides as text (`FsDiff`), so the client only needs line
 * alignment. This is an LCS over lines with an explicit cap: past the cap it
 * degrades to "all before removed, all after added" rather than freezing the
 * pane on a huge file.
 */
export type DiffLineKind = "context" | "added" | "removed";

export type DiffLine = {
  kind: DiffLineKind;
  text: string;
  /** One-based line number on the old side, when present. */
  oldLine: number | null;
  /** One-based line number on the new side, when present. */
  newLine: number | null;
};

export type DiffStats = {
  added: number;
  removed: number;
};

const LCS_LIMIT = 2_000_000;

export function computeDiff(before: string, after: string): DiffLine[] {
  const left = before.length === 0 ? [] : before.split("\n");
  const right = after.length === 0 ? [] : after.split("\n");
  // A trailing newline produces a final empty element; it is not a real line.
  if (left.at(-1) === "") left.pop();
  if (right.at(-1) === "") right.pop();

  if (left.length * right.length > LCS_LIMIT) {
    return fallback(left, right);
  }

  // LCS table, walked backwards to emit a stable top-to-bottom diff.
  const rows = left.length + 1;
  const cols = right.length + 1;
  const table = new Uint32Array(rows * cols);
  for (let i = left.length - 1; i >= 0; i -= 1) {
    for (let j = right.length - 1; j >= 0; j -= 1) {
      table[i * cols + j] =
        left[i] === right[j]
          ? (table[(i + 1) * cols + j + 1] ?? 0) + 1
          : Math.max(table[(i + 1) * cols + j] ?? 0, table[i * cols + j + 1] ?? 0);
    }
  }

  const lines: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < left.length && j < right.length) {
    if (left[i] === right[j]) {
      lines.push({ kind: "context", text: left[i] ?? "", oldLine: i + 1, newLine: j + 1 });
      i += 1;
      j += 1;
    } else if ((table[(i + 1) * cols + j] ?? 0) >= (table[i * cols + j + 1] ?? 0)) {
      lines.push({ kind: "removed", text: left[i] ?? "", oldLine: i + 1, newLine: null });
      i += 1;
    } else {
      lines.push({ kind: "added", text: right[j] ?? "", oldLine: null, newLine: j + 1 });
      j += 1;
    }
  }
  while (i < left.length) {
    lines.push({ kind: "removed", text: left[i] ?? "", oldLine: i + 1, newLine: null });
    i += 1;
  }
  while (j < right.length) {
    lines.push({ kind: "added", text: right[j] ?? "", oldLine: null, newLine: j + 1 });
    j += 1;
  }
  return lines;
}

function fallback(left: string[], right: string[]): DiffLine[] {
  return [
    ...left.map((text, index) => ({
      kind: "removed" as const,
      text,
      oldLine: index + 1,
      newLine: null,
    })),
    ...right.map((text, index) => ({
      kind: "added" as const,
      text,
      oldLine: null,
      newLine: index + 1,
    })),
  ];
}

export function diffStats(lines: DiffLine[]): DiffStats {
  let added = 0;
  let removed = 0;
  for (const line of lines) {
    if (line.kind === "added") added += 1;
    else if (line.kind === "removed") removed += 1;
  }
  return { added, removed };
}

/**
 * Collapses long runs of unchanged lines into a single hunk marker.
 *
 * `context` lines around a change are kept so a reviewer can see what the
 * change sits inside. Lines outside every hunk become skip markers
 * that the pane renders as an expandable summary.
 */
export type DiffRow = DiffLine | { kind: "skip"; count: number };

export function collapseDiff(lines: DiffLine[], context = 3): DiffRow[] {
  const keep = new Set<number>();
  for (let index = 0; index < lines.length; index += 1) {
    if (lines[index]?.kind === "context") continue;
    for (let offset = index - context; offset <= index + context; offset += 1) {
      if (offset >= 0 && offset < lines.length) keep.add(offset);
    }
  }
  const rows: DiffRow[] = [];
  let skipped = 0;
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (!line) continue;
    if (keep.has(index)) {
      if (skipped > 0) {
        rows.push({ kind: "skip", count: skipped });
        skipped = 0;
      }
      rows.push(line);
    } else {
      skipped += 1;
    }
  }
  if (skipped > 0) rows.push({ kind: "skip", count: skipped });
  return rows;
}
