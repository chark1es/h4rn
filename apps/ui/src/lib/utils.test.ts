import { expect, test } from "vitest";
import { cn } from "./utils";

test("cn merges conditional classes", () => {
  expect(cn("px-2", false, "py-1", undefined)).toBe("px-2 py-1");
});

test("cn resolves conflicting tailwind classes in favour of the last", () => {
  expect(cn("px-2", "px-4")).toBe("px-4");
});
