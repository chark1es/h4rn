// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import {
  hasPreviewOrigin,
  preparePreviewFrame,
  previewCapabilities,
  releasePreviewFrame,
} from "./previewFrame";

type Internals = { invoke: unknown };

function installShell(
  invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>,
) {
  (window as unknown as { __TAURI_INTERNALS__?: Internals }).__TAURI_INTERNALS__ = { invoke };
}

afterEach(() => {
  delete (window as unknown as { __TAURI_INTERNALS__?: Internals }).__TAURI_INTERNALS__;
});

describe("preview origin", () => {
  it("stores the document through the shell and returns only its URL", async () => {
    const calls: { command: string; args?: Record<string, unknown> | undefined }[] = [];
    installShell(async (command, args) => {
      calls.push({ command, args });
      if (command === "preview_prepare") {
        return { url: "h4rn-preview://localhost/abc123" };
      }
      return null;
    });

    expect(hasPreviewOrigin()).toBe(true);
    const prepared = await preparePreviewFrame("<p>hi</p>");
    // The bridge token is already in the document; the shell does not mint a
    // second one for the frame.
    expect(prepared).toEqual({ url: "h4rn-preview://localhost/abc123" });
    expect(calls[0]).toEqual({ command: "preview_prepare", args: { html: "<p>hi</p>" } });

    await releasePreviewFrame(prepared?.url ?? null);
    expect(calls[1]).toEqual({
      command: "preview_release",
      args: { url: "h4rn-preview://localhost/abc123" },
    });
  });

  it("asks the shell what its WebView can do", async () => {
    installShell(async (command) => {
      if (command === "preview_capabilities") {
        return { platform: "android", documentStartScripts: false };
      }
      return null;
    });
    expect(await previewCapabilities()).toEqual({
      platform: "android",
      documentStartScripts: false,
    });
  });

  it("keeps srcdoc in the browser build", async () => {
    expect(hasPreviewOrigin()).toBe(false);
    expect(await preparePreviewFrame("<p>hi</p>")).toBeNull();
    expect(await previewCapabilities()).toBeNull();
    // Releasing without a shell is a no-op, never a throw.
    await releasePreviewFrame("h4rn-preview://localhost/abc123");
  });

  it("falls back when the shell refuses the document", async () => {
    installShell(async () => {
      throw new Error("too large");
    });
    expect(await preparePreviewFrame("<p>hi</p>")).toBeNull();
  });
});
