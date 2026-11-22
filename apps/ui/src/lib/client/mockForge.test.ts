import { describe, expect, it } from "vitest";

import type { ClientRequest, ResponseBody } from "@/gen";

import { MockForge } from "./mockForge";

const titles: Record<string, string> = {
  thr_a: "Fix flaky auth test",
  thr_b: "Ship relay retry with backoff",
  thr_c: "Audit dependency tree",
};

function call(forge: MockForge, request: ClientRequest): ResponseBody {
  let answer: ResponseBody | null = null;
  const handled = forge.handle(request as { type: string }, {
    ok: (body) => {
      answer = body;
    },
    err: (code, message, hint) => {
      throw new Error(`${code}: ${message} ${hint ?? ""}`);
    },
    later: () => undefined,
    transient: () => undefined,
    threadTitle: (threadId) => titles[threadId] ?? null,
  });
  if (!handled || !answer) throw new Error(`the mock did not answer ${request.type}`);
  return answer;
}

const repo = {
  forge: "github" as const,
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

describe("the dev mock's thread pull request links", () => {
  it("lists, links, watches, unlinks and reverse-lists the same PR data", () => {
    const forge = new MockForge();

    const seeded = call(forge, { type: "list_thread_pull_requests", thread_id: "thr_a" });
    if (seeded.type !== "thread_pull_requests") throw new Error(seeded.type);
    expect(seeded.pulls).toHaveLength(1);
    expect(seeded.pulls[0]?.number).toBe(482);
    expect(seeded.pulls[0]?.watching).toBe(true);
    expect(seeded.pulls[0]?.summary?.title).toBe("Fix flaky auth test");

    const linked = call(forge, {
      type: "link_pull_request",
      thread_id: "thr_c",
      account_id: null,
      repo,
      number: 476,
    });
    if (linked.type !== "thread_pull_request") throw new Error(linked.type);
    expect(linked.pull.number).toBe(476);
    expect(linked.pull.watching).toBe(false);

    expect(
      call(forge, {
        type: "watch_pull_request",
        thread_id: "thr_c",
        repo,
        number: 476,
      }).type,
    ).toBe("ack");
    const watched = call(forge, { type: "list_thread_pull_requests", thread_id: "thr_c" });
    if (watched.type !== "thread_pull_requests") throw new Error(watched.type);
    expect(watched.pulls[0]?.watching).toBe(true);

    const reverse = call(forge, { type: "list_pr_threads", repo, number: 476 });
    if (reverse.type !== "pr_threads") throw new Error(reverse.type);
    expect(reverse.threads).toEqual([
      {
        thread_id: "thr_c",
        title: "Audit dependency tree",
        watching: true,
      },
    ]);

    expect(
      call(forge, {
        type: "unwatch_pull_request",
        thread_id: "thr_c",
        repo,
        number: 476,
      }).type,
    ).toBe("ack");
    const stopped = call(forge, { type: "list_thread_pull_requests", thread_id: "thr_c" });
    if (stopped.type !== "thread_pull_requests") throw new Error(stopped.type);
    expect(stopped.pulls[0]?.watching).toBe(false);

    expect(
      call(forge, {
        type: "unlink_pull_request",
        thread_id: "thr_c",
        repo,
        number: 476,
      }).type,
    ).toBe("ack");
    const unlinked = call(forge, { type: "list_thread_pull_requests", thread_id: "thr_c" });
    if (unlinked.type !== "thread_pull_requests") throw new Error(unlinked.type);
    expect(unlinked.pulls).toEqual([]);
    const gone = call(forge, { type: "list_pr_threads", repo, number: 476 });
    if (gone.type !== "pr_threads") throw new Error(gone.type);
    expect(gone.threads).toEqual([]);
  });
});
