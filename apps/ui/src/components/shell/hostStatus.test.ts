import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { type HostStatusRow, hostStatus } from "./hostStatus";

const row = (name: string, over: Partial<HostStatusRow> = {}): HostStatusRow => ({
  name,
  online: true,
  sub: false,
  machineId: null,
  ...over,
});

describe("hostStatus", () => {
  it("names the one host a fresh install has", () => {
    expect(hostStatus({ loading: false, offline: false, rows: [row("atlas")] })).toEqual({
      text: "atlas",
      icon: "dot",
    });
  });

  it("shows a lone host that is down as unreachable", () => {
    expect(
      hostStatus({ loading: false, offline: false, rows: [row("atlas", { online: false })] }),
    ).toEqual({ text: "atlas", icon: "off" });
  });

  it("counts several hosts honestly", () => {
    const rows = [
      row("atlas", { machineId: "m1" }),
      row("Ubuntu-24.04", { sub: true, machineId: "m1" }),
      row("orion", { machineId: "m2" }),
      row("buildbox", { online: false, machineId: "m3" }),
    ];
    expect(hostStatus({ loading: false, offline: false, rows })).toEqual({
      text: "3 of 4 hosts",
      icon: "dot",
    });
  });

  it("says machines when no machine has a sub-host", () => {
    const rows = [row("atlas", { machineId: "m1" }), row("orion", { machineId: "m2" })];
    expect(hostStatus({ loading: false, offline: false, rows }).text).toBe("2 of 2 machines");
  });

  it("loading, offline and none outrank the count", () => {
    const rows = [row("atlas"), row("orion")];
    expect(hostStatus({ loading: true, offline: false, rows })).toEqual({
      text: "Connecting…",
      icon: "orb",
    });
    expect(hostStatus({ loading: false, offline: true, rows }).text).toBe("Offline");
    expect(hostStatus({ loading: false, offline: false, rows: [] })).toEqual({
      text: "No hosts",
      icon: "off",
    });
  });

  it("speaks the active language", async () => {
    const rows = [
      row("atlas", { machineId: "m1" }),
      row("Ubuntu-24.04", { sub: true, machineId: "m1" }),
      row("orion", { online: false, machineId: "m2" }),
    ];
    await setLocalePreference("zh-CN");
    expect(hostStatus({ loading: false, offline: false, rows }).text).toBe("2/3 台主机");
    await setLocalePreference("es");
    expect(hostStatus({ loading: false, offline: false, rows }).text).toBe("2 de 3 hosts");
    expect(hostStatus({ loading: false, offline: true, rows }).text).toBe("Sin conexión");
  });

  it("formats both counts as numbers in the active language", async () => {
    const rows = Array.from({ length: 12_000 }, (_, index) =>
      row(`h${index}`, { machineId: `m${index}`, online: index > 0 }),
    );
    expect(hostStatus({ loading: false, offline: false, rows }).text).toBe(
      "11,999 of 12,000 machines",
    );
    await setLocalePreference("es");
    expect(hostStatus({ loading: false, offline: false, rows }).text).toBe(
      "11.999 de 12.000 máquinas",
    );
  });
});

afterEach(async () => {
  await setLocalePreference("en");
});
