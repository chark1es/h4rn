import { describe, expect, it } from "vitest";

import { filterTerminalInput, isTerminalQueryReply } from "./terminal-reply";

describe("terminal query replies", () => {
  it("recognises xterm's automatic answers", () => {
    for (const reply of [
      "\x1b[?1;2c",
      "\x1b[?62;22c",
      "\x1b[>0;95;0c",
      "\x1b[12;40R",
      "\x1b[?12;40;1R",
      "\x1b[0n",
      "\x1b[?2026;2$y",
      "\x1b[?1u",
      "\x1b]10;rgb:cccc/cccc/cccc\x07",
      "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
    ]) {
      expect(isTerminalQueryReply(reply), reply).toBe(true);
    }
  });

  it("does not mistake typing or paste for a query answer", () => {
    for (const input of ["a", "\r", "\x1b", "\x1b[A", "\x1b[200~hello\x1b[201~", "ls -la\n"]) {
      expect(isTerminalQueryReply(input), input).toBe(false);
    }
  });

  it("drops only automatic replies on a non-responder", () => {
    expect(filterTerminalInput("\x1b[0n", false)).toBeNull();
    expect(filterTerminalInput("\x1b[?1;2c", false)).toBeNull();
    expect(filterTerminalInput("echo hi\r", false)).toBe("echo hi\r");
    expect(filterTerminalInput("\x1b[A", false)).toBe("\x1b[A");
  });

  it("forwards automatic replies on the elected responder", () => {
    expect(filterTerminalInput("\x1b[0n", true)).toBe("\x1b[0n");
    expect(filterTerminalInput("\x1b[12;40R", true)).toBe("\x1b[12;40R");
  });
});
