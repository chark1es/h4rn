import { ServerIcon, SmartphoneIcon } from "lucide-react";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { EmptyState } from "@/components/common/EmptyState";
import { PresenceDot } from "@/components/common/PresenceDot";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, hostPlatformLabel, machineSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

export function HostsPane() {
  const hosts = useApp((state) => state.hosts);
  const loading = useApp((state) => state.loadingThreads);
  const now = useNow();
  const list = Object.values(hosts).sort((a, b) => {
    if (a.online !== b.online) return a.online ? -1 : 1;
    return a.name.localeCompare(b.name);
  });

  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-paper">
      <header className="flex h-12 shrink-0 items-center justify-between border-b border-line px-4">
        <h1 className="text-headline">Hosts</h1>
        <Button variant="secondary" size="sm" disabled title="Pairing arrives in Phase 2">
          <SmartphoneIcon aria-hidden />
          Pair a device
        </Button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[760px] px-4 py-4">
          {loading && list.length === 0 ? (
            <div className="space-y-2" role="status" aria-busy="true" aria-label="Loading hosts">
              {["a", "b", "c", "d"].map((key) => (
                <div
                  key={key}
                  className="h-16 animate-pulse rounded-md border border-line bg-panel"
                />
              ))}
            </div>
          ) : list.length === 0 ? (
            <EmptyState
              icon={ServerIcon}
              title="No hosts yet."
              description="A host is a machine running the h4rn daemon. Pair this device or add a host on your tailnet to give agents somewhere to run."
              action={
                <Button variant="secondary" disabled>
                  Pair a device
                </Button>
              }
            />
          ) : (
            <ul className="divide-y divide-line rounded-md border border-line bg-raised">
              {list.map((host) => (
                <li key={host.host_id} className="flex items-center gap-3 px-3 py-3">
                  <EntityAvatar
                    seed={machineSeed(host.host_id)}
                    kind="machine"
                    label={host.name}
                    size={28}
                    decorative={false}
                  />
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-1.5 text-compact font-medium text-ink">
                      {host.name}
                      {host.platform.startsWith("wsl:") ? (
                        <Badge variant="outline">WSL</Badge>
                      ) : null}
                    </p>
                    <p className="text-micro text-ink-muted">
                      {hostPlatformLabel(host.platform)} ·{" "}
                      {host.online
                        ? "online"
                        : `last seen ${relativeAge(host.last_seen ?? "", now)} ago`}
                    </p>
                  </div>
                  <div className="hidden flex-wrap items-center gap-1 sm:flex">
                    {host.harnesses.map((harness) => (
                      <Badge key={harness} variant="default">
                        {harnessLabel(harness)}
                      </Badge>
                    ))}
                  </div>
                  <span className="flex w-16 items-center justify-end gap-1.5 text-micro text-ink-muted">
                    <PresenceDot online={host.online} />
                    {host.online ? "online" : "offline"}
                  </span>
                  <span className="ml-1 font-mono text-micro tabular text-ink-faint">
                    {host.online ? relativeAge(host.last_seen ?? "", now) : "—"}
                  </span>
                </li>
              ))}
            </ul>
          )}

          <p className="mt-4 text-micro leading-relaxed text-ink-muted">
            Presence is a dot plus a word. A WSL distribution is its own host, so pairing the
            Windows host does not grant the distribution.
          </p>
        </div>
      </div>
    </main>
  );
}
