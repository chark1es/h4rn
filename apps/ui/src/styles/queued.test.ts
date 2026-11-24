import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

/**
 * The queue row's actions on a phone.
 *
 * A phone finger needs a real target, and three named actions plus two move
 * buttons do not fit on one line at 390 px: the row has to wrap instead of
 * squeezing the buttons.
 */
describe("queued message controls", () => {
  const css = readFileSync(new URL("./quiet.css", import.meta.url), "utf8");

  function rule(selector: string): string {
    const start = css.indexOf(selector);
    expect(start).toBeGreaterThan(-1);
    return css.slice(start, css.indexOf("}", start));
  }

  it("lets the actions wrap", () => {
    expect(rule(".q-queued-a {")).toMatch(/flex-wrap: wrap/);
  });

  it("gives the move buttons a 44 px target on a phone", () => {
    const phone = css.slice(css.indexOf("@media (max-width: 639px)"));
    const moveRule = phone.slice(
      phone.indexOf(".q-queued-a .q-ibtn"),
      phone.indexOf("}", phone.indexOf(".q-queued-a .q-ibtn")),
    );
    expect(moveRule).toMatch(/min-width: 44px/);
    expect(moveRule).toMatch(/min-height: 44px/);
  });
});
