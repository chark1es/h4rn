// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import { parseThreadUrl } from "./mobile";

describe("notification deep links", () => {
  it("opens the thread a tapped alert names", () => {
    expect(parseThreadUrl("h4rn://thread/th_0192f0")).toBe("th_0192f0");
    expect(parseThreadUrl("h4rn://thread/th_0192f0/")).toBe("th_0192f0");
    expect(parseThreadUrl("h4rn://thread/th%2Fone")).toBe("th/one");
  });

  it("leaves pairing and unrelated links alone", () => {
    expect(parseThreadUrl("h4rn://connect?daemon=ws%3A%2F%2Fx&token=t")).toBeNull();
    expect(parseThreadUrl("h4rn://thread")).toBeNull();
    expect(parseThreadUrl("https://example.test/thread/th_one")).toBeNull();
    expect(parseThreadUrl("not a url")).toBeNull();
  });
});
