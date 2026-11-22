import { beforeEach, describe, expect, it, vi } from "vitest";
import type { FileEntry, FsListing } from "@/gen";

import {
  clearWorkspaceFilesCache,
  loadWorkspaceFiles,
  MAX_INDEXED_FILES,
  readWorkspaceFilesCache,
  setWorkspaceFileListerForTests,
  walkWorkspace,
} from "./workspaceFiles";

function file(path: string): FileEntry {
  return {
    name: path.split("/").pop() ?? path,
    path,
    kind: "file",
    size_bytes: 10,
    modified_at: null,
    git_status: null,
  };
}

function directory(path: string): FileEntry {
  return {
    name: path.split("/").pop() ?? path,
    path,
    kind: "directory",
    size_bytes: 0,
    modified_at: null,
    git_status: null,
  };
}

function symlink(path: string): FileEntry {
  return {
    name: path.split("/").pop() ?? path,
    path,
    kind: "symlink",
    size_bytes: 0,
    modified_at: null,
    git_status: null,
  };
}

function listing(path: string, entries: FileEntry[]): FsListing {
  return { workspace: "/work/repo", path, entries, truncated: false };
}

const target = { host: "host_1", workspace: "/work/repo" };

beforeEach(() => {
  clearWorkspaceFilesCache();
  setWorkspaceFileListerForTests(null);
});

describe("workspace file walk", () => {
  it("collects files and prunes ignored directories and symlinks", async () => {
    const tree: Record<string, FileEntry[]> = {
      "": [directory("src"), directory("node_modules"), file("README.md"), symlink("link")],
      src: [file("src/main.ts"), directory("src/nested")],
      node_modules: [file("node_modules/pkg/index.js")],
      "src/nested": [file("src/nested/deep.ts")],
    };
    const lister = vi.fn(async ({ path }: { path: string }) => listing(path, tree[path] ?? []));

    const files = await walkWorkspace(target, lister);

    expect(files).toEqual(["README.md", "src/main.ts", "src/nested/deep.ts"]);
    expect(lister).not.toHaveBeenCalledWith(expect.objectContaining({ path: "node_modules" }));
  });

  it("keeps the files from directories that answered when one listing fails", async () => {
    const lister = async ({ path }: { path: string }) => {
      if (path === "broken") throw new Error("permission denied");
      if (path === "") return listing("", [directory("broken"), directory("src")]);
      return listing(path, [file(`${path}/file.ts`)]);
    };

    await expect(walkWorkspace(target, lister)).resolves.toEqual(["src/file.ts"]);
  });

  it("stops at the file cap", async () => {
    const many = Array.from({ length: MAX_INDEXED_FILES + 25 }, (_, index) =>
      file(`src/file-${index}.ts`),
    );
    const lister = async ({ path }: { path: string }) =>
      listing(path, path === "" ? [directory("src")] : many);

    await expect(walkWorkspace(target, lister)).resolves.toHaveLength(MAX_INDEXED_FILES);
  });

  it("does not descend past the depth cap", async () => {
    const lister = async ({ path }: { path: string }) => {
      const depth = path.split("/").filter(Boolean).length;
      return listing(path, [
        ...(depth < 20 ? [directory(path ? `${path}/next` : "next")] : []),
        file(path ? `${path}/file.ts` : "file.ts"),
      ]);
    };

    const files = await walkWorkspace(target, lister);
    expect(files).toContain("next/next/next/next/next/next/next/next/file.ts");
    expect(files.some((path) => path.split("/").length > 10)).toBe(false);
  });
});

describe("workspace file cache", () => {
  it("shares one walk between concurrent loads", async () => {
    const lister = vi.fn(async ({ path }: { path: string }) =>
      listing(path, path === "" ? [file("a.ts"), file("b.ts")] : []),
    );
    setWorkspaceFileListerForTests(lister);
    const [first, second] = await Promise.all([
      loadWorkspaceFiles(target),
      loadWorkspaceFiles(target),
    ]);
    expect(first).toEqual(["a.ts", "b.ts"]);
    expect(second).toEqual(["a.ts", "b.ts"]);
    expect(lister).toHaveBeenCalledTimes(1);
  });

  it("returns a completed index without walking again", async () => {
    const lister = vi.fn(async ({ path }: { path: string }) =>
      listing(path, path === "" ? [file("a.ts")] : []),
    );
    setWorkspaceFileListerForTests(lister);
    await loadWorkspaceFiles(target);
    await loadWorkspaceFiles(target);
    expect(lister).toHaveBeenCalledTimes(1);
    expect(readWorkspaceFilesCache(target)).toEqual(["a.ts"]);
  });
});
