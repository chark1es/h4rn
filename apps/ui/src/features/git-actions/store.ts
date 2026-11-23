import type {
  ClientRequest,
  GitCommitResult,
  GitPushResult,
  GitThreadStatus,
  ResponseBody,
} from "@/gen";
import { getClient } from "@/lib/store/appStore";

/**
 * Typed git-action requests for the thread surface.
 *
 * The daemon owns every git call; this client only names a thread and the
 * fields the panel shows. Keeping the wire details here means the panel never
 * branches on a response tag and a test can hand in a fake transport.
 */
export type GitActionsRequest = (
  request: ClientRequest,
  options?: { timeoutMs?: number },
) => Promise<ResponseBody>;

/** A generated pull request title and body. */
export type PullRequestText = {
  title: string;
  body: string;
};

/** A commit message can take a model's whole timeout. */
const GENERATE_TIMEOUT_MS = 40_000;
/** A hook or a remote may take minutes; keep the socket open for it. */
export const ACTION_TIMEOUT_MS = 200_000;

/**
 * Opening a pull request shares one 220 s budget on the daemon (lookup, push
 * and forge call); this leaves a 20 s transport margin around it.
 */
export const OPEN_PR_TIMEOUT_MS = 240_000;

export class GitActionsApi {
  constructor(private readonly request: GitActionsRequest) {}

  private async call<K extends ResponseBody["type"]>(
    request: ClientRequest,
    type: K,
    options?: { timeoutMs?: number },
  ): Promise<Extract<ResponseBody, { type: K }>> {
    const response = await this.request(request, options);
    if (response.type !== type) {
      throw new Error(`expected ${type}, got ${response.type}`);
    }
    return response as Extract<ResponseBody, { type: K }>;
  }

  async status(threadId: string): Promise<GitThreadStatus> {
    const status = await this.call({ type: "git_status", thread_id: threadId }, "git_status");
    return status.status;
  }

  async commit(
    threadId: string,
    message: string,
    includeUnstaged: boolean,
    paths: string[] | null = null,
  ): Promise<GitCommitResult> {
    const response = await this.call(
      {
        type: "git_commit",
        thread_id: threadId,
        message,
        include_unstaged: includeUnstaged,
        paths,
      },
      "git_committed",
      { timeoutMs: ACTION_TIMEOUT_MS },
    );
    return response.result;
  }

  async push(threadId: string, setUpstream: boolean): Promise<GitPushResult> {
    const response = await this.call(
      { type: "git_push", thread_id: threadId, set_upstream: setUpstream },
      "git_pushed",
      { timeoutMs: ACTION_TIMEOUT_MS },
    );
    return response.result;
  }

  async generateCommitMessage(
    threadId: string,
    includeUnstaged: boolean,
    paths: string[] | null = null,
  ): Promise<string> {
    const response = await this.call(
      {
        type: "generate_git_commit_message",
        thread_id: threadId,
        include_unstaged: includeUnstaged,
        paths,
      },
      "generated_commit_message",
      { timeoutMs: GENERATE_TIMEOUT_MS },
    );
    return response.message;
  }

  async generatePullRequest(threadId: string, base: string | null): Promise<PullRequestText> {
    const response = await this.call(
      { type: "generate_pull_request_text", thread_id: threadId, base },
      "generated_pull_request_text",
      { timeoutMs: GENERATE_TIMEOUT_MS },
    );
    return { title: response.title, body: response.body };
  }
}

/** The live client. */
export function gitActionsApi(): GitActionsApi {
  return new GitActionsApi((request, options) => getClient().request(request, options));
}
