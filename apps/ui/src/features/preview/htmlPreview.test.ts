import { describe, expect, it } from "vitest";

import {
  buildPreviewDocument,
  injectCsp,
  isHtmlPath,
  PREVIEW_CSP,
  PREVIEW_CSP_META,
  PREVIEW_MAX_TOTAL_BYTES,
  PREVIEW_SANDBOX,
  previewAssetFits,
  previewBridge,
  previewEmbeddedBytes,
  previewLinkOpens,
  previewMimeType,
  previewOpenRequest,
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
      { url: "style.css", path: "reports/style.css", kind: "text", count: 1 },
      { url: "app.js", path: "reports/app.js", kind: "text", count: 1 },
      { url: "hero.png", path: "reports/hero.png", kind: "bytes", count: 1 },
      { url: "chart.png", path: "reports/chart.png", kind: "bytes", count: 1 },
      { url: "bg.png", path: "reports/bg.png", kind: "bytes", count: 1 },
    ]);
    expect(scan.external).toEqual([
      "https://cdn.example.com/lib.js",
      "https://cdn.example.com/photo.jpg",
    ]);
    expect(scan.rejected).toEqual(["../../outside.png"]);
  });

  it("counts every reference to the same asset", () => {
    const html = `<img src="logo.png"><img src="./logo.png"><div style="background: url(logo.png)">`;
    const scan = scanPreviewReferences(html, "index.html");
    expect(scan.refs).toEqual([{ url: "logo.png", path: "logo.png", kind: "bytes", count: 3 }]);
  });

  it("finds srcset, media, object, embed and SVG references", () => {
    const html = `<img srcset="a.png 1x, b.png 2x">
      <video src="clip.mp4" poster="thumb.jpg"></video>
      <audio src="sound.mp3"></audio>
      <track src="captions.vtt">
      <object data="doc.pdf"></object>
      <embed src="widget.swf">
      <svg><image href="icon.png"></image><use xlink:href="icons.svg#gear"></use></svg>
      <iframe src="frame.html"></iframe>`;
    const scan = scanPreviewReferences(html, "reports/index.html");
    expect(scan.refs.map((ref) => ref.path)).toEqual([
      "reports/a.png",
      "reports/b.png",
      "reports/clip.mp4",
      "reports/thumb.jpg",
      "reports/sound.mp3",
      "reports/captions.vtt",
      "reports/doc.pdf",
      "reports/widget.swf",
      "reports/icon.png",
      "reports/icons.svg",
      "reports/frame.html",
    ]);
  });

  it("finds imported stylesheets as CSS text", () => {
    const css = `@import "base.css"; @import url(print.css) print; .x { background: url(bg.png) }`;
    const scan = scanCssReferences(css, "assets/site.css");
    expect(scan.refs).toEqual([
      { url: "base.css", path: "assets/base.css", kind: "text", count: 1 },
      { url: "print.css", path: "assets/print.css", kind: "text", count: 1 },
      { url: "bg.png", path: "assets/bg.png", kind: "bytes", count: 1 },
    ]);
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
    expect(built.srcdoc).toContain(PREVIEW_CSP_META);
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

  it("puts the policy first even when the page fakes a head", () => {
    const built = buildPreviewDocument({
      html: `<!-- <head> --><p>Hello</p>`,
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    // The trusted doctype and policy are the document's first bytes, before
    // the fake-head comment.
    expect(built.srcdoc.startsWith(`<!doctype html>${PREVIEW_CSP_META}`)).toBe(true);
    expect(built.srcdoc.indexOf(PREVIEW_CSP_META)).toBeLessThan(
      built.srcdoc.indexOf("<!-- <head> -->"),
    );
  });

  it("keeps the bridge out of a script that contains a body tag", () => {
    const built = buildPreviewDocument({
      html: `<!doctype html><p>Hi</p><script>const close = "</body>";</script>`,
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    // The bridge is appended, never spliced into the script's string.
    expect(built.srcdoc).toContain(`const close = "</body>";`);
    expect(built.srcdoc).toContain(`const close = "</body>";</script>`);
    expect(built.srcdoc.indexOf("h4rn:preview-open")).toBeGreaterThan(
      built.srcdoc.indexOf(`const close = "</body>";`),
    );
  });

  it("emits a trusted doctype and policy before the workspace document", () => {
    const injected = injectCsp("<p>hello</p>");
    expect(injected.startsWith(`<!doctype html>${PREVIEW_CSP_META}`)).toBe(true);
    expect(injected).toContain(PREVIEW_CSP);

    // The document's own doctype is dropped; ours is the only one.
    const withDoctype = injectCsp("<!doctype html><p>hello</p>");
    expect(withDoctype.startsWith(`<!doctype html>${PREVIEW_CSP_META}<p>hello</p>`)).toBe(true);
    expect(withDoctype.match(/<!doctype/gi)).toHaveLength(1);
  });

  it("removes local references it could not inline", () => {
    const built = buildPreviewDocument({
      html: `<link rel="icon" href="favicon.png">
        <link rel="stylesheet" href="gone.css">
        <script src="gone.js"></script>
        <img src="gone.png"><img src="../../outside.png">
        <img src="https://cdn.example.com/ok.png">
        <style>.x { background: url(gone-bg.png) }</style>`,
      docPath: "reports/index.html",
      assets,
      token: "t1",
    });
    for (const relative of ['href="favicon.png"', 'href="gone.css"', 'src="gone.js"']) {
      expect(built.srcdoc).not.toContain(relative);
    }
    expect(built.srcdoc).not.toContain('src="gone.png"');
    expect(built.srcdoc).not.toContain('src="../../outside.png"');
    expect(built.srcdoc).not.toContain("gone-bg.png");
    expect(built.srcdoc).toContain("background: none");
    // A CDN asset is still the page's own business.
    expect(built.srcdoc).toContain('src="https://cdn.example.com/ok.png"');
    expect(built.missing.sort()).toEqual([
      "../../outside.png",
      "reports/gone-bg.png",
      "reports/gone.css",
      "reports/gone.js",
      "reports/gone.png",
    ]);
  });

  it("preserves the type of an inlined script", () => {
    const built = buildPreviewDocument({
      html: `<script type="application/ld+json" src="data.json"></script>
        <script type="importmap" src="map.json"></script>
        <script src="plain.js"></script>`,
      docPath: "index.html",
      assets: {
        "data.json": { kind: "text" as const, text: `{"name":"h4rn"}` },
        "map.json": { kind: "text" as const, text: `{"imports":{}}` },
        "plain.js": { kind: "text" as const, text: `run()` },
      },
      token: "t1",
    });
    expect(built.srcdoc).toContain('<script type="application/ld+json">{"name":"h4rn"}</script>');
    expect(built.srcdoc).toContain('<script type="importmap">{"imports":{}}</script>');
    expect(built.srcdoc).toContain("<script>run()</script>");
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

  it("counts every embedded copy against the budget", () => {
    const asset = { kind: "bytes" as const, base64: "AAAA", mime: "image/png" };
    const one = previewEmbeddedBytes(asset);
    expect(one).toBe(4);
    expect(previewAssetFits(0, PREVIEW_MAX_TOTAL_BYTES)).toBe(true);
    expect(previewAssetFits(PREVIEW_MAX_TOTAL_BYTES, 1)).toBe(false);
    // Repeating a large asset multiplies its cost, so the page cannot
    // serialize far past the cap through repetition.
    const many = previewEmbeddedBytes(asset) * 3;
    expect(previewAssetFits(PREVIEW_MAX_TOTAL_BYTES - 16, many)).toBe(true);
    expect(previewAssetFits(PREVIEW_MAX_TOTAL_BYTES - 16, many * 2)).toBe(false);
  });

  it("inlines srcset candidates and drops the ones it cannot", () => {
    const built = buildPreviewDocument({
      html: `<img srcset="a.png 1x, b.png 2x, gone.png 3x, https://cdn.example.com/c.png 4x">`,
      docPath: "reports/index.html",
      assets: {
        "reports/a.png": { kind: "bytes" as const, base64: "AAAA", mime: "image/png" },
        "reports/b.png": { kind: "bytes" as const, base64: "BBBB", mime: "image/png" },
      },
      token: "t1",
    });
    expect(built.srcdoc).toContain(
      'srcset="data:image/png;base64,AAAA 1x, data:image/png;base64,BBBB 2x, https://cdn.example.com/c.png 4x"',
    );
    expect(built.srcdoc).not.toContain("gone.png");
    expect(built.missing).toEqual(["reports/gone.png"]);
  });

  it("keeps a data: srcset candidate while inlining a workspace one", () => {
    const built = buildPreviewDocument({
      html: `<img srcset="data:image/png;base64,AAAA 1x, small.png 2x">`,
      docPath: "index.html",
      assets: {
        "small.png": { kind: "bytes" as const, base64: "BBBB", mime: "image/png" },
      },
      token: "t1",
    });
    // The data URL's own comma is part of its URL, not a candidate boundary.
    expect(built.srcdoc).toContain(
      'srcset="data:image/png;base64,AAAA 1x, data:image/png;base64,BBBB 2x"',
    );
    expect(built.missing).toEqual([]);
  });

  it("treats a trailing comma as the boundary after a descriptorless candidate", () => {
    const built = buildPreviewDocument({
      html: `<img srcset="data:image/png;base64,AAAA, small.png 2x">`,
      docPath: "index.html",
      assets: {
        "small.png": { kind: "bytes" as const, base64: "BBBB", mime: "image/png" },
      },
      token: "t1",
    });
    // The data URL has no descriptor, so its trailing comma ends it. The
    // workspace candidate after the comma is still rewritten and inlined.
    expect(built.srcdoc).toContain(
      'srcset="data:image/png;base64,AAAA, data:image/png;base64,BBBB 2x"',
    );
    expect(built.missing).toEqual([]);
  });

  it("removes the workspace candidate that follows a descriptorless one when it is missing", () => {
    const built = buildPreviewDocument({
      html: `<img srcset="data:image/png;base64,AAAA, gone.png 2x">`,
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    expect(built.srcdoc).toContain('srcset="data:image/png;base64,AAAA"');
    expect(built.srcdoc).not.toContain("gone.png");
    expect(built.missing).toEqual(["gone.png"]);
  });

  it("removes a missing workspace srcset candidate and keeps the data: one", () => {
    const built = buildPreviewDocument({
      html: `<img srcset="data:image/png;base64,AAAA 1x, gone.png 2x, https://cdn.example.com/c.png 3x">`,
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    expect(built.srcdoc).toContain(
      'srcset="data:image/png;base64,AAAA 1x, https://cdn.example.com/c.png 3x"',
    );
    expect(built.srcdoc).not.toContain("gone.png");
    expect(built.missing).toEqual(["gone.png"]);
  });

  it("drops a srcset attribute when no candidate can be inlined", () => {
    const built = buildPreviewDocument({
      html: `<picture><source srcset="gone.png 1x"></picture>`,
      docPath: "index.html",
      assets: {},
      token: "t1",
    });
    expect(built.srcdoc).not.toContain("srcset");
    expect(built.srcdoc).not.toContain("gone.png");
    expect(built.missing).toEqual(["gone.png"]);
  });

  it("inlines media, object, embed and SVG references", () => {
    const built = buildPreviewDocument({
      html: `<video src="clip.mp4" poster="thumb.jpg"></video>
        <audio src="sound.mp3"></audio>
        <track src="captions.vtt">
        <object data="doc.pdf"></object>
        <embed src="widget.swf">
        <svg><image href="icon.png"></image><use xlink:href="icons.svg#gear"></use>
        <use href="#local"></use></svg>
        <iframe src="frame.html"></iframe>`,
      docPath: "reports/index.html",
      assets: {
        "reports/clip.mp4": { kind: "bytes" as const, base64: "VklE", mime: "video/mp4" },
        "reports/thumb.jpg": { kind: "bytes" as const, base64: "SlBH", mime: "image/jpeg" },
        "reports/sound.mp3": { kind: "bytes" as const, base64: "QVVESU8=", mime: "audio/mpeg" },
        "reports/captions.vtt": { kind: "bytes" as const, base64: "V0VCVlRU", mime: "text/vtt" },
        "reports/doc.pdf": { kind: "bytes" as const, base64: "UERG", mime: "application/pdf" },
        "reports/icon.png": { kind: "bytes" as const, base64: "SU1H", mime: "image/png" },
        "reports/icons.svg": { kind: "bytes" as const, base64: "U1ZH", mime: "image/svg+xml" },
      },
      token: "t1",
    });
    expect(built.srcdoc).toContain('src="data:video/mp4;base64,VklE"');
    expect(built.srcdoc).toContain('poster="data:image/jpeg;base64,SlBH"');
    expect(built.srcdoc).toContain('src="data:audio/mpeg;base64,QVVESU8="');
    expect(built.srcdoc).toContain('src="data:text/vtt;base64,V0VCVlRU"');
    expect(built.srcdoc).toContain('data="data:application/pdf;base64,UERG"');
    expect(built.srcdoc).toContain('href="data:image/png;base64,SU1H"');
    expect(built.srcdoc).toContain('xlink:href="data:image/svg+xml;base64,U1ZH#gear"');
    expect(built.srcdoc).toContain('<use href="#local"></use>');
    // A local iframe document is not a supported inline asset: its reference
    // is removed and counted, never left as a relative request.
    expect(built.srcdoc).toContain("<iframe></iframe>");
    expect(built.srcdoc).not.toContain("frame.html");
    expect(built.srcdoc).not.toContain("widget.swf");
    expect(built.missing.sort()).toEqual(["reports/frame.html", "reports/widget.swf"]);
  });

  it("inlines an imported workspace stylesheet as CSS text", () => {
    const built = buildPreviewDocument({
      html: `<link rel="stylesheet" href="style.css">`,
      docPath: "reports/index.html",
      assets: {
        "reports/style.css": {
          kind: "text" as const,
          text: `@import "base.css";\n@import url(print.css) print;\n.x { color: red }`,
        },
        "reports/base.css": { kind: "text" as const, text: `.base { margin: 0 }` },
        "reports/print.css": { kind: "text" as const, text: `.print { display: none }` },
      },
      token: "t1",
    });
    expect(built.srcdoc).toContain(".base { margin: 0 }");
    expect(built.srcdoc).toContain("@media print {.print { display: none }}");
    expect(built.srcdoc).not.toContain("@import");
    expect(built.srcdoc).not.toContain("base.css");
    expect(built.missing).toEqual([]);
  });

  it("keeps leading comments after the policy instead of scanning for a doctype", () => {
    const injected = injectCsp("\n  <!-- generated -->\n<!doctype html><p>hello</p>");
    expect(injected.startsWith(`<!doctype html>${PREVIEW_CSP_META}`)).toBe(true);
    expect(injected.indexOf(PREVIEW_CSP_META)).toBeLessThan(injected.indexOf("<!-- generated -->"));
    expect(injected.indexOf("<!-- generated -->")).toBeLessThan(injected.indexOf("<p>hello</p>"));
  });

  it("charges an asset again when an imported stylesheet embeds it", () => {
    const pixel = "A".repeat(36 * 1024);
    const html = `<link rel="stylesheet" href="theme.css"><img src="logo.png">`;
    const assets = {
      "theme.css": { kind: "text" as const, text: `@import url("colors.css");` },
      "colors.css": { kind: "text" as const, text: `.x { background: url(logo.png) }` },
      "logo.png": { kind: "bytes" as const, base64: pixel, mime: "image/png" },
    };
    const bytes = (text: string) => new TextEncoder().encode(text).length;
    const dataUrl = `data:image/png;base64,${pixel}`;
    // Room for the page, both stylesheets, exactly one copy of the image, and
    // the fixed prefix and bridge every document carries.
    const maxBytes =
      bytes(html) +
      bytes(assets["theme.css"].text) +
      bytes(assets["colors.css"].text) +
      bytes(dataUrl) +
      bytes(injectCsp("")) +
      bytes(previewBridge("t1")) +
      512;
    const built = buildPreviewDocument({
      html,
      docPath: "page.html",
      assets,
      token: "t1",
      maxBytes,
    });
    // The image is inserted once through the imported stylesheet. The second
    // copy, asked for by the <img>, does not fit and is not inlined or counted
    // as missing silently: it is reported as skipped.
    expect(built.srcdoc.split("data:image/png;base64,").length - 1).toBe(1);
    expect(built.missing).toEqual(["logo.png"]);
  });

  it("names images and fonts by type, with a safe fallback", () => {
    expect(previewMimeType("a/b.PNG")).toBe("image/png");
    expect(previewMimeType("a/b.woff2")).toBe("font/woff2");
    expect(previewMimeType("a/b.bin")).toBe("application/octet-stream");
    expect(PREVIEW_MAX_TOTAL_BYTES).toBe(10 * 1024 * 1024);
  });
});

describe("HTML preview links", () => {
  it("accepts only http and https links from the frame", () => {
    expect(previewOpenRequest("https://example.com/a?b=1")).toEqual({
      url: "https://example.com/a?b=1",
      host: "example.com",
    });
    expect(previewOpenRequest("http://example.com")?.host).toBe("example.com");
    expect(previewOpenRequest("javascript:alert(1)")).toBeNull();
    expect(previewOpenRequest("file:///etc/passwd")).toBeNull();
    expect(previewOpenRequest("h4rn-preview://localhost/token")).toBeNull();
    expect(previewOpenRequest("not a url")).toBeNull();
    expect(previewOpenRequest(42)).toBeNull();
    expect(previewOpenRequest(null)).toBeNull();
  });

  it("leaves modifier-clicks to the browser", () => {
    expect(previewLinkOpens({})).toBe(true);
    expect(previewLinkOpens({ button: 0 })).toBe(true);
    expect(previewLinkOpens({ metaKey: true })).toBe(false);
    expect(previewLinkOpens({ ctrlKey: true })).toBe(false);
    expect(previewLinkOpens({ shiftKey: true })).toBe(false);
    expect(previewLinkOpens({ altKey: true })).toBe(false);
    expect(previewLinkOpens({ button: 1 })).toBe(false);
  });
});
