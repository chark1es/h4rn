import { describe, expect, it } from "vitest";

import type { FsFileContent } from "@/gen";
import {
  PREVIEW_MAX_ASSET_BYTES,
  PREVIEW_MAX_DOCUMENT_BYTES,
  PREVIEW_MAX_TEXT_BYTES,
  PREVIEW_MAX_TOTAL_BYTES,
} from "./htmlPreview";
import { loadPreview, type PreviewReader } from "./loadPreview";

function textFile(path: string, content: string): FsFileContent {
  return {
    path,
    content,
    etag: "etag",
    encoding: "utf-8",
    size_bytes: new TextEncoder().encode(content).length,
    binary: false,
    truncated: false,
    bytes_base64: null,
    bytes_omitted: null,
    content_omitted: null,
    modified_at: null,
    hint: null,
  };
}

function byteFile(path: string, base64: string, size: number): FsFileContent {
  return {
    path,
    content: null,
    etag: "etag",
    encoding: "binary",
    size_bytes: size,
    binary: true,
    truncated: false,
    bytes_base64: base64,
    bytes_omitted: null,
    content_omitted: null,
    modified_at: null,
    hint: null,
  };
}

function reader(files: Record<string, FsFileContent | Error>): PreviewReader {
  return async (path) => {
    const file = files[path];
    if (!file) throw new Error(`not found: ${path}`);
    if (file instanceof Error) throw file;
    return file;
  };
}

describe("loadPreview errors", () => {
  it("reports a file that vanished", async () => {
    const load = await loadPreview({
      path: "gone.html",
      read: reader({}),
    });
    expect(load.document).toBeNull();
    expect(load.failure?.title).toContain("no longer in the workspace");
  });

  it("reports the frame-sized text limit, not the 10 MB budget", async () => {
    const big = {
      ...textFile("big.html", ""),
      content: null,
      content_omitted: "too_large" as const,
      size_bytes: PREVIEW_MAX_DOCUMENT_BYTES + 1,
      hint: "This file is 0.4 MiB; a preview reads at most 400 KiB of text.",
    };
    const load = await loadPreview({ path: "big.html", read: reader({ "big.html": big }) });
    expect(load.document).toBeNull();
    expect(load.failure?.title).toBe("This file is too large to preview.");
    expect(load.failure?.detail).toContain("400 KiB");
    expect(load.failure?.detail).not.toContain("10.0 MiB");
    expect(load.watched).toEqual(["big.html"]);
  });

  it("keeps watching the selected file when the load fails", async () => {
    const binary = { ...textFile("binary.html", ""), content: null, binary: true };
    const cases = [
      { path: "gone.html", files: {}, expected: "no longer in the workspace" },
      {
        path: "big.html",
        files: {
          "big.html": {
            ...textFile("big.html", ""),
            content: null,
            content_omitted: "too_large" as const,
            size_bytes: PREVIEW_MAX_DOCUMENT_BYTES + 1,
          },
        },
        expected: "too large",
      },
      { path: "binary.html", files: { "binary.html": binary }, expected: "UTF-8" },
    ];
    for (const testCase of cases) {
      const load = await loadPreview({ path: testCase.path, read: reader(testCase.files) });
      expect(load.document).toBeNull();
      expect(load.failure?.title).toContain(testCase.expected);
      expect(load.watched).toEqual([testCase.path]);
    }
  });

  it("asks for the frame-sized text cap on documents and stylesheets", async () => {
    const budgets: number[] = [];
    const load = await loadPreview({
      path: "page.html",
      read: async (path, bytes, maxBytes) => {
        if (!bytes && maxBytes !== undefined) budgets.push(maxBytes);
        return path === "page.html"
          ? textFile("page.html", `<link rel="stylesheet" href="style.css">`)
          : textFile(path, ".x { color: red }");
      },
    });
    expect(load.failure).toBeNull();
    expect(budgets).toEqual([PREVIEW_MAX_TEXT_BYTES, PREVIEW_MAX_TEXT_BYTES]);
  });

  it("reports a file that is not UTF-8 text", async () => {
    const binary = {
      ...textFile("binary.html", ""),
      content: null,
      binary: true,
    };
    const load = await loadPreview({
      path: "binary.html",
      read: reader({ "binary.html": binary }),
    });
    expect(load.document).toBeNull();
    expect(load.failure?.title).toBe("This file isn’t UTF-8 text.");
  });
});

