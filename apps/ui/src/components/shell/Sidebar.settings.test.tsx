// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import { installPopupStubs } from "@/test/popups";

vi.mock("@/features/harnesses/harnessStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/features/harnesses/harnessStore")>();
  return { ...actual, useHarnessVersions: () => undefined, useHarnesses: () => 3 };
});

import { getState, store } from "@/lib/store/appStore";
import { Sidebar } from "./Sidebar";

beforeAll(installPopupStubs);
afterEach(cleanup);

describe("settings sidebar", () => {
  it("shows how many harness updates are available", () => {
    store.setState({
      ...getState(),
      ui: {
        ...getState().ui,
        route: { kind: "placeholder", surface: "settings", path: "/settings/hosts" },
      },
    });
    render(<Sidebar />);
    expect(screen.getByRole("button", { name: /Harnesses\s*3/ })).toBeTruthy();
  });
});
