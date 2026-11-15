/**
 * Host choices for a surface that can only start work on one host.
 *
 * `CreateThread` rejects a host other than the daemon's own (`threads can only
 * run on this daemon's host`) and `spawn_subagent` rejects a host other than
 * the parent's. The forms therefore show the other online hosts but disable
 * them with a reason, so a user never submits a request the daemon will refuse.
 */
export type ThreadHostOption = {
  value: string;
  label: string;
  disabled?: boolean;
  hint?: string;
};

type HostChoice = { host_id: string; name: string };

/** Marks every host except `localHostId` as disabled with `hint`. */
export function localOnlyHostOptions(
  hosts: readonly HostChoice[],
  localHostId: string | null,
  hint: string,
): ThreadHostOption[] {
  return hosts.map((host) =>
    localHostId === null || host.host_id === localHostId
      ? { value: host.host_id, label: host.name }
      : { value: host.host_id, label: host.name, disabled: true, hint },
  );
}

/** The host a new form should start on: this daemon's host when it is online. */
export function defaultThreadHostId(
  hosts: readonly HostChoice[],
  localHostId: string | null,
): string | null {
  const local = hosts.find((host) => host.host_id === localHostId);
  return (local ?? hosts[0])?.host_id ?? null;
}
