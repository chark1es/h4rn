import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * Tailwind v4's `outline-none` sets `--tw-outline-style: none`, and
 * `focus-visible:outline-2` only sets the width, reading the style from that
 * variable. A class string with both draws no focus ring unless it also says
 * `focus-visible:outline-solid`.
 */
const SRC = join(import.meta.dirname, "../..");

function sources(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return sources(path);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [path] : [];
  });
}

describe("focus rings", () => {
  it("restore the outline style wherever outline-none meets focus-visible:outline-<n>", () => {
    const offenders = sources(SRC).flatMap((file) =>
      readFileSync(file, "utf8")
        .split("\n")
        .flatMap((line, index) =>
          /focus-visible:outline-\d/.test(line) && !line.includes("focus-visible:outline-solid")
            ? [`${relative(SRC, file)}:${index + 1}`]
            : [],
        ),
    );
    expect(offenders).toEqual([]);
  });
});
