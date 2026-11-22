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

    const index = await walkWorkspace(target, lister);

    expect(index.files).toEqual(["README.md", "src/main.ts", "src/nested/deep.ts"]);
    expect(index.truncated).toBe(false);
    expect(lister).not.toHaveBeenCalledWith(expect.objectContaining({ path: "node_modules" }));
  });

  it("keeps the files from directories that answered but marks the index incomplete", async () => {
    const lister = async ({ path }: { path: string }) => {
      if (path === "broken") throw new Error("permission denied");
      if (path === "") return listing("", [directory("broken"), directory("src")]);
      return listing(path, [file(`${path}/file.ts`)]);
    };

    await expect(walkWorkspace(target, lister)).resolves.toEqual({
      files: ["src/file.ts"],
      // A listing that failed is not an empty directory: the picker says the
      // index is a prefix instead of claiming nothing else exists.
      truncated: true,
    });
  });

  it("rejects when the root listing fails so the picker can show its error", async () => {
    const lister = async ({ path }: { path: string }) => {
      if (path === "") throw new Error("The workspace could not be read.");
      return listing(path, []);
    };

    await expect(walkWorkspace(target, lister)).rejects.toThrow("The workspace could not be read.");
  });

  it("stops at the file cap", async () => {
    const many = Array.from({ length: MAX_INDEXED_FILES + 25 }, (_, index) =>
      file(`src/file-${index}.ts`),
    );
    const lister = async ({ path }: { path: string }) =>
      listing(path, path === "" ? [directory("src")] : many);

    const index = await walkWorkspace(target, lister);
    expect(index.files).toHaveLength(MAX_INDEXED_FILES);
    expect(index.truncated).toBe(true);
  });

  it("does not descend past the depth cap", async () => {
    const lister = async ({ path }: { path: string }) => {
      const depth = path.split("/").filter(Boolean).length;
      return listing(path, [
        ...(depth < 20 ? [directory(path ? `${path}/next` : "next")] : []),
        file(path ? `${path}/file.ts` : "file.ts"),
      ]);
    };

    const index = await walkWorkspace(target, lister);
    expect(index.files).toContain("next/next/next/next/next/next/next/next/file.ts");
    expect(index.files.some((path) => path.split("/").length > 10)).toBe(false);
    // Directories below the depth cap are left out, so the list is incomplete.
    expect(index.truncated).toBe(true);
  });

  it("reports a listing the daemon itself truncated", async () => {
    const lister = async ({ path }: { path: string }) => ({
      ...listing(path, path === "" ? [file("a.ts")] : []),
      truncated: true,
    });
    await expect(walkWorkspace(target, lister)).resolves.toEqual({
      files: ["a.ts"],
      truncated: true,
    });
  });

  it("reports the directory cap", async () => {
    const directories = Array.from({ length: 300 }, (_, index) => directory(`dir-${index}`));
    const lister = async ({ path }: { path: string }) =>
      listing(path, path === "" ? directories : []);
    const index = await walkWorkspace(target, lister);
    expect(index.truncated).toBe(true);
  });

  it("counts failed listings against the directory budget", async () => {
    const directories = Array.from({ length: 300 }, (_, index) => directory(`dir-${index}`));
    const lister = vi.fn(async ({ path }: { path: string }) => {
      if (path === "") return listing("", directories);
      throw new Error("permission denied");
    });

    const index = await walkWorkspace(target, lister);

    expect(index).toEqual({ files: [], truncated: true });
    // The root plus 249 children: failures spend the budget too, and no batch
    // may start more listings than the budget still allows.
    expect(lister).toHaveBeenCalledTimes(250);
  });

  it("never grows past the file cap across one batch", async () => {
    const siblings = Array.from({ length: 6 }, (_, index) => directory(`dir-${index}`));
    const lister = async ({ path }: { path: string }) => {
      if (path === "") return listing("", siblings);
      const files = Array.from({ length: 1_000 }, (_, index) => file(`${path}/file-${index}.ts`));
      return listing(path, files);
    };

    const index = await walkWorkspace(target, lister);

    // Six siblings of 1,000 files fit in one batch; the remaining siblings
    // must not add one file each after the cap was reached.
    expect(index.files).toHaveLength(MAX_INDEXED_FILES);
    expect(index.truncated).toBe(true);
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
    expect(first).toEqual({ files: ["a.ts", "b.ts"], truncated: false });
    expect(second).toEqual({ files: ["a.ts", "b.ts"], truncated: false });
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
    expect(readWorkspaceFilesCache(target)).toEqual({
      files: ["a.ts"],
      truncated: false,
    });
  });
});
