/**
 * Presentation helpers for pairing scopes and the typed short code.
 *
 * Which scopes a device holds, and whether a code is valid, are decided in
 * Rust (`h4rn-connect`). This file only names the five device scopes for the UI
 * and formats the code for reading and typing. A guest link carries a token and
 * no device scope, so pairing never offers guest access (ADR-044).
 */

import type { PairScope } from "@/gen";
import { formatList, i18n } from "@/lib/i18n";

/** A scope's words come from `scopeLabel` and `scopeSub`. */
export type ScopeOption = {
  id: PairScope;
  /** `view` is always held. */
  locked: boolean;
};

export const SCOPE_OPTIONS: readonly ScopeOption[] = [
  { id: "view", locked: true },
  { id: "approve", locked: false },
  { id: "terminal", locked: false },
  { id: "files", locked: false },
  { id: "admin", locked: false },
];

const ALL_SCOPES: readonly PairScope[] = SCOPE_OPTIONS.map((option) => option.id);

/** A scope's name in the active language. */
export function scopeLabel(scope: PairScope): string {
  return i18n.t(`settings:devices.scopes.${scope}.label`);
}

/** A scope's second line in the active language, or "" when it has none. */
export function scopeSub(scope: PairScope): string {
  return i18n.t(`settings:devices.scopes.${scope}.sub`);
}

/** What the Settings checkboxes start as: view plus approve. */
export const DEFAULT_PAIR_SCOPES: readonly PairScope[] = ["view", "approve"];

/** `view` first, the rest in the order the options are listed, no repeats. */
export function orderedScopes(scopes: Iterable<PairScope>): PairScope[] {
  const held = new Set<PairScope>(scopes);
  held.add("view");
  return ALL_SCOPES.filter((scope) => held.has(scope));
}

/** Toggles one scope. `view` cannot be removed. */
export function toggleScope(scopes: readonly PairScope[], scope: PairScope): PairScope[] {
  if (scope === "view") return orderedScopes(scopes);
  const next = new Set(scopes);
  if (!next.delete(scope)) next.add(scope);
  return orderedScopes(next);
}

/** The Scopes column: "All", or the held scopes by name. */
export function scopesLabel(scopes: readonly PairScope[] | undefined): string {
  if (!scopes || scopes.length === 0 || scopes.length === ALL_SCOPES.length) {
    return i18n.t("settings:devices.scopes.all");
  }
  const names = scopes
    .filter((scope) => scope !== "view")
    .map((scope) => (ALL_SCOPES.includes(scope) ? scopeLabel(scope) : scope));
  return names.length === 0
    ? i18n.t("settings:devices.scopes.viewOnly")
    : formatList(names, "conjunction", "narrow");
}

/** Shows an 8-character code as `ABCD-EFGH`. */
export function formatShortCode(code: string): string {
  const clean = code.replace(/[^0-9A-Za-z]/g, "").toUpperCase();
  return clean.length > 4 ? `${clean.slice(0, 4)}-${clean.slice(4, 8)}` : clean;
}

/**
 * Masks what a person types into the code field: upper-case, hyphen after
 * four, at most eight characters. The server does the real validation.
 */
export function maskShortCode(input: string): string {
  return formatShortCode(input.slice(0, 12));
}

/** Whether the typed code has the 8 characters the server expects. */
export function isCompleteShortCode(input: string): boolean {
  return input.replace(/[^0-9A-Za-z]/g, "").length === 8;
}

/** Narrows a string from a link or the wire to a scope this build knows. */
export function isPairScope(value: string): value is PairScope {
  return (ALL_SCOPES as readonly string[]).includes(value);
}
