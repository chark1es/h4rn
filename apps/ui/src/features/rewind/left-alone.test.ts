import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import { leftAloneList } from "./left-alone";

describe("leftAloneList", () => {
  afterEach(() => setLocalePreference("system"));

  it("says nothing when a rewind leaves nothing alone", () => {
    expect(leftAloneList([])).toBeNull();
    expect(leftAloneList(undefined)).toBeNull();
  });

  it("names four paths, and counts the rest", () => {
    expect(leftAloneList([".env"])).toBe(".env");
    expect(leftAloneList([".env", "secret.log"])).toBe(".env, secret.log");
    expect(leftAloneList(["a", "b", "c", "d", "e"])).toBe("a, b, c, d, and 1 more");
  });

  it("follows the language", async () => {
    await setLocalePreference("ja");
    expect(leftAloneList(["a", "b", "c", "d", "e", "f"])).toBe("a、b、c、d、ほか 2 件");
  });

  it("says the list's conjunction once in Spanish and Chinese", async () => {
    await setLocalePreference("es");
    expect(leftAloneList(["a", "b", "c", "d", "e"])).toBe("a, b, c, d y 1 más");
    expect(leftAloneList(["a", "b", "c", "d"])).toBe("a, b, c y d");
    await setLocalePreference("zh-CN");
    expect(leftAloneList(["a", "b", "c", "d", "e", "f"])).toBe("a、b、c、d和另外 2 个");
  });
});
