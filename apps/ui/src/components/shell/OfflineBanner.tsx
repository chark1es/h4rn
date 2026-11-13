import { RefreshCwIcon, WifiOffIcon } from "lucide-react";
import { absoluteTime } from "@/lib/format";
import { getClient, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

export function OfflineBanner() {
  const status = useApp((state) => state.connection.status);
  const attempt = useApp((state) => state.connection.attempt);
  const error = useApp((state) => state.connection.error);
  const lastSyncAt = useApp((state) => state.lastSyncAt);

  if (status === "online") return null;

  const cached =
    lastSyncAt !== null
      ? ` Showing cached data from ${absoluteTime(new Date(lastSyncAt).toISOString())}.`
      : "";

  return (
    <div
      role="status"
      className={cn(
        "flex min-h-7 shrink-0 items-center gap-2 border-b px-3 py-1 text-micro",
        status === "offline"
          ? "border-strike/30 bg-strike-tint text-strike-ink"
          : "border-cue/30 bg-cue-tint text-cue-ink",
      )}
    >
      <WifiOffIcon className="size-3.5 shrink-0" aria-hidden />
      <span className="min-w-0 flex-1 truncate">
        {status === "connecting" ? (
          <>Connecting to the daemon…</>
        ) : status === "degraded" ? (
          <>
            <span className="font-medium">Reconnecting to the daemon…</span>
            {attempt > 0 ? ` attempt ${attempt}.` : ""}
            {cached}
          </>
        ) : (
          <>
            <span className="font-medium">Offline.</span>{" "}
            {error?.message ?? "The daemon is unreachable."}
            {cached}
          </>
        )}
      </span>
      {status === "offline" ? (
        <button
          type="button"
          className="flex shrink-0 items-center gap-1 font-medium underline underline-offset-2"
          onClick={() => getClient().reconnectNow()}
        >
          <RefreshCwIcon className="size-3" aria-hidden />
          Reconnect
        </button>
      ) : null}
    </div>
  );
}
