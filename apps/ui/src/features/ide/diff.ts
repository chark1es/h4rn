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
 * Collapses long runs of unchanged lines and labels each run the way a unified
 * diff does: `@@ -oldStart,oldCount +newStart,newCount @@ context`.
 *
 * `context` lines around a change are kept so a reviewer can see what the
 * change sits inside. Lines outside every hunk collapse into the gap between
 * two headers, which is what the reference documents show.
 */
export type DiffHunk = {
  kind: "hunk";
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  /** The nearest preceding context line, used as the trailing @@ label. */
  context: string | null;
};

export type DiffRow = DiffLine | DiffHunk;

/** The nearest non-blank context line before `start`, trimmed for the header. */
function hunkContext(lines: DiffLine[], start: number): string | null {
  for (let index = start - 1; index >= 0; index -= 1) {
    const line = lines[index];
    if (line?.kind !== "context") continue;
    const text = line.text.trim();
    if (text.length > 0) return text.length > 72 ? `${text.slice(0, 71)}…` : text;
  }
  return null;
}

export function collapseDiff(lines: DiffLine[], context = 3): DiffRow[] {
  const keep = new Set<number>();
  for (let index = 0; index < lines.length; index += 1) {
    if (lines[index]?.kind === "context") continue;
    for (let offset = index - context; offset <= index + context; offset += 1) {
      if (offset >= 0 && offset < lines.length) keep.add(offset);
    }
  }

  const runs: number[][] = [];
  let run: number[] = [];
  for (let index = 0; index < lines.length; index += 1) {
    if (keep.has(index)) run.push(index);
    else if (run.length > 0) {
      runs.push(run);
      run = [];
    }
  }
  if (run.length > 0) runs.push(run);

  const rows: DiffRow[] = [];
  let lastOld = 0;
  let lastNew = 0;
  let cursor = 0;
  for (const indices of runs) {
    const firstIndex = indices[0] ?? 0;
    // Track the line numbers of the collapsed gap so the header starts where
    // the previous hunk stopped.
    for (let index = cursor; index < firstIndex; index += 1) {
      const line = lines[index];
      if (line?.oldLine != null) lastOld = line.oldLine;
      if (line?.newLine != null) lastNew = line.newLine;
    }
    const first = lines[firstIndex];
    let oldCount = 0;
    let newCount = 0;
    for (const index of indices) {
      const line = lines[index];
      if (line?.kind !== "added") oldCount += 1;
      if (line?.kind !== "removed") newCount += 1;
    }
    rows.push({
      kind: "hunk",
      oldStart: first?.oldLine ?? lastOld + 1,
      oldCount,
      newStart: first?.newLine ?? lastNew + 1,
      newCount,
      context: hunkContext(lines, firstIndex),
    });
    for (const index of indices) {
      const line = lines[index];
      if (!line) continue;
      rows.push(line);
      if (line.oldLine != null) lastOld = line.oldLine;
      if (line.newLine != null) lastNew = line.newLine;
    }
    cursor = (indices.at(-1) ?? 0) + 1;
  }
  return rows;
}
