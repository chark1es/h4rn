/** What the sidebar footer says about the hosts, and which icon goes with it. */

import { formatNumber, i18n } from "@/lib/i18n";

export type HostStatusRow = {
  name: string;
  online: boolean;
  /** A WSL distro or similar, shown under its machine. */
  sub: boolean;
  /** Connect machine id, when the row belongs to a machine. */
  machineId: string | null;
};

export type HostStatus = {
  text: string;
  /** `dot` when something is reachable, `off` when nothing is. */
  icon: "orb" | "dot" | "off";
};

/**
 * One host reads as its own name (the design's "atlas"); several read as
 * "3 of 4 hosts", counting what the person actually has, in the active
 * language. Machines and hosts
 * are different nouns: a machine with WSL distros is several hosts.
 */
export function hostStatus(input: {
  loading: boolean;
  offline: boolean;
  rows: readonly HostStatusRow[];
}): HostStatus {
  const { loading, offline, rows } = input;
  if (loading) return { text: i18n.t("sidebar:status.connecting"), icon: "orb" };
  if (offline) return { text: i18n.t("sidebar:status.offline"), icon: "off" };
  if (rows.length === 0) return { text: i18n.t("sidebar:status.none"), icon: "off" };
  if (rows.length === 1) {
    const only = rows[0] as HostStatusRow;
    return { text: only.name, icon: only.online ? "dot" : "off" };
  }
  const online = rows.filter((row) => row.online).length;
  const machines = new Set(rows.map((row) => row.machineId).filter(Boolean)).size;
  const noun = rows.some((row) => row.sub) || rows.length !== machines ? "hosts" : "machines";
  return {
    text: i18n.t(`sidebar:status.${noun}`, { online: formatNumber(online), count: rows.length }),
    icon: online > 0 ? "dot" : "off",
  };
}
