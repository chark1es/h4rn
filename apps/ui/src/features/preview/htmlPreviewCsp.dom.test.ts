// @vitest-environment jsdom
import { describe, expect, it } from "vitest";

import { buildPreviewDocument, PREVIEW_CSP_META } from "./htmlPreview";

/**
 * Parses a prepared document the way a browser would.
 *
 * The CSP meta must be the first element in the parsed document, ahead of any
 * workspace markup, whatever malformed comment prefix the file starts with.
 */
function parse(srcdoc: string): Document {
  return new DOMParser().parseFromString(srcdoc, "text/html");
}

function policies(doc: Document): Element[] {
  return [...doc.querySelectorAll('meta[http-equiv="Content-Security-Policy" i]')];
}

describe("CSP placement", () => {
  const prefixes = [
    { name: "an empty comment", prefix: "<!-->", keepsMarkup: true },
    { name: "an empty comment closed with a dash", prefix: "<!--->", keepsMarkup: true },
    { name: "a comment closed with --!>", prefix: "<!-- x --!>", keepsMarkup: true },
    // An unclosed comment swallows the rest of the file; the policy is still
    // ahead of it, which is what matters.
    { name: "an unclosed comment", prefix: "<!-- never closed", keepsMarkup: false },
    { name: "a processing-instruction lookalike", prefix: "<?xml?>", keepsMarkup: true },
  ];

  for (const { name, prefix, keepsMarkup } of prefixes) {
    it(`comes before the workspace document after ${name}`, () => {
      const built = buildPreviewDocument({
        // The trailing comment and doctype make the old "scan comments to find
        // the doctype" path skip over the workspace paragraph and land the
        // policy after it.
        html: `${prefix}<p id="workspace">hello</p><!-- trailing --><!doctype html><p>tail</p>`,
        docPath: "index.html",
        assets: {},
        token: "t1",
      });
      const doc = parse(built.srcdoc);
      expect(doc.compatMode).toBe("CSS1Compat");

      const meta = policies(doc)[0];
      expect(meta).toBeDefined();
      const workspace = doc.querySelector("#workspace");
      if (keepsMarkup) {
        expect(workspace).not.toBeNull();
        // DOCUMENT_POSITION_FOLLOWING: the workspace element comes after the
        // policy in document order.
        expect(
          meta &&
            workspace &&
            meta.compareDocumentPosition(workspace) & Node.DOCUMENT_POSITION_FOLLOWING,
        ).toBeTruthy();
      }

      // No element at all is parsed ahead of the policy.
      const elements = [...doc.querySelectorAll("*")].filter(
        (element) => !["HTML", "HEAD", "BODY"].includes(element.tagName),
      );
      expect(elements[0]).toBe(meta);
    });
  }

  it("keeps the policy as the first bytes of the document", () => {
    const built = buildPreviewDocument({
      html: "<!--> <p>hello</p>",
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    expect(built.srcdoc.startsWith(`<!doctype html>${PREVIEW_CSP_META}`)).toBe(true);
  });
});
