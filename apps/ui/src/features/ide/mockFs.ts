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

/**
 * The atlas api repository at HEAD. The screenshot IDE shows an auth suite
 * mid-edit, so the tree, the change set and the test diff all come from the
 * same small repo.
 */
const HEAD_FILES: Record<string, string> = {
  "package.json": `{
  "name": "@atlas/api",
  "private": true,
  "type": "module",
  "scripts": {
    "test": "vitest run",
    "typecheck": "tsc --noEmit"
  },
  "dependencies": {
    "pg": "8.13.1"
  },
  "devDependencies": {
    "typescript": "5.6.2",
    "vitest": "2.1.1"
  }
}
`,
  "vitest.config.ts": `import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    setupFiles: ["tests/setup.ts"],
    pool: "forks",
  },
});
`,
  "src/auth/token.ts": `import { randomBytes } from "node:crypto";

import { db } from "../db/pool";

export function mintToken(): string {
  return \`t_\${randomBytes(2).toString("hex").slice(0, 3)}\`;
}

export async function findSession(token: string) {
  return db.session.find(token);
}

export async function seedSession(input: { token: string; ttl: number }) {
  await db.session.insert({
    token: input.token,
    userId: \`user_\${input.token.slice(2)}\`,
    expiresAt: Date.now() + input.ttl,
  });
}
`,
  "src/auth/session.ts": `import { db } from "../db/pool";

export const CLOCK_SKEW_MS = 30_000;

export async function verifySession(token: string) {
  if (!token) return null;
  const session = await db.session.find(token);
  if (!session) return null;
  if (session.expiresAt <= Date.now()) return null;
  return session;
}
`,
  "src/auth/refresh.ts": `import { verifySession } from "./session";

export async function refresh(token: string) {
  const session = await verifySession(token);
  if (!session) return null;
  return { ...session, expiresAt: Date.now() + 30_000 };
}
`,
  "src/db/pool.ts": `import { Pool } from "pg";

const pool = new Pool({ connectionString: process.env.DATABASE_URL });

export const db = {
  session: {
    async find(token: string) {
      const { rows } = await pool.query("select * from sessions where token = $1", [token]);
      return rows[0] ?? null;
    },
    async insert(session: { token: string; userId: string; expiresAt: number }) {
      await pool.query(
        "insert into sessions (token, user_id, expires_at) values ($1, $2, $3)",
        [session.token, session.userId, session.expiresAt],
      );
    },
    async deleteMany() {
      await pool.query("delete from sessions");
    },
  },
};
`,
  "tests/setup.ts": `import { afterAll, beforeAll, vi } from "vitest";

import { db } from "../src/db/pool";

beforeAll(async () => {
  vi.useFakeTimers();
  await db.session.deleteMany();
});

afterAll(() => {
  vi.useRealTimers();
});
`,
  "src/auth/session.test.ts": [
    `import { beforeEach, describe, expect, it, vi } from "vitest";`,
    ``,
    `import { findSession, mintToken, seedSession } from "../auth/token";`,
    `import { verifySession } from "../auth/session";`,
    `import { db } from "../db/pool";`,
    ``,
    `const clock = vi.useFakeTimers({ now: new Date("2026-03-04T12:00:00Z") });`,
    ``,
    `const LIVE = {`,
    `  token: "t_9f2",`,
    `  userId: "user_9f2",`,
    `  expiresAt: Date.now() + 30_000,`,
    `};`,
    ``,
    `beforeEach(async () => {`,
    `  vi.setSystemTime(new Date("2026-03-04T12:00:00Z"));`,
    `  await db.session.deleteMany();`,
    `  await seedSession(LIVE);`,
    `});`,
    ``,
    `describe("token helpers", () => {`,
    `  it("mints an opaque token", () => {`,
    `    expect(mintToken()).toMatch(/^t_[0-9a-f]{3}$/);`,
    `  });`,
    ``,
    `  it("pins the token to the session", async () => {`,
    `    const session = await findSession("t_9f2");`,
    `    expect(session?.userId).toBe("user_9f2");`,
    `  });`,
    ``,
    `  it("rejects a token without a session", async () => {`,
    `    expect(await findSession("t_none")).toBeNull();`,
    `  });`,
    `});`,
    ``,
    `describe("verifySession", () => {`,
    `  beforeEach(async () => {`,
    `    await seedSession({ token: "t_9f2", ttl: 30_000 });`,
    `  });`,
    ``,
    `  it("returns the session before expiry", async () => {`,
    `    const session = await verifySession("t_9f2");`,
    `    expect(session).toMatchObject({ token: "t_9f2" });`,
    `  });`,
    ``,
    `  it("rejects an unknown token", async () => {`,
    `    expect(await verifySession("nope")).toBe(null);`,
    `  });`,
    `});`,
    ``,
  ].join("\n"),
};

