import { describe, expect, it } from "vitest";

import {
  buildPreviewDocument,
  injectCsp,
  isHtmlPath,
  PREVIEW_CSP,
  PREVIEW_MAX_TOTAL_BYTES,
  PREVIEW_SANDBOX,
  previewAssetFits,
  previewMimeType,
  previewPathFromLink,
  resolvePreviewAsset,
  scanCssReferences,
  scanPreviewReferences,
} from "./htmlPreview";

describe("HTML preview paths", () => {
  it("only treats .html and .htm as previewable", () => {
    expect(isHtmlPath("reports/index.html")).toBe(true);
    expect(isHtmlPath("REPORT.HTM")).toBe(true);
    expect(isHtmlPath("chart.html?v=2")).toBe(true);
    expect(isHtmlPath("chart.svg")).toBe(false);
    expect(isHtmlPath("index.html.bak")).toBe(false);
  });

  it("resolves relative paths from the document's folder", () => {
    expect(resolvePreviewAsset("reports/index.html", "chart.png")).toBe("reports/chart.png");
    expect(resolvePreviewAsset("reports/index.html", "./assets/chart.png")).toBe(
      "reports/assets/chart.png",
    );
    expect(resolvePreviewAsset("reports/deep/page.html", "../shared.css")).toBe(
      "reports/shared.css",
    );
    expect(resolvePreviewAsset("index.html", "assets/app.css")).toBe("assets/app.css");
    expect(resolvePreviewAsset("index.html", "/assets/app.css")).toBe("assets/app.css");
    expect(resolvePreviewAsset("reports/index.html", "chart.png?cache=1#top")).toBe(
      "reports/chart.png",
    );
    expect(resolvePreviewAsset("reports/index.html", "my%20chart.png")).toBe(
      "reports/my chart.png",
    );
    expect(resolvePreviewAsset("a\\b\\index.html", "chart.png")).toBe("a/b/chart.png");
  });

  it("rejects paths that climb above the workspace root", () => {
    expect(resolvePreviewAsset("index.html", "../outside.png")).toBeNull();
    expect(resolvePreviewAsset("reports/index.html", "../../outside.png")).toBeNull();
    // Climbing back to the root is fine; climbing past it is not.
    expect(resolvePreviewAsset("reports/index.html", "assets/../../outside.png")).toBe(
      "outside.png",
    );
    expect(resolvePreviewAsset("reports/index.html", "assets/../../../outside.png")).toBeNull();
  });

  it("rejects absolute URLs and other schemes", () => {
    expect(resolvePreviewAsset("index.html", "https://example.com/a.png")).toBeNull();
    expect(resolvePreviewAsset("index.html", "//cdn.example.com/a.png")).toBeNull();
    expect(resolvePreviewAsset("index.html", "data:image/png;base64,AAAA")).toBeNull();
    expect(resolvePreviewAsset("index.html", "javascript:alert(1)")).toBeNull();
    expect(resolvePreviewAsset("index.html", "#top")).toBeNull();
  });

  it("turns agent links into workspace paths without accepting escapes", () => {
    expect(previewPathFromLink("reports/chart.html")).toBe("reports/chart.html");
    expect(previewPathFromLink("./chart.htm?x=1")).toBe("chart.htm");
    expect(previewPathFromLink("/reports/chart.html")).toBe("reports/chart.html");
    expect(previewPathFromLink("../secret.html")).toBeNull();
    expect(previewPathFromLink("https://example.com/a.html")).toBeNull();
    expect(previewPathFromLink("chart.png")).toBeNull();
  });
});

describe("HTML preview scanning", () => {
  it("classifies stylesheets, scripts, images and external assets", () => {
    const html = `<!doctype html><html><head>
      <link rel="stylesheet" href="style.css">
      <link rel="icon" href="favicon.png">
      <script src="app.js"></script>
      <script src="https://cdn.example.com/lib.js"></script>
      <style>.hero { background: url(hero.png) }</style>
    </head><body>
      <img src="chart.png">
      <img src="https://cdn.example.com/photo.jpg">
      <img src="../../outside.png">
      <div style="background: url(bg.png)">Panel</div>
    </body></html>`;
    const scan = scanPreviewReferences(html, "reports/index.html");
    expect(scan.refs).toEqual([
      { url: "style.css", path: "reports/style.css", kind: "text" },
      { url: "app.js", path: "reports/app.js", kind: "text" },
      { url: "hero.png", path: "reports/hero.png", kind: "bytes" },
      { url: "chart.png", path: "reports/chart.png", kind: "bytes" },
      { url: "bg.png", path: "reports/bg.png", kind: "bytes" },
    ]);
    expect(scan.external).toEqual([
      "https://cdn.example.com/lib.js",
      "https://cdn.example.com/photo.jpg",
    ]);
    expect(scan.rejected).toEqual(["../../outside.png"]);
  });

  it("does not mistake markup inside script bodies for tags", () => {
    const html = `<script>const template = '<img src="not-an-asset.png">';</script><img src="real.png">`;
    const scan = scanPreviewReferences(html, "index.html");
    expect(scan.refs.map((ref) => ref.path)).toEqual(["real.png"]);
  });

  it("resolves stylesheet url()s from the stylesheet's own folder", () => {
    const css = `@font-face { src: url("../fonts/geist.woff2") } .bg { background: url(noise.png) }`;
    const scan = scanCssReferences(css, "assets/css/site.css");
    expect(scan.refs.map((ref) => ref.path)).toEqual([
      "assets/fonts/geist.woff2",
      "assets/css/noise.png",
    ]);
    expect(
      scanCssReferences("a { background: url(../../escape.png) }", "assets/site.css").rejected,
    ).toEqual(["../../escape.png"]);
  });
});

