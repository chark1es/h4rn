// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
// Starts i18n with the English catalogue, as main.tsx does in the app.
import "@/lib/i18n";

import type { ImportProgress, ImportScan } from "@/gen";

const mock = vi.hoisted(() => {
  const state: {
    scan: ImportScan | null;
    loading: boolean;
    error: string | null;
    selected: string[];
    reviewAll: boolean;
    progress: ImportProgress | null;
    starting: boolean;
  } = {
    scan: null,
    loading: false,
    error: null,
    selected: [],
    reviewAll: false,
    progress: null,
    starting: false,
  };
  return { state };
});

vi.mock("@/lib/client/frames", () => ({ onImportFrame: () => () => {} }));

vi.mock("./store", () => ({
  shortPath: (path: string) => path,
  loadProjects: vi.fn(),
}));

vi.mock("./importStore", () => ({
  useImportState: (selector: (value: typeof mock.state) => unknown) => selector(mock.state),
  loadImportScan: vi.fn(),
  applyImportEvent: vi.fn(),
  selectAllImportProjects: vi.fn(),
  selectNoImportProjects: vi.fn(),
  setReviewAll: vi.fn(),
  startImport: vi.fn(),
  toggleImportProject: vi.fn(),
  visibleProjects: (scan: ImportScan | null) => scan?.projects ?? [],
}));

import { ImportPanel } from "./ImportPanel";

afterEach(cleanup);

describe("ImportPanel", () => {
  it("shows the limits a scan hit and the projects it found", () => {
    mock.state.scan = {
      projects: [
        {
          key: "path:/code/api",
          path: "/code/api",
          name: "api",
          git: true,
          last_active: "2026-11-20T10:00:00Z",
          conversations: 4,
          importable: 2,
          recommended: true,
        },
      ],
      warnings: ["api holds more than 4 GiB of transcripts; older conversations were not scanned."],
      scanned_at: "2026-11-20T10:00:00Z",
    };

    render(<ImportPanel />);

    expect(screen.getByText("api")).toBeDefined();
    expect(screen.getByText(/api holds more than 4 GiB of transcripts/)).toBeDefined();
  });

  it("shows the current counts of a run reopened mid-import", () => {
    mock.state.scan = {
      projects: [
        {
          key: "path:/code/api",
          path: "/code/api",
          name: "api",
          git: true,
          last_active: "2026-11-20T10:00:00Z",
          conversations: 60,
          importable: 60,
          recommended: true,
        },
      ],
      warnings: [],
      scanned_at: "2026-11-20T10:00:00Z",
    };
    mock.state.progress = {
      import_id: "imp_1",
      running: true,
      projects_done: 0,
      projects_total: 1,
      conversations_done: 37,
      conversations_total: 60,
      imported: 30,
      skipped: 7,
      failed: 0,
      current_project: "api",
      warnings: [],
      finished_at: null,
    };

    render(<ImportPanel />);

    expect(screen.getByText("Importing 37 of 60…")).toBeDefined();
  });
});
