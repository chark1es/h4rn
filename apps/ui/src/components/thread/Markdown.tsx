import { CheckIcon, CopyIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

import { previewLinkOpens, previewPathFromLink } from "@/features/preview/htmlPreview";
import { openPreview, resolvedTheme, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function languageOf(className: string | undefined): string | null {
  const match = /language-([\w+-]+)/.exec(className ?? "");
  return match?.[1] ?? null;
}

function CodeBlock({ code, language }: { code: string; language: string | null }) {
  const theme = useApp((state) => state.ui.theme);
  const resolved = resolvedTheme(theme);
  const [html, setHtml] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!language) {
      setHtml(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const { codeToHtml } = await import("shiki");
        const output = await codeToHtml(code, {
          lang: language,
          theme: resolved === "dark" ? "github-dark-default" : "github-light-default",
        });
        if (!cancelled) setHtml(output);
      } catch {
        if (!cancelled) setHtml(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [code, language, resolved]);

  const copy = () => {
    void navigator.clipboard?.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1_400);
    });
  };

  return (
    <div className="group/code relative my-2 overflow-hidden rounded-md border border-line bg-bg-sunk">
      <div className="flex h-7 items-center gap-2 border-b border-line px-2.5">
        <span className="text-caption text-fg-3">{language ?? "text"}</span>
        <button
          type="button"
          className="ml-auto flex items-center gap-1 text-caption text-fg-3 opacity-0 transition-opacity duration-[var(--dur-instant)] hover:text-fg focus-visible:opacity-100 group-hover/code:opacity-100"
          onClick={copy}
          aria-label="Copy code"
        >
          {copied ? <CheckIcon className="size-3" /> : <CopyIcon className="size-3" />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {html ? (
        <div
          className="overflow-x-auto text-code [&>pre]:p-2.5"
          // Shiki escapes the source before wrapping it in spans.
          // biome-ignore lint/security/noDangerouslySetInnerHtml: highlighted markup is generated from text
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="overflow-x-auto p-2.5 font-mono text-code text-fg">
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
}

/**
 * Markdown components for one message.
 *
 * Only thread messages intercept relative `.html` links; PR bodies and comments
 * keep the behaviour of `origin/main`. A modifier-click (or a non-left button)
 * is left to the browser, so ⌘-click still opens a new tab.
 */
function markdownComponents(interceptHtmlLinks: boolean): Components {
  return {
    a: ({ children, href }) => {
      const preview = interceptHtmlLinks && href ? previewPathFromLink(href) : null;
      return (
        <a
          href={href}
          target="_blank"
          rel="noreferrer noopener"
          onClick={
            preview
              ? (event) => {
                  if (!previewLinkOpens(event)) return;
                  event.preventDefault();
                  openPreview(preview);
                }
              : undefined
          }
        >
          {children}
        </a>
      );
    },
    ...baseComponents,
  };
}

const baseComponents: Components = {
  code: ({ className, children }) => {
    const text = String(children ?? "").replace(/\n$/, "");
    const language = languageOf(className);
    if (!language && !text.includes("\n")) {
      return <code>{text}</code>;
    }
    return <CodeBlock code={text} language={language} />;
  },
  pre: ({ children }) => <>{children}</>,
};

/** Markdown for one message. Highlighting loads only when code appears. */
export function Markdown({
  text,
  className,
  interceptHtmlLinks = false,
}: {
  text: string;
  className?: string;
  /** Thread messages open relative `.html` links in the Preview tab. */
  interceptHtmlLinks?: boolean;
}) {
  const memo = useMemo(() => text, [text]);
  const components = useMemo(() => markdownComponents(interceptHtmlLinks), [interceptHtmlLinks]);
  return (
    <div className={cn("stream-prose", className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {memo}
      </ReactMarkdown>
    </div>
  );
}