/** The pre-turn snapshot: HEAD plus the seed the agent started from. */
const AGENT_FILES: Record<string, string> = { ...HEAD_FILES };

/**
 * The working tree after the turn. The regression test below is the hunk the
 * IDE screenshot shows, with real line numbers and real added/removed rows.
 */
const TURN_AFTER: Record<string, string> = {
  "tests/setup.ts": `import { afterAll, beforeAll, beforeEach, vi } from "vitest";

import { db } from "../src/db/pool";

beforeAll(async () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-03-04T12:00:00Z"));
});

beforeEach(async () => {
  await db.session.deleteMany();
  await db.session.insert({
    token: "t_seed",
    userId: "user_seed",
    expiresAt: Date.now() + 30_000,
  });
});

afterAll(() => {
  vi.useRealTimers();
});
`,
  "src/auth/session.test.ts": [
    `import { beforeEach, describe, expect, it, vi } from "vitest";`,
    ``,
    `import { findSession, mintToken, seedSession } from "../auth/token";`,
    `import { verifySession } from "../auth/session";`,
    `import { db } from "../db/pool";`,
    ``,
    `const clock = vi.useFakeTimers({ now: new Date("2026-03-04T12:00:00Z") });`,
    ``,
    `const LIVE = {`,
    `  token: "t_9f2",`,
    `  userId: "user_9f2",`,
    `  expiresAt: Date.now() + 30_000,`,
    `};`,
    ``,
    `beforeEach(async () => {`,
    `  vi.setSystemTime(new Date("2026-03-04T12:00:00Z"));`,
    `  await db.session.deleteMany();`,
    `  await seedSession(LIVE);`,
    `});`,
    ``,
    `describe("token helpers", () => {`,
    `  it("mints an opaque token", () => {`,
    `    expect(mintToken()).toMatch(/^t_[0-9a-f]{3}$/);`,
    `  });`,
    ``,
    `  it("pins the token to the session", async () => {`,
    `    const session = await findSession("t_9f2");`,
    `    expect(session?.userId).toBe("user_9f2");`,
    `  });`,
    ``,
    `  it("rejects a token without a session", async () => {`,
    `    expect(await findSession("t_none")).toBeNull();`,
    `  });`,
    `});`,
    ``,
    `describe("verifySession", () => {`,
    `  beforeEach(async () => {`,
    `    await seedSession({ token: "t_9f2", ttl: 30_000 });`,
    `  });`,
    ``,
    `  it("returns the session before expiry", async () => {`,
    `    const session = await verifySession("t_9f2");`,
    `    expect(session).toMatchObject({ token: "t_9f2" });`,
    `  });`,
    ``,
    `  // Regression: the pool used to read Date.now() at import time.`,
    `  it("is still valid at the 30 s boundary", async () => {`,
    `    clock.advanceTimersByTime(29_999);`,
    `    expect(await verifySession("t_9f2")).not.toBeNull();`,
    `    clock.advanceTimersByTime(1);`,
    `    expect(await verifySession("t_9f2")).toBeNull();`,
    `  });`,
    ``,
    `  it("rejects an unknown token", async () => {`,
    `    expect(await verifySession("nope")).toBeNull();`,
    `  });`,
    `});`,
    ``,
  ].join("\n"),
};

/** The change set the prototype's IDE footer shows. */
const TURN_CHANGES = [
  { path: "tests/setup.ts", added: 12, removed: 3 },
  { path: "src/auth/session.test.ts", added: 6, removed: 1 },
];

const WORKTREE_FILES: Record<string, string> = { ...AGENT_FILES, ...TURN_AFTER };

const BINARY = new Set<string>();

function etag(text: string): string {
  // FNV-1a, stable and short; the real daemon uses BLAKE3.
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
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
      entries.push({
        name,
        path: join(path, name),
        kind: "directory",
        size_bytes: 0,
        modified_at: new Date().toISOString(),
        git_status: null,
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
        bytes_base64: null,
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
        bytes_base64: null,
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
      bytes_base64: null,
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
    if (request.base_etag !== null && current === null) {
      return {
        path: request.path,
        etag: "",
        size_bytes: 0,
        modified_at: null,
        conflict: {
          expected_etag: request.base_etag,
          current_etag: null,
          current_content: null,
          current_encoding: "utf-8",
        },
      };
    }
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
      label: "Turn 4 · Claude Code",
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
      files: TURN_CHANGES.map(({ path, added, removed }) => {
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
