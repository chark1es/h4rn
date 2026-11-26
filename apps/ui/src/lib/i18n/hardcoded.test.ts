import { readFileSync } from "node:fs";
import { join } from "node:path";
import ts from "typescript";
import { describe, expect, it } from "vitest";

/**
 * Converted files must not grow new hard-coded, user-facing English.
 *
 * Each later area PR adds its files to `CONVERTED`. The check reads the
 * source with the TypeScript parser and flags:
 *   - JSX text with letters in it;
 *   - string or template literals rendered as a JSX child, or given to an
 *     attribute a person reads (`aria-label`, `title`, `placeholder`, …);
 *   - string literals in an object's `title`, `description`, `label`,
 *     `hint`, `sub`, `subtitle` or `message` (toasts, picker options, error
 *     objects), or in a store field named `error`, `…Error` or `notice`;
 *   - template literals rendered the same way that interpolate a value read
 *     from a store as is (`const busy = useRemote((s) => s.busy)` then
 *     `` `${busy}…` ``): a store keeps kinds, and the component translates
 *     them. A field of a store value (`${error.hint}`, a daemon error's own
 *     text) is let through;
 *   - lists joined with a hard-coded `", "`: use `formatList`, which joins
 *     the way the active language does ("a、b、c" in Chinese and Japanese).
 *     A join that feeds data rather than display (an editable field's value,
 *     a request body) opts out with `// i18n-ignore-next-line: <why>` on the
 *     line above it. The reason is required, and the escape covers only that
 *     one join, never other text.
 * Values that are not prose are let through: URLs, paths, e-mail addresses,
 * identifiers such as `NAME` or `gpt-5.4-mini`, and the product names below.
 */
const CONVERTED = [
  "src/components/inbox/PhoneInbox.tsx",
  "src/components/inbox/StateTabs.tsx",
  "src/components/inbox/ThreadRow.tsx",
  "src/components/pages/SettingsPage.tsx",
  "src/components/shell/Sidebar.tsx",
  "src/components/shell/hostStatus.ts",
  "src/features/access/AccessSettings.tsx",
  "src/features/forge/ForgeSettings.tsx",
  "src/features/guest/GuestSettings.tsx",
  "src/features/harnesses/HarnessSettings.tsx",
  "src/features/harnesses/acpForm.ts",
  "src/features/issues/LinearSettings.tsx",
  "src/features/projects/SidebarProjects.tsx",
  "src/features/projects/store.ts",
  "src/features/proxy/ProxySettings.tsx",
  "src/features/remote/ConnectSignIn.tsx",
  "src/features/remote/LocalMachineAction.tsx",
  "src/features/remote/NetworkAccess.tsx",
  "src/features/remote/SettingsRemote.tsx",
  "src/features/remote/pairingScopes.ts",
  "src/features/schedules/SchedulesSettings.tsx",
  "src/features/schedules/scheduleForm.ts",
  "src/features/source-control/SourceControlSettings.tsx",
  "src/features/text-generation/TextGenerationSettings.tsx",
  "src/features/threads/ThreadSettings.tsx",
  "src/features/updates/AboutSettings.tsx",
  "src/features/usage/AutoResumeSettings.tsx",
  "src/features/worktrees/WorktreeSettings.tsx",
  "src/features/worktrees/worktreeStore.ts",
  "src/lib/settings.ts",
];

/** Names that read the same in every language. */
const PRODUCT_NAMES = new Set([
  "CLIProxyAPI",
  "h4rn-connect",
  "Tailscale",
  "Anthropic",
  "Cron",
  "Linear",
]);

const READ_ATTRIBUTES = new Set([
  "aria-label",
  "aria-description",
  "title",
  "placeholder",
  "alt",
  "label",
  "confirmLabel",
  "description",
  "hint",
  "sub",
]);

const READ_PROPERTIES = new Set([
  "title",
  "description",
  "label",
  "hint",
  "sub",
  "subtitle",
  "message",
]);

/** A store field a component shows as is: `error`, `clearError`, `notice`. */
function isReadProperty(name: string): boolean {
  return READ_PROPERTIES.has(name) || /(^e|E)rror$/.test(name) || name === "notice";
}

