import type {
  Event,
  FileEntry,
  FsDiff,
  FsFileContent,
  FsListing,
  FsSearchMatch,
  FsTurnChanges,
  FsWriteResult,
  ProtoError,
  ResponseBody,
} from "@/gen";

/**
 * An in-browser filesystem for `?mock=1`.
 *
 * The IDE must be screenshot-able and usable without a daemon, so the mock
 * transport answers the same `fs_*` requests from a small seeded repository.
 * It is deliberately not a POSIX emulator: paths are relative strings, `.git`
 * is fiction, and search streams through the same transient event shape the
 * real daemon uses. The editor consumes one code path.
 */

type MockHooks = {
  ok: (body: ResponseBody) => void;
  err: (code: ProtoError["code"], message: string, hint: string | null) => void;
  later: (ms: number, fn: () => void) => void;
  transient: (event: Event) => void;
};

const IGNORED = ["node_modules/", "dist/", ".env"];

const HEAD_FILES: Record<string, string> = {
  "package.json": `{
  "name": "@atlas/api",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "tsx src/server.ts",
    "test": "vitest run",
    "lint": "biome check ."
  },
  "dependencies": {
    "hono": "4.6.3",
    "zod": "3.23.8"
  },
  "devDependencies": {
    "vitest": "2.1.1",
    "typescript": "5.6.2"
  }
}
`,
  "tsconfig.json": `{
  "compilerOptions": {
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "module": "ESNext",
    "target": "ES2022",
    "moduleResolution": "bundler"
  },
  "include": ["src", "tests"]
}
`,
  "README.md": `# atlas api

Session verification, login and the database pool for the atlas control plane.

## Working agreements

- Every write is optimistic: read the file, keep its etag, send it back.
- Tests pin the clock with \`vi.useFakeTimers()\` before the pool opens.
- Do not commit generated bindings by hand.

\`\`\`bash
pnpm dev
pnpm test --filter auth
\`\`\`
`,
  ".gitignore": `node_modules
dist
.env
coverage
`,
  "docs/testing.md": `# Testing

The auth suite is the canary. If it flakes, the fake clock is installed after
the pool opens somewhere; find the module-scoped \`connect()\` and make it lazy.
`,
  "src/server.ts": `import { app } from "./app";

const port = Number(process.env.PORT ?? 4780);
app.listen({ port });
`,
  "src/app.ts": `import { Hono } from "hono";
import { login } from "./routes/login";

export const app = new Hono();
app.post("/login", login);
`,
  "src/routes/login.ts": `import type { Context } from "hono";
import { verifySession } from "../auth/session";

export async function login(context: Context) {
  const body = await context.req.json<{ token?: string }>();
  const session = await verifySession(body.token ?? "");
  if (!session) return context.json({ error: "unauthorized" }, 401);
  return context.json({ session });
}
`,
  "src/auth/auth.ts": `import { db } from "../lib/db";

export type User = { id: string; email: string };

export async function findUser(email: string): Promise<User | null> {
  return db.user.findByEmail(email);
}
`,
  "src/auth/session.ts": `import { db } from "../lib/db";

export async function verifySession(token: string) {
  const now = Date.now();
  if (!token) return null;

  const session = await db.session.find(token);
  if (!session) return null;
  return session;
}
`,
  "src/lib/db.ts": `type Session = { token: string; userId: string; expiresAt: number };

export const db = {
  session: {
    async find(token: string): Promise<Session | null> {
      return null;
    },
  },
  user: {
    async findByEmail(email: string) {
      return null;
    },
  },
};
`,
  "tests/auth.test.ts": `import { beforeEach, expect, it, vi } from "vitest";
import { verifySession } from "../src/auth/session";

beforeEach(() => {
  vi.useFakeTimers();
});

it("rejects an empty token", async () => {
  expect(await verifySession("")).toBeNull();
});
`,
};

