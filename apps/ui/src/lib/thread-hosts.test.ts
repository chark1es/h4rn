import { describe, expect, it } from "vitest";

import { defaultThreadHostId, localOnlyHostOptions } from "./thread-hosts";

const hosts = [
  { host_id: "alpha", name: "Alpha" },
  { host_id: "atlas", name: "Atlas" },
];

describe("localOnlyHostOptions", () => {
  it("disables every online host other than this daemon's host", () => {
    const options = localOnlyHostOptions(hosts, "atlas", "Not yet: new threads run on this host");
    expect(options).toEqual([
      {
        value: "alpha",
        label: "Alpha",
        disabled: true,
        hint: "Not yet: new threads run on this host",
      },
      { value: "atlas", label: "Atlas" },
    ]);
  });

  it("leaves the options alone when the local host is not known yet", () => {
    expect(localOnlyHostOptions(hosts, null, "unused")).toEqual([
      { value: "alpha", label: "Alpha" },
      { value: "atlas", label: "Atlas" },
    ]);
  });
});

describe("defaultThreadHostId", () => {
  it("prefers this daemon's host over the alphabetically first one", () => {
    expect(defaultThreadHostId(hosts, "atlas")).toBe("atlas");
  });

  it("falls back to the first host, or null when there is none", () => {
    expect(defaultThreadHostId(hosts, "offline-host")).toBe("alpha");
    expect(defaultThreadHostId([], "atlas")).toBeNull();
  });
});
