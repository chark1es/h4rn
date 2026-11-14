/** What the sidebar footer says about the hosts, and which icon goes with it. */

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
 * "3 of 4 hosts", counting what the person actually has. Machines and hosts
 * are different nouns: a machine with WSL distros is several hosts.
 */
export function hostStatus(input: {
  loading: boolean;
  offline: boolean;
  rows: readonly HostStatusRow[];
}): HostStatus {
  const { loading, offline, rows } = input;
  if (loading) return { text: "Connecting…", icon: "orb" };
  if (offline) return { text: "Offline", icon: "off" };
  if (rows.length === 0) return { text: "No hosts", icon: "off" };
  if (rows.length === 1) {
    const only = rows[0] as HostStatusRow;
    return { text: only.name, icon: only.online ? "dot" : "off" };
  }
  const online = rows.filter((row) => row.online).length;
  const machines = new Set(rows.map((row) => row.machineId).filter(Boolean)).size;
  const noun = rows.some((row) => row.sub) || rows.length !== machines ? "hosts" : "machines";
  return { text: `${online} of ${rows.length} ${noun}`, icon: online > 0 ? "dot" : "off" };
}
