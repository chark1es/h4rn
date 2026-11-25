import { beforeEach, describe, expect, it } from "vitest";

import {
  bindingFor,
  COMMANDS,
  chordFromEvent,
  chordKeys,
  chordText,
  eventKey,
  formatChord,
  matchCommand,
  parseChord,
  rebind,
  resetAllBindings,
  resetBinding,
} from "./keybindings";

const press = (code: string, key: string, mods: Partial<KeyboardEvent> = {}) => ({
  code,
  key,
  metaKey: false,
  ctrlKey: true,
  shiftKey: false,
  altKey: false,
  ...mods,
});

describe("keybindings", () => {
  beforeEach(() => resetAllBindings());

  it("binds every command to a unique Cmd/Ctrl chord", () => {
    const seen = new Set<string>();
    for (const command of COMMANDS) {
      const chord = parseChord(command.binding);
      expect(chord && formatChord(chord), command.id).toBe(command.binding);
      expect(seen.has(command.binding), `${command.id} duplicates ${command.binding}`).toBe(false);
      seen.add(command.binding);
    }
  });

  it("refuses chords without Cmd or Ctrl, except the composer's Option+Up", () => {
    expect(chordFromEvent(press("KeyT", "t", { ctrlKey: false }))).toBeNull();
    expect(parseChord("shift+t")).toBeNull();
    expect(parseChord("t")).toBeNull();
    expect(rebind("terminal.toggle", "t")).toEqual({
      ok: false,
      reason: "Shortcuts need Cmd or Ctrl plus a key.",
    });
    // Option alone is allowed to parse so the composer can offer Alt+Up, but
    // only the command that opted in may bind it.
    const bareAlt = parseChord("alt+arrowup");
    expect(bareAlt && formatChord(bareAlt)).toBe("alt+arrowup");
    expect(rebind("terminal.toggle", "alt+t")).toEqual({
      ok: false,
      reason: "Shortcuts need Cmd or Ctrl plus a key.",
    });
    const optionUp = chordFromEvent(press("ArrowUp", "ArrowUp", { ctrlKey: false, altKey: true }));
    expect(optionUp && matchCommand(optionUp)).toBe("thread.editQueuedMessage");
  });

  it("binds the queue shortcuts", () => {
    expect(bindingFor("thread.steerQueuedMessage")).toBe("mod+shift+enter");
    expect(bindingFor("thread.editQueuedMessage")).toBe("alt+arrowup");
    const steer = chordFromEvent(press("Enter", "Enter", { shiftKey: true }));
    expect(steer && matchCommand(steer)).toBe("thread.steerQueuedMessage");
    expect(rebind("thread.editQueuedMessage", "alt+arrowdown")).toEqual({ ok: true });
    expect(bindingFor("thread.editQueuedMessage")).toBe("alt+arrowdown");
  });

  it("reads the physical key so Shift and Option do not change the match", () => {
    expect(eventKey({ code: "KeyB", key: "∫" })).toBe("b");
    expect(eventKey({ code: "Slash", key: "?" })).toBe("/");
    expect(eventKey({ code: "Digit1", key: "!" })).toBe("1");
    expect(eventKey({ code: "Backquote", key: "`" })).toBe("`");
    expect(eventKey({ code: "ShiftLeft", key: "Shift" })).toBeNull();
    const chord = chordFromEvent(
      press("KeyB", "∫", { altKey: true, metaKey: true, ctrlKey: false }),
    );
    expect(chord && matchCommand(chord)).toBe("panel.toggle");
    const help = chordFromEvent(press("Slash", "/"));
    expect(help && matchCommand(help)).toBe("shortcuts.open");
  });

  it("rebinds, rejects conflicts, and resets", () => {
    expect(rebind("terminal.toggle", "mod+k")).toEqual({
      ok: false,
      reason: "Already used by “Search or run”.",
    });
    expect(rebind("terminal.toggle", "mod+shift+t")).toEqual({ ok: true });
    expect(bindingFor("terminal.toggle")).toBe("mod+shift+t");
    const chord = chordFromEvent(press("KeyT", "T", { shiftKey: true }));
    expect(chord && matchCommand(chord)).toBe("terminal.toggle");
    resetBinding("terminal.toggle");
    expect(bindingFor("terminal.toggle")).toBe("mod+`");
  });

  it("formats keycaps per platform", () => {
    expect(chordKeys("mod+shift+n", false)).toEqual(["Ctrl", "Shift", "N"]);
    expect(chordKeys("mod+alt+b", true)).toEqual(["⌘", "⌥", "B"]);
    expect(chordText("mod+enter", false)).toBe("Ctrl+↵");
    expect(chordText("mod+shift+n", true)).toBe("⌘⇧N");
  });

  it("opens the thread permission picker on mod+shift+a", () => {
    expect(bindingFor("thread.permissions")).toBe("mod+shift+a");
    const chord = chordFromEvent(press("KeyA", "A", { shiftKey: true }));
    expect(chord && matchCommand(chord)).toBe("thread.permissions");
    // Acknowledge moved off the chord the permission picker owns.
    expect(bindingFor("thread.acknowledge")).toBe("mod+alt+a");
  });
});
