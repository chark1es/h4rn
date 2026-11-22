import { describe, expect, it } from "vitest";

import type { FsFileContent } from "@/gen";
import {
  PREVIEW_MAX_ASSET_BYTES,
  PREVIEW_MAX_DOCUMENT_BYTES,
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

  it("reports the daemon's 2 MiB read limit, not the 10 MB budget", async () => {
    const big = {
      ...textFile("big.html", ""),
      content: null,
      truncated: true,
      size_bytes: PREVIEW_MAX_DOCUMENT_BYTES + 1,
      hint: "This file is 2.1 MiB; the editor refuses files over 2 MiB.",
    };
    const load = await loadPreview({ path: "big.html", read: reader({ "big.html": big }) });
    expect(load.document).toBeNull();
    expect(load.failure?.title).toBe("This file is too large to preview.");
    expect(load.failure?.detail).toContain("2.0 MiB");
    expect(load.failure?.detail).not.toContain("10.0 MiB");
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
