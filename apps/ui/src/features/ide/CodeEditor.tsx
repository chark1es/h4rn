import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import {
  bracketMatching,
  indentOnInput,
  LanguageDescription,
  type LanguageSupport,
} from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { Compartment, EditorState, Prec } from "@codemirror/state";
import {
  drawSelection,
  dropCursor,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
} from "@codemirror/view";
import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";

import { h4rnEditorTheme, h4rnHighlighting } from "./theme";

export type CursorPosition = { line: number; column: number };

type CodeEditorProps = {
  path: string;
  content: string;
  readOnly: boolean;
  revealLine: number | null;
  revealNonce: number;
  onChange: (content: string) => void;
  onSave: () => void;
  onCursor: (position: CursorPosition) => void;
};

/** Applies one LanguageDescription to the language compartment. */
async function loadLanguage(
  view: EditorView,
  compartment: Compartment,
  path: string,
): Promise<() => void> {
  const name = path.split("/").at(-1) ?? path;
  const description = LanguageDescription.matchFilename(languages, name);
  if (!description) return () => undefined;
  let support: LanguageSupport | null = null;
  try {
    support = await description.load();
  } catch {
    // A missing language package is not an editor failure: plain text works.
    return () => undefined;
  }
  view.dispatch({ effects: compartment.reconfigure(support) });
  return () => undefined;
}

/**
 * The CodeMirror 6 surface.
 *
 * One editor instance, reconfigured per tab: the language loads lazily from
 * `@codemirror/language-data` on the path's extension, so the initial bundle
 * only carries the editor core. The buffer itself lives in the tab state, not
 * here, which keeps saves optimistic and survives remounts.
 */
export function CodeEditor({
  path,
  content,
  readOnly,
  revealLine,
  revealNonce,
  onChange,
  onSave,
  onCursor,
}: CodeEditorProps) {
  const host = useRef<HTMLDivElement | null>(null);
  const view = useRef<EditorView | null>(null);
  const language = useRef(new Compartment());
  const readOnlyCompartment = useRef(new Compartment());
  // The editor's accessible name follows the active language; the compartment
  // swaps it without rebuilding the editor.
  const { t } = useTranslation("ide");
  const label = t("editor");
  const labelCompartment = useRef(new Compartment());
  const labelRef = useRef(label);
  const callbacks = useRef({ onChange, onSave, onCursor });
  callbacks.current = { onChange, onSave, onCursor };
  const contentRef = useRef(content);
  contentRef.current = content;

  useEffect(() => {
    const parent = host.current;
    if (!parent) return;

    const state = EditorState.create({
      doc: "",
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightSpecialChars(),
        history(),
        drawSelection(),
        dropCursor(),
        indentOnInput(),
        bracketMatching(),
        highlightActiveLine(),
        h4rnEditorTheme,
        h4rnHighlighting,
        EditorView.lineWrapping,
        language.current.of([]),
        readOnlyCompartment.current.of(EditorState.readOnly.of(true)),
        Prec.highest(
          keymap.of([
            {
              key: "Mod-s",
              preventDefault: true,
              run: () => {
                callbacks.current.onSave();
                return true;
              },
            },
          ]),
        ),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) callbacks.current.onChange(update.state.doc.toString());
          if (update.selectionSet || update.docChanged) {
            const head = update.state.selection.main.head;
            const line = update.state.doc.lineAt(head);
            callbacks.current.onCursor({
              line: line.number,
              column: head - line.from + 1,
            });
          }
        }),
        labelCompartment.current.of(
          EditorView.contentAttributes.of({ "aria-label": labelRef.current }),
        ),
        EditorView.contentAttributes.of({
          "aria-multiline": "true",
          spellcheck: "false",
          autocapitalize: "off",
        }),
      ],
    });

    const instance = new EditorView({ state, parent });
    view.current = instance;
    return () => {
      instance.destroy();
      view.current = null;
    };
  }, []);

  // Swap the document when the active tab changes. `path` is the identity of
  // the buffer; content sync below handles reloads within the same tab.
  useEffect(() => {
    const instance = view.current;
    if (!instance) return;
    const next = contentRef.current;
    if (instance.state.doc.toString() !== next) {
      instance.dispatch({
        changes: { from: 0, to: instance.state.doc.length, insert: next },
      });
    }
    void loadLanguage(instance, language.current, path);
  }, [path]);

  useEffect(() => {
    const instance = view.current;
    if (!instance) return;
    const current = instance.state.doc.toString();
    if (current === content) return;
    instance.dispatch({ changes: { from: 0, to: current.length, insert: content } });
  }, [content]);

  useEffect(() => {
    view.current?.dispatch({
      effects: readOnlyCompartment.current.reconfigure(EditorState.readOnly.of(readOnly)),
    });
  }, [readOnly]);

  useEffect(() => {
    if (labelRef.current === label) return;
    labelRef.current = label;
    view.current?.dispatch({
      effects: labelCompartment.current.reconfigure(
        EditorView.contentAttributes.of({ "aria-label": label }),
      ),
    });
  }, [label]);

  useEffect(() => {
    const instance = view.current;
    if (!instance || revealLine === null || revealNonce === 0) return;
    const line = Math.max(1, Math.min(revealLine, instance.state.doc.lines));
    const position = instance.state.doc.line(line).from;
    instance.dispatch({
      selection: { anchor: position },
      effects: EditorView.scrollIntoView(position, { y: "center" }),
    });
    instance.focus();
  }, [revealLine, revealNonce]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" data-ide-editor="" />;
}
