/**
 * xterm answers some escape sequences by itself: device attributes (DA),
 * cursor position and status reports (DSR), mode reports (DECRQM) and the
 * foreground/background colour queries (OSC 10/11/12). Those answers arrive on
 * `onData` mixed with the person's keystrokes.
 *
 * One PTY shared by two viewers must hear only one answer, or the shell reads
 * duplicate replies as input. The daemon elects one responder; every other
 * viewer drops the sequences below and forwards everything else, so typing is
 * never filtered.
 */
// Patterns are strings because Biome rejects control characters in regex
// literals; each one is anchored, so only a complete reply is dropped.
const QUERY_REPLY = [
  // DA1: CSI ? 1 ; 2 c
  "^\\x1b\\[\\?[0-9;]*c$",
  // DA2 / XTVERSION: CSI > 0 ; 95 c
  "^\\x1b\\[>[0-9;]*c$",
  // DSR cursor position: CSI row ; col R
  "^\\x1b\\[\\d+;\\d+R$",
  // DECXCPR: CSI ? row ; col ; 1 R
  "^\\x1b\\[\\?\\d+;\\d+(?:;\\d+)?R$",
  // DSR status: CSI 0 n
  "^\\x1b\\[\\d+n$",
  // DECRQM: CSI ? mode ; state $ y
  "^\\x1b\\[\\?[0-9;]+\\$y$",
  // Kitty keyboard flags: CSI ? flags u
  "^\\x1b\\[\\?[0-9;]*u$",
  // OSC 10/11/12 colour: ESC ] 10 ; rgb:rrrr/gggg/bbbb BEL or ST
  "^\\x1b\\](?:10|11|12);(?:rgb|rgba):[0-9a-fA-F/]+(?:\\x07|\\x1b\\\\)$",
].map((pattern) => new RegExp(pattern));

/** Whether `data` is exactly one of xterm's automatic query answers. */
export function isTerminalQueryReply(data: string): boolean {
  return data.length > 0 && QUERY_REPLY.some((pattern) => pattern.test(data));
}

/**
 * Returns the bytes to send to the PTY, or `null` when a non-responder would
 * duplicate an answer the elected viewer is already sending.
 */
export function filterTerminalInput(data: string, answersQueries: boolean): string | null {
  return !answersQueries && isTerminalQueryReply(data) ? null : data;
}
