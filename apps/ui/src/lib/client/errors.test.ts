import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { clientProto, clientSource, errorWords, protoError, toProtoError } from "./errors";

afterEach(() => setLocalePreference("system"));

describe("client-written errors", () => {
  it("are worded when raised and keep their kind", () => {
    const error = clientProto("internal", "notConnected");
    expect(error).toMatchObject({
      code: "internal",
      message: "The daemon is not connected.",
      hint: "Wait until the status bar says online, then try again.",
    });
    expect(clientSource(error)).toEqual({ kind: "notConnected" });
  });

  it("are worded again in the active language", async () => {
    const error = clientProto("internal", "requestTimeout", {
      params: { request: "list_threads", seconds: 1500 },
    });
    expect(error.message).toBe("The daemon did not answer `list_threads` within 1,500 s.");
    await setLocalePreference("ja");
    expect(errorWords(error)).toEqual({
      message: "デーモンが 1,500 秒以内に `list_threads` に応答しませんでした。",
      hint: "デーモンが混み合っている可能性があります。もう一度試すか、ステータスバーで接続を確認してください。",
    });
  });

  it("keep text that came with the failure as sent, translating only the hint", async () => {
    const error = clientProto("internal", "closed", { message: "going away" });
    await setLocalePreference("es");
    expect(errorWords(error)).toEqual({
      message: "going away",
      hint: "El cliente se reconecta automáticamente. Las lecturas se reintentan; las escrituras, no.",
    });
  });

  it("leave a daemon's error as sent", async () => {
    await setLocalePreference("zh-CN");
    expect(errorWords(protoError("conflict", "thread is busy", "wait for the turn"))).toEqual({
      message: "thread is busy",
      hint: "wait for the turn",
    });
  });

  it("give an unknown failure a kind with its own text", async () => {
    const thrown = toProtoError(new Error("socket hung up"));
    expect(thrown.message).toBe("socket hung up");
    expect(clientSource(thrown)).toMatchObject({ kind: "unexpected" });
    const unreadable = toProtoError(42);
    await setLocalePreference("zh-CN");
    expect(errorWords(unreadable).message).toBe("守护进程返回了客户端无法读取的内容。");
  });
});
