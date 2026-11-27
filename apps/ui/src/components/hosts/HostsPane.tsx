import { ServerIcon, SmartphoneIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { EmptyState } from "@/components/common/EmptyState";
import { PresenceDot } from "@/components/common/PresenceDot";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, hostPlatformLabel, machineSeed } from "@/lib/entities";
import { formatAge, formatRelativeTime } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";

export function HostsPane() {
  const { t } = useTranslation(["pairing", "sidebar"]);
  const hosts = useApp((state) => state.hosts);
  const loading = useApp((state) => state.loadingThreads);
  const now = useNow();
  const list = Object.values(hosts).sort((a, b) => {
    if (a.online !== b.online) return a.online ? -1 : 1;
    return a.name.localeCompare(b.name);
  });

  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-bg">
      <header className="flex h-12 shrink-0 items-center justify-between border-b border-line px-4">
        <h1 className="text-headline">{t("sidebar:hosts.title")}</h1>
        <Button variant="secondary" size="sm" disabled title={t("hostsPane.pairingLater")}>
          <SmartphoneIcon aria-hidden />
          {t("hostsPane.pairDevice")}
        </Button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[760px] px-4 py-4">
          {loading && list.length === 0 ? (
            <div
              className="space-y-2"
              role="status"
              aria-busy="true"
              aria-label={t("hostsPane.loading")}
            >
              {["a", "b", "c", "d"].map((key) => (
                <div
                  key={key}
                  className="h-16 animate-pulse rounded-lg border border-line bg-bg-side"
                />
              ))}
            </div>
          ) : list.length === 0 ? (
            <EmptyState
              icon={ServerIcon}
              title={t("hostsPane.emptyTitle")}
              description={t("hostsPane.emptyBody")}
              action={
                <Button variant="secondary" disabled>
                  {t("hostsPane.pairDevice")}
                </Button>
              }
            />
          ) : (
            <ul className="divide-y divide-line rounded-lg border border-line bg-bg-raised">
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
                    <p className="flex items-center gap-1.5 text-compact font-medium text-fg">
                      {host.name}
                      {host.platform.startsWith("wsl:") ? (
                        <Badge variant="outline">WSL</Badge>
                      ) : null}
                    </p>
                    <p className="text-small text-fg-2">
                      {hostPlatformLabel(host.platform)} ·{" "}
                      {host.online
                        ? t("sidebar:hosts.dot.online")
                        : t("hostsPane.lastSeen", {
                            when: formatRelativeTime(host.last_seen, now),
                          })}
                    </p>
                  </div>
                  <div className="hidden flex-wrap items-center gap-1 sm:flex">
                    {host.harnesses.map((harness) => (
                      <Badge key={harness} variant="default">
                        {harnessLabel(harness)}
                      </Badge>
                    ))}
                  </div>
                  <span className="flex w-16 items-center justify-end gap-1.5 text-small text-fg-2">
                    <PresenceDot online={host.online} />
                    {host.online ? t("sidebar:hosts.dot.online") : t("sidebar:hosts.dot.offline")}
                  </span>
                  <span className="ml-1 text-small tabular text-fg-3">
                    {host.online ? formatAge(host.last_seen ?? "", now) : "—"}
                  </span>
                </li>
              ))}
            </ul>
          )}

          <p className="mt-4 text-small leading-relaxed text-fg-2">{t("hostsPane.note")}</p>
        </div>
      </div>
    </main>
  );
}
