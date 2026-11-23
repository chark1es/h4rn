import { beforeEach, describe, expect, it, vi } from "vitest";

import type { ImportProject, ImportScan } from "@/gen";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));
const { loadProjects } = vi.hoisted(() => ({ loadProjects: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  getState: () => ({ localHostId: "host_local" }),
  pushToast: vi.fn(),
}));

vi.mock("./store", () => ({ loadProjects }));

import {
  applyImportEvent,
  getImportState,
  loadImportScan,
  resetImportStoreForTests,
  selectAllImportProjects,
  selectNoImportProjects,
  setReviewAll,
  startImport,
  toggleImportProject,
  visibleProjects,
} from "./importStore";

function project(overrides: Partial<ImportProject> = {}): ImportProject {
  return {
    key: "path:/home/me/api",
    path: "/home/me/api",
    name: "api",
    git: true,
    last_active: "2026-11-20T10:00:00Z",
    conversations: 4,
    importable: 4,
    recommended: true,
    ...overrides,
  };
}

function scan(): ImportScan {
  return {
    projects: [
      project(),
      project({
        key: "path:/home/me/other",
        path: "/home/me/other",
        name: "other",
        git: false,
        conversations: 1,
        importable: 1,
        recommended: false,
      }),
      project({
        key: "github:acme/docs",
        path: "/home/me/docs",
        name: "acme/docs",
        conversations: 2,
        importable: 0,
        recommended: false,
      }),
    ],
    warnings: [],
    scanned_at: "2026-11-20T10:00:00Z",
  };
}

beforeEach(() => {
  resetImportStoreForTests();
  request.mockReset();
  loadProjects.mockReset();
});

describe("import discovery", () => {
  it("checks only recommended projects and can review everything", async () => {
    request.mockResolvedValueOnce({ type: "session_imports", scan: scan() });
    request.mockResolvedValueOnce({
      type: "session_import_status",
      progress: null,
    });
    await loadImportScan();
    expect(request).toHaveBeenCalledWith({
      type: "scan_session_imports",
      host: "host_local",
    });
    expect(getImportState().selected).toEqual(["path:/home/me/api"]);
    expect(visibleProjects(getImportState().scan, false)).toHaveLength(1);

    setReviewAll(true);
    expect(visibleProjects(getImportState().scan, true)).toHaveLength(3);
  });

  it("selects and clears every importable project in the current view", () => {
    resetImportStoreForTests({ scan: scan(), reviewAll: true });
    selectAllImportProjects();
    expect(getImportState().selected).toEqual(["path:/home/me/api", "path:/home/me/other"]);
    selectNoImportProjects();
    expect(getImportState().selected).toEqual([]);
  });

  it("toggles one project without touching the others", () => {
    resetImportStoreForTests({ scan: scan(), reviewAll: true });
    toggleImportProject("path:/home/me/other");
    expect(getImportState().selected).toEqual(["path:/home/me/other"]);
    toggleImportProject("path:/home/me/other");
    expect(getImportState().selected).toEqual([]);
  });

  it("shows an honest error when the scan fails", async () => {
    request.mockRejectedValueOnce(new Error("the daemon refused the scan"));
    await loadImportScan();
    expect(getImportState().error).toContain("the daemon refused the scan");
    expect(getImportState().scan).toBeNull();
  });
});

describe("import progress", () => {
  it("starts the selected projects and tracks the run", async () => {
    resetImportStoreForTests({
      scan: scan(),
      selected: ["path:/home/me/api"],
    });
    const progress = {
      import_id: "imp_1",
      running: true,
      projects_done: 0,
      projects_total: 1,
      conversations_done: 0,
      conversations_total: 4,
      imported: 0,
      skipped: 0,
      failed: 0,
      current_project: "api",
      warnings: [],
      finished_at: null,
    };
    request.mockResolvedValueOnce({ type: "session_import_status", progress });
    await startImport();
    expect(request).toHaveBeenCalledWith({
      type: "import_sessions",
      host: "host_local",
      projects: ["path:/home/me/api"],
    });
    expect(getImportState().progress?.import_id).toBe("imp_1");
  });

  it("counts imported, skipped and failed conversations", () => {
    applyImportEvent({
      import_id: "imp_1",
      running: true,
      projects_done: 1,
      projects_total: 2,
      conversations_done: 3,
      conversations_total: 8,
      imported: 2,
      skipped: 1,
      failed: 0,
      current_project: "api",
      warnings: ["one transcript was too large"],
      finished_at: null,
    });
    const state = getImportState();
    expect(state.progress?.imported).toBe(2);
    expect(state.progress?.skipped).toBe(1);
    expect(state.progress?.warnings).toHaveLength(1);
  });

  it("refreshes the list when a run finishes", async () => {
    request.mockResolvedValue({ type: "session_imports", scan: scan() });
    applyImportEvent({
      import_id: "imp_1",
      running: true,
      projects_done: 0,
      projects_total: 1,
      conversations_done: 0,
      conversations_total: 1,
      imported: 0,
      skipped: 0,
      failed: 0,
      current_project: "api",
      warnings: [],
      finished_at: null,
    });
    applyImportEvent({
      import_id: "imp_1",
      running: false,
      projects_done: 1,
      projects_total: 1,
      conversations_done: 1,
      conversations_total: 1,
      imported: 1,
      skipped: 0,
      failed: 0,
      current_project: null,
      warnings: [],
      finished_at: "2026-11-20T10:00:02Z",
    });
    await vi.waitFor(() => expect(loadProjects).toHaveBeenCalled());
    expect(getImportState().progress?.running).toBe(false);
  });

  it("shows the current counts when the dialog is reopened mid-run", async () => {
    const progress = {
      import_id: "imp_7",
      running: true,
      projects_done: 1,
      projects_total: 2,
      conversations_done: 37,
      conversations_total: 60,
      imported: 30,
      skipped: 7,
      failed: 0,
      current_project: "api",
      warnings: ["one transcript was too large"],
      finished_at: null,
    };
    request.mockResolvedValueOnce({ type: "session_imports", scan: scan() });
    request.mockResolvedValueOnce({ type: "session_import_status", progress });
    await loadImportScan();
    // The scan read is followed by the status read without another click.
    await vi.waitFor(() =>
      expect(request).toHaveBeenLastCalledWith({
        type: "session_import_status",
        host: "host_local",
      }),
    );
    await vi.waitFor(() => expect(getImportState().progress?.conversations_done).toBe(37));
    expect(getImportState().progress?.conversations_total).toBe(60);
  });

  it("keeps the scan's warnings for the panel", async () => {
    const withWarnings = scan();
    withWarnings.warnings = [
      "api holds more than 4 GiB of transcripts; older conversations were not scanned.",
    ];
    request.mockResolvedValueOnce({ type: "session_imports", scan: withWarnings });
    request.mockResolvedValueOnce({ type: "session_import_status", progress: null });
    await loadImportScan();
    expect(getImportState().scan?.warnings).toEqual([
      "api holds more than 4 GiB of transcripts; older conversations were not scanned.",
    ]);
  });
});
