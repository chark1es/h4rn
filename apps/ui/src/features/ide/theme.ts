import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { EditorView } from "@codemirror/view";
import { tags } from "@lezer/highlight";

/**
 * CodeMirror's skin in h4rn tokens (DESIGN.md).
 *
 * Every colour is a CSS custom property, so light and dark are the same
 * stylesheet: the token layer swaps values when `.dark` is on the root. Nothing
 * here reintroduces a code-editor palette of its own.
 */
export const h4rnEditorTheme = EditorView.theme(
  {
    "&": {
      height: "100%",
      color: "var(--ink)",
      backgroundColor: "var(--paper)",
      fontFamily: "var(--font-code)",
      fontSize: "12.5px",
    },
    ".cm-scroller": {
      fontFamily: "inherit",
      lineHeight: "20px",
      overflow: "auto",
    },
    ".cm-content": {
      padding: "12px 0",
      caretColor: "var(--cue)",
    },
    ".cm-line": {
      padding: "0 20px 0 12px",
    },
    ".cm-cursor, .cm-dropCursor": {
      borderLeftColor: "var(--cue)",
      borderLeftWidth: "2px",
    },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
      backgroundColor: "var(--cue-tint)",
    },
    ".cm-activeLine": {
      backgroundColor: "color-mix(in oklch, var(--panel) 60%, transparent)",
    },
    ".cm-gutters": {
      backgroundColor: "var(--panel)",
      color: "var(--ink-faint)",
      border: "none",
      borderRight: "1px solid var(--line)",
      fontFamily: "var(--font-code)",
      fontSize: "11px",
    },
    ".cm-activeLineGutter": {
      backgroundColor: "var(--cue-tint)",
      color: "var(--cue-ink)",
    },
    ".cm-lineNumbers .cm-gutterElement": {
      minWidth: "44px",
      padding: "0 10px 0 6px",
    },
    ".cm-foldGutter": { color: "var(--ink-faint)" },
    ".cm-selectionMatch": { backgroundColor: "var(--cue-tint)" },
    ".cm-searchMatch": {
      backgroundColor: "var(--cue-tint)",
      outline: "1px solid var(--cue)",
    },
    ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "var(--cue)" },
    ".cm-matchingBracket": {
      backgroundColor: "var(--cue-tint)",
      outline: "1px solid var(--cue)",
    },
    ".cm-tooltip": {
      backgroundColor: "var(--raised)",
      border: "1px solid var(--line)",
      color: "var(--ink)",
    },
    ".cm-panels": {
      backgroundColor: "var(--panel)",
      color: "var(--ink-muted)",
    },
    ".cm-placeholder": { color: "var(--ink-faint)" },
  },
  { dark: false },
);

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.modifier, tags.operatorKeyword], color: "var(--cue-ink)" },
  { tag: [tags.string, tags.special(tags.string)], color: "var(--standby-ink)" },
  { tag: [tags.number, tags.bool, tags.null], color: "var(--prompt)" },
  {
    tag: [tags.comment, tags.lineComment, tags.blockComment],
    color: "var(--ink-faint)",
    fontStyle: "italic",
  },
  { tag: [tags.typeName, tags.className, tags.namespace], color: "var(--prompt)" },
  {
    tag: [tags.function(tags.variableName), tags.function(tags.propertyName)],
    color: "var(--cue-ink)",
  },
  { tag: [tags.propertyName, tags.attributeName], color: "var(--ink)" },
  { tag: [tags.variableName], color: "var(--ink)" },
  { tag: [tags.tagName], color: "var(--cue-ink)" },
  { tag: [tags.attributeValue], color: "var(--standby-ink)" },
  { tag: [tags.heading], color: "var(--cue-ink)", fontWeight: "600" },
  { tag: [tags.strong], fontWeight: "600" },
  { tag: [tags.emphasis], fontStyle: "italic" },
  { tag: [tags.link, tags.url], color: "var(--prompt)", textDecoration: "underline" },
  { tag: [tags.invalid], color: "var(--strike)" },
]);

export const h4rnHighlighting = syntaxHighlighting(highlight);