/** True for text a person reads as words rather than a value or a name. */
function isProse(text: string): boolean {
  const value = text.trim();
  if (!/[A-Za-z]/.test(value)) return false;
  if (PRODUCT_NAMES.has(value)) return false;
  if (/^(https?:|wss?:|mailto:)/.test(value)) return false;
  if (/^[\w.+-]+@[\w.-]+$/.test(value)) return false;
  if (/^(~|\.{0,2})\//.test(value)) return false;
  // Identifiers and tokens: no spaces, only code-like characters.
  if (/^[A-Z0-9_]+$/.test(value)) return false;
  if (/^[a-z0-9][a-z0-9._/:-]*$/.test(value)) return false;
  return true;
}

type Finding = { file: string; line: number; text: string };

/** `// i18n-ignore-next-line: <why>`; the reason after the colon is required. */
const IGNORE_NEXT_LINE = /\/\/\s*i18n-ignore-next-line:\s*\S/;

function literalText(node: ts.Node): string | null {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text;
  if (ts.isTemplateExpression(node)) {
    return [node.head.text, ...node.templateSpans.map((span) => span.literal.text)].join(" ");
  }
  return null;
}

/** Walks up through `a ? b : c`, `a ?? b`, `a || b` and parentheses. */
function renderedParent(node: ts.Node): ts.Node {
  let current = node.parent;
  while (
    current &&
    (ts.isParenthesizedExpression(current) ||
      ts.isConditionalExpression(current) ||
      (ts.isBinaryExpression(current) &&
        (current.operatorToken.kind === ts.SyntaxKind.QuestionQuestionToken ||
          current.operatorToken.kind === ts.SyntaxKind.BarBarToken ||
          current.operatorToken.kind === ts.SyntaxKind.AmpersandAmpersandToken)))
  ) {
    // A condition is not rendered: `x === "on" ? a : b` only renders a and b.
    if (ts.isConditionalExpression(current) && current.condition === node) return current;
    node = current;
    current = current.parent;
  }
  return current;
}

/** Names bound to a store selection: `const x = useStore((state) => state.x)`. */
function storeSelections(sourceFile: ts.SourceFile): Set<string> {
  const names = new Set<string>();
  const visit = (node: ts.Node) => {
    if (
      ts.isVariableDeclaration(node) &&
      ts.isIdentifier(node.name) &&
      node.initializer &&
      ts.isCallExpression(node.initializer) &&
      ts.isIdentifier(node.initializer.expression) &&
      /^use[A-Z]/.test(node.initializer.expression.text)
    ) {
      const [selector] = node.initializer.arguments;
      if (selector && (ts.isArrowFunction(selector) || ts.isFunctionExpression(selector))) {
        names.add(node.name.text);
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return names;
}

function findHardcoded(file: string, source: string): Finding[] {
  const sourceFile = ts.createSourceFile(
    file,
    source,
    ts.ScriptTarget.Latest,
    true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );
  const findings: Finding[] = [];
  const selections = storeSelections(sourceFile);
  const lines = source.split("\n");
  const report = (node: ts.Node, text: string) => {
    const { line } = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
    findings.push({ file, line: line + 1, text: text.trim() });
  };
  /** True when the line above the node opts it out, with a reason. */
  const ignored = (node: ts.Node) => {
    const { line } = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
    return line > 0 && IGNORE_NEXT_LINE.test(lines[line - 1] ?? "");
  };

  const visit = (node: ts.Node) => {
    if (
      ts.isCallExpression(node) &&
      ts.isPropertyAccessExpression(node.expression) &&
      node.expression.name.text === "join" &&
      node.arguments.length === 1 &&
      ts.isStringLiteralLike(node.arguments[0] as ts.Expression) &&
      /^\s*,\s+$/.test((node.arguments[0] as ts.StringLiteralLike).text) &&
      !ignored(node)
    ) {
      report(node, node.getText(sourceFile));
    }
    if (ts.isJsxText(node)) {
      if (isProse(node.text)) report(node, node.text);
    } else {
      const text = literalText(node);
      const storeText =
        ts.isTemplateExpression(node) &&
        node.templateSpans.some(
          (span) => ts.isIdentifier(span.expression) && selections.has(span.expression.text),
        );
      if (text !== null && (isProse(text) || storeText)) {
        const parent = renderedParent(node);
        const inChild =
          parent && ts.isJsxExpression(parent) && !ts.isJsxAttribute(parent.parent ?? parent);
        const inAttribute =
          parent &&
          (ts.isJsxAttribute(parent) ||
            (ts.isJsxExpression(parent) && parent.parent && ts.isJsxAttribute(parent.parent)));
        const attribute = inAttribute
          ? ts.isJsxAttribute(parent)
            ? parent
            : (parent.parent as ts.JsxAttribute)
          : null;
        const property =
          parent && ts.isPropertyAssignment(parent) && ts.isIdentifier(parent.name)
            ? parent.name.text
            : null;
        if (
          inChild ||
          (attribute && READ_ATTRIBUTES.has(attribute.name.getText(sourceFile))) ||
          (property && isReadProperty(property))
        ) {
          report(node, storeText ? node.getText(sourceFile) : text);
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return findings;
}

describe("hard-coded strings in converted files", () => {
  it("recognises prose and lets values through", () => {
    expect(isProse("Save task")).toBe(true);
    expect(isProse("Saved.")).toBe(true);
    for (const value of [
      "~/worktrees",
      "https://github.com",
      "you@example.com",
      "NAME",
      "gpt-5.4-mini",
      "h4rn/",
      "0 9 * * *",
      "—",
      "CLIProxyAPI",
    ]) {
      expect(isProse(value), value).toBe(false);
    }
  });

  it("flags JSX text, rendered literals, read attributes and toast titles", () => {
    const sample = [
      "export function A({ on }: { on: boolean }) {",
      "  pushToast({ title: 'Added the account.' });",
      "  patch({ clearError: 'Refresh it first.', kind: 'Plain words' });",
      "  patch({ settingsError: { code: 'conflict', message: 'Reload them first.' } });",
      "  return (",
      "    <div title='A title' data-kind='plain words are fine here'>",
      "      Hello there",
      "      {on ? 'Turned on' : 'Turned off'}",
      "      {on === 'yes' ? null : null}",
      "    </div>",
      "  );",
      "}",
    ].join("\n");
    const texts = findHardcoded("sample.tsx", sample).map((finding) => finding.text);
    expect(texts).toEqual([
      "Added the account.",
      "Refresh it first.",
      "Reload them first.",
      "A title",
      "Hello there",
      "Turned on",
      "Turned off",
    ]);
  });

  it("flags a rendered template that interpolates a store value", () => {
    const sample = `
export function A({ label }: { label: string }) {
  const busy = useRemote((state) => state.busy);
  const error = useRemote((state) => state.error);
  return (
    <div title={\`\${busy} now\`}>
      {busy ? \`\${busy}…\` : null}
      {\`\${label}…\`}
      {error?.hint ? \` \${error.hint}\` : ""}
      <span data-kind={\`\${busy}\`} />
    </div>
  );
}`;
    const texts = findHardcoded("sample.tsx", sample).map((finding) => finding.text);
    expect(texts).toEqual([`\`\${busy} now\``, `\`\${busy}…\``]);
  });

  it("flags a list joined with a hard-coded comma", () => {
    const sample = [
      "const a = names.join(', ');",
      "const b = keys.join(',');",
      "const c = lines.join('\\n');",
      "const d = parts.join(' · ');",
    ].join("\n");
    const texts = findHardcoded("sample.ts", sample).map((finding) => finding.text);
    expect(texts).toEqual(["names.join(', ')"]);
  });

  it("lets a data join opt out with a reasoned i18n-ignore-next-line", () => {
    const sample = [
      "// i18n-ignore-next-line: the field's value, edited as text",
      "const labels = names.join(', ');",
      "const shown = names.join(', ');",
      "// i18n-ignore-next-line",
      "const noReason = names.join(', ');",
      "// i18n-ignore-next-line: only the join is exempt",
      "pushToast({ title: 'Saved the labels.', body: tags.join(', ') });",
    ].join("\n");
    const texts = findHardcoded("sample.ts", sample).map((finding) => finding.text);
    expect(texts).toEqual(["names.join(', ')", "names.join(', ')", "Saved the labels."]);
  });

  for (const file of CONVERTED) {
    it(`${file} has no hard-coded user-facing text`, () => {
      const source = readFileSync(join(__dirname, "../../..", file), "utf8");
      const findings = findHardcoded(file, source).map(
        (finding) => `${finding.file}:${finding.line} ${JSON.stringify(finding.text)}`,
      );
      expect(findings).toEqual([]);
    });
  }
});