const AGENT_FILES: Record<string, string> = {
  ...HEAD_FILES,
  "src/routes/login.ts": `import type { Context } from "hono";
import { verifySession } from "../auth/session";

// New: the login route returns the session's expiry so the client can schedule
// a refresh instead of discovering an expired token on the next request.
export async function login(context: Context) {
  const body = await context.req.json<{ token?: string }>();
  const session = await verifySession(body.token ?? "");
  if (!session) return context.json({ error: "unauthorized" }, 401);
  return context.json({
    session: { userId: session.userId },
    expiresAt: session.expiresAt,
  });
}
`,
  "src/auth/auth.ts": `import { db } from "../lib/db";

export type User = { id: string; email: string };

export async function findUser(email: string): Promise<User | null> {
  return db.user.findByEmail(email);
}

// Keep the refresh path in one place so both login and middleware agree.
export async function refreshSession(token: string) {
  const session = await db.session.find(token);
  if (!session) return null;
  if (session.expiresAt <= Date.now()) return null;
  return session;
}
`,
  "src/auth/session.ts": `import { db } from "../lib/db";

const CLOCK_SKEW_MS = 30_000;

function expired(expiresAt: number, now: number): boolean {
  return expiresAt - CLOCK_SKEW_MS <= now;
}

export async function verifySession(token: string) {
  const now = Date.now();
  if (!token) return null;

  const session = await db.session.find(token);
  if (!session) return null;
  if (expired(session.expiresAt, now)) return null;
  return session;
}
`,
  "tests/auth.test.ts": `import { beforeEach, expect, it, vi } from "vitest";
import { verifySession } from "../src/auth/session";

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
});

it("rejects an empty token", async () => {
  expect(await verifySession("")).toBeNull();
});

it("rejects an expired session at the skew boundary", async () => {
  vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
  expect(await verifySession("expired-token")).toBeNull();
});
`,
};

/** Changes the agent's turn made, relative to `AGENT_FILES` (pre-turn). */
const TURN_CHANGES = ["src/auth/session.ts", "tests/auth.test.ts", "src/routes/login.ts"];

/**
 * The working tree after the turn: pre-turn snapshot plus the agent's edits.
 * Seeding from this (rather than `AGENT_FILES`) keeps the turn diff and the
 * status-bar counts meaningful.
 */
const TURN_AFTER: Record<string, string> = {
  "src/auth/session.ts": `import { db } from "../lib/db";

const CLOCK_SKEW_MS = 30_000;

function expired(expiresAt: number, now: number): boolean {
  return expiresAt - CLOCK_SKEW_MS <= now;
}

export async function verifySession(token: string) {
  const now = Date.now();
  if (!token) return null;

  const session = await db.session.find(token);
  if (!session) return null;
  // A token inside the skew window is treated as already expired.
  if (expired(session.expiresAt, now)) return null;
  return { ...session, verifiedAt: now };
}
`,
  "src/routes/login.ts": `import type { Context } from "hono";
import { verifySession } from "../auth/session";

// New: the login route returns the session's expiry so the client can schedule
// a refresh instead of discovering an expired token on the next request.
export async function login(context: Context) {
  const body = await context.req.json<{ token?: string }>();
  const session = await verifySession(body.token ?? "");
  if (!session) return context.json({ error: "unauthorized" }, 401);
  return context.json({
    session: { userId: session.userId },
    expiresAt: session.expiresAt,
    verifiedAt: session.verifiedAt,
  });
}
`,
  "tests/auth.test.ts": `import { beforeEach, expect, it, vi } from "vitest";
import { verifySession } from "../src/auth/session";

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
});

it("rejects an empty token", async () => {
  expect(await verifySession("")).toBeNull();
});

it("rejects an expired session at the skew boundary", async () => {
  vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
  expect(await verifySession("expired-token")).toBeNull();
});

it("stamps the verification time on a live session", async () => {
  const session = await verifySession("live-token");
  expect(session?.verifiedAt).toBe(Date.now());
});
`,
};

