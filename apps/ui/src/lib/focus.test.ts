// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";

import { rememberNeighbours } from "./focus";

afterEach(() => {
  document.body.innerHTML = "";
});

function button(name: string) {
  const el = document.createElement("button");
  el.textContent = name;
  return el;
}

/** Before, a one-row list holding the trigger, After. */
function page() {
  const before = button("Before");
  const list = document.createElement("ul");
  const row = document.createElement("li");
  const trigger = button("Revoke");
  row.append(trigger);
  list.append(row);
  const after = button("After");
  document.body.append(before, list, after);
  return { before, list, row, trigger, after };
}

/** A confirm dialog with focus on its action. */
function openDialog() {
  const dialog = document.createElement("div");
  dialog.setAttribute("role", "alertdialog");
  const confirm = button("Confirm");
  dialog.append(confirm);
  document.body.append(dialog);
  confirm.focus();
  return dialog;
}

describe("rememberNeighbours", () => {
  it("moves focus to the next control when the row goes", async () => {
    const { row, trigger, after } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    row.remove();
    await restore();
    expect(document.activeElement).toBe(after);
  });

  it("finds the next control again when the update remounted it", async () => {
    const { row, trigger, after } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    // The list empties and the controls after it render afresh.
    row.remove();
    const remounted = button("After");
    after.replaceWith(remounted);
    await restore();
    expect(document.activeElement).toBe(remounted);
  });

  it("moves on from where a closing dialog fell back to", async () => {
    const { before, row, trigger, after } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    const dialog = openDialog();
    // The dialog closes while the trigger is busy and falls back to the control before.
    dialog.remove();
    before.focus();
    row.remove();
    const remounted = button("After");
    after.replaceWith(remounted);
    await restore();
    expect(document.activeElement).toBe(remounted);
  });

  it("returns focus to a trigger that survived once it is enabled again", async () => {
    const { before, trigger } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    const dialog = openDialog();
    trigger.disabled = true;
    dialog.remove();
    before.focus();
    trigger.disabled = false;
    await restore();
    expect(document.activeElement).toBe(trigger);
  });

  it("leaves focus where the person moved it while a failing action ran", async () => {
    const { before, trigger, after } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    const dialog = openDialog();
    trigger.disabled = true;
    dialog.remove();
    before.focus();
    // The person Tabs on while the request is still out.
    after.focus();
    trigger.disabled = false;
    await restore();
    expect(document.activeElement).toBe(after);
  });

  it("falls back to the control before when nothing follows the row", async () => {
    const { before, row, trigger, after } = page();
    trigger.focus();
    const restore = rememberNeighbours(trigger);
    row.remove();
    after.remove();
    await restore();
    expect(document.activeElement).toBe(before);
  });
});