describe("loadPreview assets", () => {
  it("inlines a text asset and a binary asset", async () => {
    const html = `<link rel="stylesheet" href="style.css"><img src="pixel.png">`;
    const load = await loadPreview({
      path: "reports/page.html",
      read: reader({
        "reports/page.html": textFile("reports/page.html", html),
        "reports/style.css": textFile("reports/style.css", `.x { background: url(pixel.png) }`),
        "reports/pixel.png": byteFile("reports/pixel.png", "iVBORwD/", 6),
      }),
    });
    expect(load.failure).toBeNull();
    expect(load.document?.srcdoc).toContain("<style>");
    expect(load.document?.srcdoc).toContain("data:image/png;base64,iVBORwD/");
    expect(load.document?.missing).toEqual([]);
    expect(load.watched.sort()).toEqual([
      "reports/page.html",
      "reports/pixel.png",
      "reports/style.css",
    ]);
  });

  it("skips an asset the daemon refused as over its per-response cap", async () => {
    const refused = {
      ...byteFile("huge.png", "", PREVIEW_MAX_ASSET_BYTES + 1),
      bytes_base64: null,
      bytes_omitted: "too_large" as const,
      hint: "This asset is 0.5 MiB; the preview embeds files up to 360 KiB.",
    };
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile("page.html", `<img src="huge.png">`),
        "huge.png": refused,
      }),
    });
    expect(load.failure).toBeNull();
    expect(load.document?.missing).toEqual(["huge.png"]);
    expect(load.document?.srcdoc).not.toContain("huge.png");
  });

  it("skips an asset with no base64 bytes", async () => {
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile("page.html", `<img src="empty.png">`),
        "empty.png": byteFile("empty.png", "", 0),
      }),
    });
    expect(load.document?.missing).toEqual(["empty.png"]);
  });

  it("counts repeated copies against the 10 MB budget", async () => {
    const perCopy = "A".repeat(PREVIEW_MAX_ASSET_BYTES);
    const copies = 30;
    const images = Array.from({ length: copies }, () => `<img src="big.png">`).join("");
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile("page.html", images),
        "big.png": byteFile("big.png", perCopy, PREVIEW_MAX_ASSET_BYTES),
      }),
    });
    // 30 copies of 360 KiB is 10.5 MB of embedded output: over the cap, so the
    // asset is left out entirely instead of slipping past it.
    expect(load.document?.missing).toEqual(["big.png"]);
    expect(load.document?.srcdoc).not.toContain("AAAA");
  });

  it("keeps repeated copies that fit", async () => {
    const perCopy = "A".repeat(PREVIEW_MAX_ASSET_BYTES);
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile("page.html", `<img src="big.png"><img src="big.png">`),
        "big.png": byteFile("big.png", perCopy, PREVIEW_MAX_ASSET_BYTES),
      }),
    });
    expect(load.document?.missing).toEqual([]);
    const copies = load.document?.srcdoc.split("data:image/png;base64,").length ?? 1;
    expect(copies - 1).toBe(2);
  });

  it("never renders a document past the total budget", async () => {
    const perCopy = "B".repeat(PREVIEW_MAX_ASSET_BYTES);
    const images = Array.from({ length: 40 }, () => `<img src="edge.png">`).join("");
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile("page.html", images),
        "edge.png": byteFile("edge.png", perCopy, PREVIEW_MAX_ASSET_BYTES),
      }),
    });
    const size = new TextEncoder().encode(load.document?.srcdoc ?? "").length;
    expect(size).toBeLessThanOrEqual(PREVIEW_MAX_TOTAL_BYTES);
  });

  it("stops fetching once the total budget is spent", async () => {
    const fetched: string[] = [];
    const names = Array.from({ length: 40 }, (_, index) => `asset-${index}.png`);
    const images = names.map((name) => `<img src="${name}">`).join("");
    const load = await loadPreview({
      path: "page.html",
      read: async (path) => {
        if (path === "page.html") return textFile("page.html", images);
        fetched.push(path);
        return byteFile(path, "A".repeat(PREVIEW_MAX_ASSET_BYTES), PREVIEW_MAX_ASSET_BYTES);
      },
    });
    const srcdoc = load.document?.srcdoc ?? "";
    expect(fetched.length).toBeLessThan(names.length);
    // Everything after the budget ran out is reported, not left as a
    // relative reference the preview origin would receive.
    for (const name of names) expect(srcdoc).not.toContain(`src="${name}"`);
    expect(load.document?.missing.length).toBeGreaterThan(0);
  });

  it("ignores paths outside the workspace and other schemes", async () => {
    const load = await loadPreview({
      path: "page.html",
      read: reader({
        "page.html": textFile(
          "page.html",
          `<img src="../../outside.png"><img src="https://cdn.example.com/a.png">`,
        ),
      }),
    });
    expect(load.document?.missing).toEqual(["../../outside.png"]);
    expect(load.document?.srcdoc).toContain("https://cdn.example.com/a.png");
  });
});