const WORKTREE_FILES: Record<string, string> = { ...AGENT_FILES, ...TURN_AFTER };

const BINARY = new Set(["assets/logo.png"]);

function etag(text: string): string {
  // FNV-1a, stable and short; the real daemon uses BLAKE3.
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}

function lineDelta(before: string, after: string): { added: number; removed: number } {
  const a = new Set(before.split("\n"));
  const b = new Set(after.split("\n"));
  let added = 0;
  let removed = 0;
  for (const line of b) if (!a.has(line) && line !== "") added += 1;
  for (const line of a) if (!b.has(line) && line !== "") removed += 1;
  return { added, removed };
}

function join(parent: string, name: string): string {
  return parent ? `${parent}/${name}` : name;
}

export class MockFs {
  private readonly files = new Map<string, string>();
  private readonly trash = new Map<string, { path: string; content: string | null }>();
  private searchCounter = 0;

  constructor(private readonly workspace: string) {
    for (const [path, content] of Object.entries(WORKTREE_FILES)) this.files.set(path, content);
    this.files.set("assets/logo.png", "");
  }

  private current(path: string): string | null {
    return this.files.get(path) ?? null;
  }

  private entry(path: string, status: FileEntry["git_status"]): FileEntry {
    const content = this.files.get(path);
    return {
      name: path.split("/").at(-1) ?? path,
      path,
      kind: "file",
      size_bytes: content?.length ?? 0,
      modified_at: new Date().toISOString(),
      git_status: status,
    };
  }

  private statusOf(path: string): FileEntry["git_status"] {
    if (BINARY.has(path)) return null;
    const head = HEAD_FILES[path];
    const current = this.files.get(path);
    if (current === undefined) return head ? "deleted" : null;
    if (head === undefined) return "untracked";
    return head === current ? null : "modified";
  }

  private listing(path: string): FsListing {
    const prefix = path ? `${path}/` : "";
    const directories = new Set<string>();
    const entries: FileEntry[] = [];
    for (const filePath of this.files.keys()) {
      if (!filePath.startsWith(prefix)) continue;
      if (IGNORED.some((ignore) => filePath.includes(ignore))) continue;
      const rest = filePath.slice(prefix.length);
      if (rest.includes("/")) {
        directories.add(rest.split("/")[0] ?? rest);
      } else {
        entries.push(this.entry(filePath, this.statusOf(filePath)));
      }
    }
    for (const name of directories) {
      const child = join(path, name);
      const dirty = [...this.files.keys()]
        .filter((filePath) => filePath.startsWith(`${child}/`))
        .some((filePath) => this.statusOf(filePath) !== null);
      entries.push({
        name,
        path: child,
        kind: "directory",
        size_bytes: 0,
        modified_at: new Date().toISOString(),
        git_status: dirty ? "modified" : null,
      });
    }
    entries.sort((left, right) => {
      if (left.kind !== right.kind) return left.kind === "directory" ? -1 : 1;
      return left.name.localeCompare(right.name);
    });
    return { workspace: this.workspace, path, entries, truncated: false };
  }

