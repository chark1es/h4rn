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
      color: "var(--fg)",
      backgroundColor: "var(--bg-sunk)",
      fontFamily: "var(--mono)",
      fontSize: "12.5px",
    },
    ".cm-scroller": {
      fontFamily: "inherit",
      lineHeight: "20px",
      overflow: "auto",
    },
    ".cm-content": {
      padding: "8px 0",
      caretColor: "var(--accent)",
    },
    ".cm-line": {
      padding: "0 16px 0 6px",
    },
    ".cm-cursor, .cm-dropCursor": {
      borderLeftColor: "var(--accent)",
      borderLeftWidth: "2px",
    },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
      backgroundColor: "var(--sel)",
    },
    ".cm-activeLine": {
      backgroundColor: "color-mix(in oklch, var(--hover) 55%, transparent)",
    },
    ".cm-gutters": {
      backgroundColor: "transparent",
      color: "var(--fg-3)",
      border: "none",
      fontFamily: "var(--mono)",
      fontSize: "12.5px",
    },
    ".cm-activeLineGutter": {
      backgroundColor: "transparent",
      color: "var(--fg)",
    },
    ".cm-lineNumbers .cm-gutterElement": {
      minWidth: "52px",
      padding: "0 16px 0 6px",
    },
    ".cm-foldGutter": { color: "var(--fg-3)" },
    ".cm-selectionMatch": { backgroundColor: "var(--sel)" },
    ".cm-searchMatch": {
      backgroundColor: "var(--sel)",
      outline: "1px solid var(--accent)",
    },
    ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "var(--accent)" },
    ".cm-matchingBracket": {
      backgroundColor: "var(--sel)",
      outline: "1px solid var(--accent)",
    },
    ".cm-tooltip": {
      backgroundColor: "var(--bg-raised)",
      border: "1px solid var(--line)",
      borderRadius: "8px",
      color: "var(--fg)",
    },
    ".cm-panels": {
      backgroundColor: "var(--bg)",
      color: "var(--fg-2)",
    },
    ".cm-placeholder": { color: "var(--fg-3)" },
  },
  { dark: false },
);

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.modifier, tags.operatorKeyword], color: "var(--syn-kw)" },
  { tag: [tags.string, tags.special(tags.string)], color: "var(--syn-str)" },
  { tag: [tags.number, tags.bool, tags.null], color: "var(--fg-2)" },
  {
    tag: [tags.comment, tags.lineComment, tags.blockComment],
    color: "var(--syn-cm)",
  },
  { tag: [tags.typeName, tags.className, tags.namespace], color: "var(--syn-fn)" },
  {
    tag: [tags.function(tags.variableName), tags.function(tags.propertyName)],
    color: "var(--syn-fn)",
  },
  { tag: [tags.propertyName, tags.attributeName], color: "var(--fg)" },
  { tag: [tags.variableName], color: "var(--fg)" },
  { tag: [tags.tagName], color: "var(--syn-kw)" },
  { tag: [tags.attributeValue], color: "var(--syn-str)" },
  { tag: [tags.heading], color: "var(--syn-fn)", fontWeight: "600" },
  { tag: [tags.strong], fontWeight: "600" },
  { tag: [tags.emphasis], fontStyle: "italic" },
  { tag: [tags.link, tags.url], color: "var(--syn-str)", textDecoration: "underline" },
  { tag: [tags.invalid], color: "var(--danger)" },
]);

export const h4rnHighlighting = syntaxHighlighting(highlight);