describe("HTML preview document", () => {
  const html = `<!doctype html><html><head><link rel="stylesheet" href="style.css"></head><body>
    <img src="pixel.png"><div style="background: url(pixel.png)">Panel</div>
    <a href="https://example.com">Example</a><a href="#top">Top</a>
    <script src="app.js"></script></body></html>`;

  const assets = {
    "reports/style.css": { kind: "text" as const, text: `.x { background: url(pixel.png) }` },
    "reports/app.js": { kind: "text" as const, text: `console.log("</script>")` },
    "reports/pixel.png": { kind: "bytes" as const, base64: "iVBORwD/", mime: "image/png" },
  };

  it("uses the strict CSP and the scripts-only sandbox", () => {
    const built = buildPreviewDocument({
      html,
      docPath: "reports/index.html",
      assets,
      token: "t1",
    });
    expect(PREVIEW_SANDBOX).toBe("allow-scripts");
    expect(built.srcdoc).toContain(
      `<meta http-equiv="Content-Security-Policy" content="${PREVIEW_CSP}">`,
    );
    expect(built.srcdoc).toContain("default-src 'none'");
    expect(built.srcdoc).not.toContain("allow-same-origin");
  });

  it("inlines workspace styles, scripts and images", () => {
    const built = buildPreviewDocument({
      html,
      docPath: "reports/index.html",
      assets,
      token: "t1",
    });
    expect(built.srcdoc).toContain("<style>");
    expect(built.srcdoc).toContain('url("data:image/png;base64,iVBORwD/")');
    expect(built.srcdoc).toContain("data:image/png;base64,iVBORwD/");
    expect(built.srcdoc).toContain(
      'style="background: url(&quot;data:image/png;base64,iVBORwD/&quot;)"',
    );
    expect(built.srcdoc).toContain('console.log("<\\/script>")');
    expect(built.srcdoc).not.toContain('src="style.css"');
    expect(built.srcdoc).not.toContain('src="pixel.png"');
    expect(built.inlined).toEqual(["reports/style.css", "reports/pixel.png", "reports/app.js"]);
    expect(built.missing).toEqual([]);
  });

  it("rewrites links for the bridge and opens them through the parent", () => {
    const built = buildPreviewDocument({
      html,
      docPath: "reports/index.html",
      assets,
      token: "t1",
    });
    expect(built.srcdoc).toContain(
      'href="https://example.com" target="_blank" rel="noopener noreferrer"',
    );
    // A fragment scrolls inside the page rather than opening a new tab.
    expect(built.srcdoc).toContain('<a href="#top">Top</a>');
    expect(built.srcdoc).toContain('var token="t1"');
    expect(built.srcdoc).toContain("h4rn:preview-open");
    expect(built.token).toBe("t1");
  });

  it("reports references it could not inline", () => {
    const built = buildPreviewDocument({
      html: `<style>.x { background: url(missing-bg.png) }</style>
        <img src="missing.png"><img src="../../outside.png">`,
      docPath: "reports/index.html",
      assets,
      token: "t1",
    });
    expect(built.missing.sort()).toEqual([
      "../../outside.png",
      "reports/missing-bg.png",
      "reports/missing.png",
    ]);
  });

  it("injects the policy when the page has no head", () => {
    const injected = injectCsp("<p>hello</p>");
    expect(injected.startsWith("<head>")).toBe(true);
    expect(injected).toContain(PREVIEW_CSP);
  });

  it("keeps a doctype ahead of the injected head", () => {
    const injected = injectCsp("<!doctype html><p>hello</p>");
    expect(injected.startsWith("<!doctype html><head>")).toBe(true);
    expect(injected).toContain(PREVIEW_CSP);
  });

  it("stops adding assets at the total size cap", () => {
    expect(previewAssetFits(0, PREVIEW_MAX_TOTAL_BYTES)).toBe(true);
    expect(previewAssetFits(PREVIEW_MAX_TOTAL_BYTES, 1)).toBe(false);
    expect(previewAssetFits(9.5 * 1024 * 1024, 0.4 * 1024 * 1024)).toBe(true);
    expect(previewAssetFits(9.9 * 1024 * 1024, 0.2 * 1024 * 1024)).toBe(false);
  });

  it("names images and fonts by type, with a safe fallback", () => {
    expect(previewMimeType("a/b.PNG")).toBe("image/png");
    expect(previewMimeType("a/b.woff2")).toBe("font/woff2");
    expect(previewMimeType("a/b.bin")).toBe("application/octet-stream");
    expect(PREVIEW_MAX_TOTAL_BYTES).toBe(10 * 1024 * 1024);
  });
});
