// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const appState = {
  ui: { addProjectOpen: true },
  localHostId: "host_box",
  hosts: { host_box: { name: "box" } },
  connection: { status: "online" },
};

const importState = {
  selected: ["path:/code/api"],
  starting: true,
  progress: {
    import_id: "imp_1",
    running: true,
    projects_done: 0,
    projects_total: 1,
    conversations_done: 3,
    conversations_total: 9,
    imported: 3,
    skipped: 0,
    failed: 0,
    current_project: "api",
    warnings: [],
    finished_at: null,
  },
};

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: typeof appState) => unknown) => selector(appState),
  closeAddProject: vi.fn(),
  pushToast: vi.fn(),
}));

vi.mock("./store", () => ({
  shortPath: (path: string) => path,
  addProject: vi.fn(),
  browseDirectories: vi.fn(async () => ({
    listing: {
      path: "/code",
      parent: null,
      home: "/home/me",
      entries: [],
      git: false,
    },
  })),
}));

vi.mock("./importStore", () => ({
  useImportState: (selector: (state: typeof importState) => unknown) => selector(importState),
}));

vi.mock("./ImportPanel", () => ({
  ImportPanel: () => <div data-testid="import-panel" />,
  submitImport: vi.fn(),
}));

import { AddProjectDialog } from "./AddProjectDialog";

afterEach(cleanup);

describe("AddProjectDialog", () => {
  it("shows the import status instead of a spinner while a run is going", () => {
    render(<AddProjectDialog />);

    fireEvent.click(screen.getByRole("radio", { name: /Import/ }));

    expect(screen.getByTestId("import-panel")).toBeDefined();
    expect(screen.getByRole("button", { name: "Importing…" })).toBeDefined();
    expect(document.querySelector(".animate-spin")).toBeNull();
  });
});
