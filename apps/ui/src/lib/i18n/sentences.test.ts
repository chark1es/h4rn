import { describe, expect, it } from "vitest";

import { joinSentences } from "./sentences";

describe("joinSentences", () => {
  it("joins sentences with a space and drops empty parts", () => {
    expect(joinSentences(["Port in use.", null, "", "Nothing changed.", undefined])).toBe(
      "Port in use. Nothing changed.",
    );
  });

  it("puts no space after a full-width stop", () => {
    expect(joinSentences(["无法保存。", "这台机器上没有任何更改。"])).toBe(
      "无法保存。这台机器上没有任何更改。",
    );
    expect(joinSentences(["保存できませんでした。", "何も変更されていません。"])).toBe(
      "保存できませんでした。何も変更されていません。",
    );
  });

  it("keeps the space after a daemon's English sentence", () => {
    expect(joinSentences(["port in use.", "这台机器上没有任何更改。"])).toBe(
      "port in use. 这台机器上没有任何更改。",
    );
  });
});
