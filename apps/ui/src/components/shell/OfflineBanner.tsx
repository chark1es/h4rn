import { RefreshCwIcon, WifiOffIcon } from "lucide-react";

import { absoluteTime } from "@/lib/format";
import { getClient, useApp } from "@/lib/store/appStore";

export function OfflineBanner({ inline = false }: { inline?: boolean }) {
  const status = useApp((state) => state.connection.status);
  const attempt = useApp((state) => state.connection.attempt);
  const error = useApp((state) => state.connection.error);
  const lastSyncAt = useApp((state) => state.lastSyncAt);

  // The first handshake is represented by the skeleton screens, not a banner.
  if (status === "online" || status === "connecting") return null;

  const cached = lastSyncAt !== null ? absoluteTime(new Date(lastSyncAt).toISOString()) : null;

  return (
    <div
      role="status"
      className="q-banner"
      style={inline ? { flex: "none", margin: "0 12px 8px" } : { flex: "none", marginTop: 8 }}
      title={error ? `${error.message}${error.hint ? ` ${error.hint}` : ""}` : undefined}
    >
      <span className="ic">
        <WifiOffIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        {status === "degraded" ? (
          <>
            <b>Reconnecting</b>
            {attempt > 0 ? ` · attempt ${attempt}` : ""}
            {cached ? ` · threads as of ${cached}` : ""}
          </>
        ) : (
          <>
            <b>Offline</b>
            {cached ? ` · threads as of ${cached}` : " · no cached threads"}
          </>
        )}
      </span>
      {status === "offline" ? (
        <button
          type="button"
          className="q-btn ghost"
          style={{ height: 24 }}
          onClick={() => getClient().reconnectNow()}
        >
          <RefreshCwIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          Reconnect
        </button>
      ) : null}
    </div>
  );
}
