import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { LOCALES, NAMESPACES } from "./index";

/**
 * Every catalogue has exactly the English keys, with no empty values.
 *
 * Plural keys are compared by their base: English has `_one` and `_other`,
 * Chinese and Japanese only `_other`, Spanish `_one`, `_many` and `_other`.
 * Each language must carry exactly the plural forms its own rules use, which
 * `Intl.PluralRules` reports.
 */

type Catalogue = { [key: string]: string | Catalogue };

const LOCALE_DIR = join(__dirname, "locales");
const PLURAL = /_(zero|one|two|few|many|other)$/;

function load(locale: string, namespace: string): Catalogue {
  return JSON.parse(readFileSync(join(LOCALE_DIR, locale, `${namespace}.json`), "utf8"));
}

function flatten(catalogue: Catalogue, prefix = ""): Map<string, string> {
  const out = new Map<string, string>();
  for (const [key, value] of Object.entries(catalogue)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "string") out.set(path, value);
    else for (const [inner, text] of flatten(value, path)) out.set(inner, text);
  }
  return out;
}

/** Base keys, and for each plural base the set of categories present. */
function shape(flat: Map<string, string>) {
  const plain = new Set<string>();
  const plurals = new Map<string, Set<string>>();
  for (const key of flat.keys()) {
    const match = PLURAL.exec(key);
    if (!match) {
      plain.add(key);
      continue;
    }
    const base = key.slice(0, match.index);
    const set = plurals.get(base) ?? new Set<string>();
    set.add(match[1] as string);
    plurals.set(base, set);
  }
  return { plain, plurals };
}

function placeholders(text: string): string[] {
  // `{{count, number}}` names `count`; the format after the comma may differ.
  return [...text.matchAll(/\{\{\s*([\w.]+)\s*(?:,[^}]*)?\}\}/g)]
    .map((match) => match[1] as string)
    .sort();
}

function tags(text: string): string[] {
  return [...text.matchAll(/<\/?([a-z]+)>/g)].map((match) => match[0]).sort();
}

describe("translation catalogues", () => {
  it("ship one folder per supported language and nothing else", () => {
    const folders = readdirSync(LOCALE_DIR).sort();
    expect(folders).toEqual([...LOCALES].sort());
    for (const locale of LOCALES) {
      const files = readdirSync(join(LOCALE_DIR, locale)).sort();
      expect(files, locale).toEqual(NAMESPACES.map((namespace) => `${namespace}.json`).sort());
    }
  });

  for (const namespace of NAMESPACES) {
    const english = flatten(load("en", namespace));
    const englishShape = shape(english);

    for (const locale of LOCALES) {
      describe(`${locale}/${namespace}`, () => {
        const flat = flatten(load(locale, namespace));
        const { plain, plurals } = shape(flat);
        const categories = new Set(new Intl.PluralRules(locale).resolvedOptions().pluralCategories);

        it("has every English key and no extra ones", () => {
          expect([...plain].sort()).toEqual([...englishShape.plain].sort());
          expect([...plurals.keys()].sort()).toEqual([...englishShape.plurals.keys()].sort());
        });

        it("has exactly the plural forms this language uses", () => {
          for (const [base, present] of plurals) {
            expect([...present].sort(), `${base} in ${locale}`).toEqual([...categories].sort());
          }
        });

        it("has no empty values", () => {
          for (const [key, value] of flat) {
            // A deliberately empty second line is allowed only where English
            // is empty too.
            const allowedEmpty = english.get(key) === "" || key === "";
            if (!allowedEmpty) expect(value.trim(), `${locale} ${key}`).not.toBe("");
          }
        });

        // Converted areas format a shown count for the language ("12,000" /
        // "12.000"); Settings predates the rule and is converted separately.
        if (namespace !== "settings") {
          it("formats every count as a number", () => {
            for (const [key, value] of flat) {
              expect(value, `${locale} ${key}`).not.toMatch(/\{\{\s*count\s*\}\}/);
            }
          });
        }

        it("keeps the same placeholders and markup as English", () => {
          for (const [key, value] of flat) {
            const match = PLURAL.exec(key);
            const base = match ? key.slice(0, match.index) : key;
            const source =
              english.get(key) ?? english.get(`${base}_other`) ?? english.get(`${base}_one`);
            expect(source, `${locale} ${key} has an English source`).toBeDefined();
            // `count` may be implied by a plural form ("One thread…").
            const own = placeholders(value).filter((name) => name !== "count");
            const theirs = placeholders(source ?? "").filter((name) => name !== "count");
            expect(own, `${locale} ${key}`).toEqual(theirs);
            expect(tags(value), `${locale} ${key}`).toEqual(tags(source ?? ""));
          }
        });
      });
    }
  }
});