  /** Answers one `fs_*` request. Returns false when it is not ours. */
  handle(body: { type: string }, hooks: MockHooks): boolean {
    switch (body.type) {
      case "fs_list": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_list" }>;
        hooks.ok({ type: "fs_listing", listing: this.listing(request.path) });
        return true;
      }
      case "fs_read": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_read" }>;
        hooks.ok({ type: "fs_file", file: this.read(request.path) });
        return true;
      }
      case "fs_write": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_write" }>;
        hooks.ok({ type: "fs_written", result: this.write(request) });
        return true;
      }
      case "fs_create": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_create" }>;
        const exists =
          request.kind === "directory"
            ? [...this.files.keys()].some((path) => path.startsWith(`${request.path}/`))
            : this.files.has(request.path);
        if (exists) {
          hooks.err("conflict", `${request.path} already exists.`, "Pick another name.");
          return true;
        }
        if (request.kind === "file") this.files.set(request.path, "");
        hooks.ok({ type: "fs_entry_created", entry: this.entry(request.path, "untracked") });
        return true;
      }
      case "fs_rename": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_rename" }>;
        const content = this.files.get(request.from);
        if (content === undefined) {
          hooks.err("not_found", `${request.from} no longer exists.`, "Refresh the tree.");
          return true;
        }
        this.files.delete(request.from);
        this.files.set(request.to, content);
        hooks.ok({ type: "fs_entry_created", entry: this.entry(request.to, "untracked") });
        return true;
      }
      case "fs_delete": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_delete" }>;
        const content = this.files.get(request.path) ?? null;
        this.files.delete(request.path);
        const trashId = `trash_${etag(request.path + Date.now())}`;
        this.trash.set(trashId, { path: request.path, content });
        hooks.ok({
          type: "fs_deleted",
          result: {
            path: request.path,
            trash_id: trashId,
            trashed_to: `/tmp/h4rn-trash/${trashId}`,
          },
        });
        return true;
      }
      case "fs_restore": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_restore" }>;
        const entry = this.trash.get(request.trash_id);
        if (!entry) {
          hooks.err(
            "not_found",
            "That trash entry is gone.",
            "Delete a file again to test restore.",
          );
          return true;
        }
        if (entry.content !== null) this.files.set(entry.path, entry.content);
        this.trash.delete(request.trash_id);
        hooks.ok({ type: "fs_restored", path: entry.path });
        return true;
      }
      case "fs_search": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_search" }>;
        return this.search(request, hooks);
      }
      case "fs_watch": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_watch" }>;
        hooks.ok({ type: "fs_watching", workspace: this.workspace, paths: request.paths });
        return true;
      }
      case "fs_unwatch":
        hooks.ok({ type: "ack" });
        return true;
      case "fs_diff": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_diff" }>;
        hooks.ok({ type: "fs_diff", diff: this.diff(request.path, request.against) });
        return true;
      }
      case "fs_turn_changes": {
        const request = body as Extract<import("@/gen").ClientRequest, { type: "fs_turn_changes" }>;
        hooks.ok({
          type: "fs_turn_changes",
          changes: this.turnChanges(request.thread_id, request.turn_id),
        });
        return true;
      }
      default:
        return false;
    }
  }

  private read(path: string): FsFileContent {
    const content = this.current(path);
    if (content === null) {
      return {
        path,
        content: null,
        etag: null,
        encoding: "unknown",
        size_bytes: 0,
        binary: false,
        truncated: false,
        modified_at: null,
        hint: `${path} no longer exists. It may have been deleted by the agent or another client.`,
      };
    }
    if (BINARY.has(path)) {
      return {
        path,
        content: null,
        etag: etag(path),
        encoding: "binary",
        size_bytes: 2_408,
        binary: true,
        truncated: false,
        modified_at: new Date().toISOString(),
        hint: "This looks like a binary file. h4rn does not edit binaries; use a terminal or your IDE.",
      };
    }
    return {
      path,
      content,
      etag: etag(content),
      encoding: "utf-8",
      size_bytes: content.length,
      binary: false,
      truncated: false,
      modified_at: new Date().toISOString(),
      hint: null,
    };
  }

  private write(request: {
    path: string;
    content: string;
    base_etag: string | null;
  }): FsWriteResult {
    const current = this.current(request.path);
    const currentEtag = current === null ? null : etag(current);
    const conflict =
      request.base_etag !== null && request.base_etag !== currentEtag
        ? current
        : request.base_etag === null && current !== null
          ? current
          : null;
    if (conflict !== null) {
      return {
        path: request.path,
        etag: etag(conflict),
        size_bytes: conflict.length,
        modified_at: new Date().toISOString(),
        conflict: {
          expected_etag: request.base_etag,
          current_etag: etag(conflict),
          current_content: conflict,
          current_encoding: "utf-8",
        },
      };
    }
    this.files.set(request.path, request.content);
    return {
      path: request.path,
      etag: etag(request.content),
      size_bytes: request.content.length,
      modified_at: new Date().toISOString(),
      conflict: null,
    };
  }

  private search(
    request: {
      query: string;
      regex: boolean;
      case_sensitive: boolean;
      glob: string | null;
      max_results: number | null;
    },
    hooks: MockHooks,
  ): boolean {
    let matcher: RegExp;
    try {
      matcher = new RegExp(
        request.regex ? request.query : request.query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"),
        request.case_sensitive ? "" : "i",
      );
    } catch {
      hooks.err("invalid_request", "That is not a valid regular expression.", "Fix the pattern.");
      return true;
    }
    const searchId = `search-${++this.searchCounter}`;
    hooks.ok({ type: "fs_search_started", search_id: searchId });

    const results: FsSearchMatch[] = [];
    const cap = request.max_results ?? 100;
    const glob = request.glob?.replace(/^\*\*\//, "").replace(/^\*\./, ".");
    for (const [path, content] of this.files) {
      if (BINARY.has(path)) continue;
      if (glob && !path.endsWith(glob)) continue;
      const lines = content.split("\n");
      for (let index = 0; index < lines.length; index += 1) {
        const line = lines[index] ?? "";
        matcher.lastIndex = 0;
        const match = matcher.exec(line);
        if (!match) continue;
        results.push({
          path,
          line_number: index + 1,
          column: match.index + 1,
          preview: line.trim().slice(0, 180),
        });
        if (results.length >= cap) break;
      }
      if (results.length >= cap) break;
    }

    // Stream in two batches so the panel's incremental rendering is exercised.
    const first = results.slice(0, Math.ceil(results.length / 2));
    const second = results.slice(first.length);
    hooks.later(90, () =>
      hooks.transient({
        type: "fs_search_batch",
        search_id: searchId,
        results: first,
        done: second.length === 0,
        truncated: results.length >= cap,
      }),
    );
    if (second.length > 0) {
      hooks.later(220, () =>
        hooks.transient({
          type: "fs_search_batch",
          search_id: searchId,
          results: second,
          done: true,
          truncated: results.length >= cap,
        }),
      );
    }
    return true;
  }

  private diff(path: string, against: FsDiff["against"]): FsDiff {
    const after = this.current(path);
    if (against.kind === "head") {
      const before = HEAD_FILES[path] ?? null;
      return {
        path,
        against,
        label: "HEAD",
        before,
        after,
        binary: BINARY.has(path),
        note: before === null ? "No HEAD version: the file is new or not tracked by git." : null,
      };
    }
    const before = AGENT_FILES[path] ?? null;
    return {
      path,
      against,
      label: "Turn 4 · Codex",
      before,
      after,
      binary: BINARY.has(path),
      note: before === null ? "No pre-turn snapshot for this file." : null,
    };
  }

  private turnChanges(threadId: string, turnId: string): FsTurnChanges {
    return {
      thread_id: threadId,
      turn_id: turnId,
      note: null,
      files: TURN_CHANGES.map((path) => {
        const { added, removed } = lineDelta(AGENT_FILES[path] ?? "", this.current(path) ?? "");
        return {
          path,
          added,
          removed,
          before_etag: etag(AGENT_FILES[path] ?? ""),
          after_etag: etag(this.current(path) ?? ""),
        };
      }),
    };
  }
}

/** One mock filesystem per workspace, seeded with the atlas auth repo. */
export function createMockFs(workspace = "/home/you/code/api"): MockFs {
  return new MockFs(workspace);
}

export { AGENT_FILES, HEAD_FILES, TURN_AFTER, TURN_CHANGES, WORKTREE_FILES };
