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
 *     one join, never other text;
 *   - prose the client writes into an error (`throw new Error("…")`): its
 *     message reaches a store's `error` or a toast as is, so it is
 *     translated where it is thrown. An internal check that can only fail
 *     on a bug (a reply of the wrong type) opts out the same way, with
 *     `// i18n-ignore-next-line: <why>` above the line that builds the error.
 * Values that are not prose are let through: URLs, paths, e-mail addresses,
 * identifiers such as `NAME` or `gpt-5.4-mini`, and the product names below.
 */
const CONVERTED = [
  "src/features/remote/remoteStore.ts",
  "src/features/remote/QrCode.tsx",
  "src/components/shell/PhoneNav.tsx",
  "src/components/pages/PairingPage.tsx",
  "src/components/hosts/HostsPane.tsx",
  "src/components/agent/AgentGlyph.tsx",
  "src/components/dialogs/CommandPalette.tsx",
  "src/components/dialogs/ShortcutsDialog.tsx",
  "src/components/inbox/PhoneInbox.tsx",
  "src/components/inbox/StateTabs.tsx",
  "src/components/inbox/ThreadRow.tsx",
  "src/components/pages/SettingsPage.tsx",
  "src/components/panel/ChangesSection.tsx",
  "src/components/panel/FileDiff.tsx",
  "src/components/panel/PhonePanelSheet.tsx",
  "src/components/panel/RightPanel.tsx",
  "src/components/panel/usePanelChanges.ts",
  "src/components/shell/Sidebar.tsx",
  "src/components/shell/hostStatus.ts",
  "src/components/shell/snoozeWake.ts",
  "src/components/thread/ApprovalCard.tsx",
  "src/components/thread/Composer.tsx",
  "src/components/thread/ComposerMenu.tsx",
  "src/components/thread/GuestRequestCard.tsx",
  "src/components/thread/HarnessSwitch.tsx",
  "src/components/thread/Markdown.tsx",
  "src/components/thread/PhoneApproval.tsx",
  "src/components/thread/QueuedTurn.tsx",
  "src/components/thread/ThreadControls.tsx",
  "src/components/thread/ThreadForkLine.tsx",
  "src/components/thread/ThreadHeader.tsx",
  "src/components/thread/ThreadPane.tsx",
  "src/components/thread/ThreadStream.tsx",
  "src/components/thread/ToolCard.tsx",
  "src/components/thread/TurnBlock.tsx",
  "src/features/access/AccessSettings.tsx",
  "src/features/attachments/AttachmentTray.tsx",
  "src/features/attachments/MessageAttachments.tsx",
  "src/features/attachments/upload.ts",
  "src/features/attachments/use-attachments.ts",
  "src/features/attachments/use-blob.ts",
  "src/features/changes/ChangeSetCard.tsx",
  "src/features/changes/store.ts",
  "src/features/composer/workspaceFiles.ts",
  "src/features/forge/ForgeSettings.tsx",
  "src/features/forge/LinkPullRequestDialog.tsx",
  "src/features/forge/PullRequestsPage.tsx",
  "src/features/forge/ThreadPullRequestBadge.tsx",
  "src/features/forge/store.ts",
  "src/features/guest/GuestSettings.tsx",
  "src/features/harnesses/HarnessSettings.tsx",
  "src/features/harnesses/acpForm.ts",
  "src/features/issues/IssuesPage.tsx",
  "src/features/issues/LinearSettings.tsx",
  "src/features/issues/store.ts",
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
  "src/features/subagents/SubagentsPanel.tsx",
  "src/features/subagents/subagentStore.ts",
  "src/features/terminal/TerminalPanel.tsx",
  "src/features/terminal/TerminalView.tsx",
  "src/features/terminal/terminal-controller.ts",
  "src/features/text-generation/TextGenerationSettings.tsx",
  "src/features/threads/ThreadSettings.tsx",
  "src/features/updates/AboutSettings.tsx",
  "src/features/usage/AutoResumeSettings.tsx",
  "src/features/worktrees/WorktreeSettings.tsx",
  "src/features/worktrees/worktreeStore.ts",
  "src/lib/agent.ts",
  "src/lib/composerCommands.ts",
  "src/lib/keybindings.ts",
  "src/lib/settings.ts",
  "src/lib/threadControls.ts",
  "src/lib/turn.ts",
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

/** `new Error(…)`, `Error(…)`, `new TypeError(…)` and the like. */
function isErrorConstruction(
  node: ts.Node | undefined,
): node is ts.NewExpression | ts.CallExpression {
  return (
    !!node &&
    (ts.isNewExpression(node) || ts.isCallExpression(node)) &&
    ts.isIdentifier(node.expression) &&
    /Error$/.test(node.expression.text)
  );
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
        const thrown = isErrorConstruction(parent) && !ignored(parent) && !ignored(node);
        if (
          inChild ||
          thrown ||
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

  it("flags prose the client writes into an error", () => {
    const sample = [
      "if (!a) throw new Error('This shell cannot open a sign-in window.');",
      `if (!b) throw new Error(\`\${name} has no key yet. Run it again.\`);`,
      "if (!c) throw new TypeError(on ? 'Turned on wrongly' : 'off');",
      "if (!d) throw Error('Something broke here');",
      "if (!e) throw new Error(i18n.t('settings:connect.errors.noSignInWindow'));",
      `if (!f) throw new Error(\`expected_\${type}\`);`,
      "if (!g) throw new Error(message);",
      "const h = new ErrorBoundary('Not an error at all');",
    ].join("\n");
    const texts = findHardcoded("sample.ts", sample).map((finding) => finding.text);
    expect(texts).toEqual([
      "This shell cannot open a sign-in window.",
      "has no key yet. Run it again.",
      "Turned on wrongly",
      "Something broke here",
    ]);
  });

  it("lets an internal check's error opt out with a reasoned i18n-ignore-next-line", () => {
    const sample = [
      "// i18n-ignore-next-line: a reply of the wrong type is a bug, not a state",
      `throw new Error(\`expected \${type}, got \${response.type}\`);`,
      "// i18n-ignore-next-line",
      "throw new Error('No reason given here');",
      "// i18n-ignore-next-line: a bug",
      "throw new Error(",
      "  'Multi-line but exempt',",
      ");",
      "throw new Error('Still flagged here');",
    ].join("\n");
    const texts = findHardcoded("sample.ts", sample).map((finding) => finding.text);
    expect(texts).toEqual(["No reason given here", "Still flagged here"]);
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
