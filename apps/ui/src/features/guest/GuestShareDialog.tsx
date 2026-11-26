import { CheckIcon, CopyIcon, LinkIcon, LoaderCircleIcon, ShieldIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useNow } from "@/hooks/use-now";
import { formatDateTime, formatDuration, i18n } from "@/lib/i18n";
import { pushToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import {
  closeGuestShare,
  copyGuestLink,
  guestErrorText,
  loadGuestSharing,
  setGuestLinksEnabled,
  shareThreadWithGuest,
  stopGuestSharing,
  useGuest,
} from "./guestStore";

/** Whether a guest URL names this machine's own loopback. */
function isLoopbackLink(url: string): boolean {
  try {
    const host = new URL(url).hostname.replace(/^\[|\]$/g, "").toLowerCase();
    return host === "localhost" || host === "::1" || /^127\.\d+\.\d+\.\d+$/.test(host);
  } catch {
    return false;
  }
}

/** When a guest link stops working, to the minute within a day. */
export function expiryText(expiresAt: string, now: number): string {
  const left = new Date(expiresAt).getTime() - now;
  if (!Number.isFinite(left) || left <= 0) return i18n.t("dialogs:guest.expired");
  const minutes = Math.max(1, Math.round(left / 60_000));
  if (minutes < 24 * 60) {
    return i18n.t("dialogs:guest.expiresIn", { duration: formatDuration(minutes * 60_000) });
  }
  return i18n.t("dialogs:guest.expiresAt", { date: formatDateTime(expiresAt) });
}

export function GuestShareDialog() {
  const { t } = useTranslation(["dialogs", "common"]);
  const threadId = useGuest().dialogThreadId;
  const sharing = useGuest().sharing[threadId ?? ""];
  const loading = useGuest().sharingLoading[threadId ?? ""] ?? false;
  const creating = useGuest().creating[threadId ?? ""] ?? false;
  const stopping = useGuest().stopping[threadId ?? ""] ?? false;
  const settingsEnabled = useGuest().settingsEnabled;
  const settingsSaving = useGuest().settingsSaving;
  const error = useGuest().error;
  const thread = useApp((app) => (threadId ? app.threads[threadId] : undefined));
  const now = useNow(30_000);
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState<"settings" | "create" | "stop" | null>(null);

  useEffect(() => {
    if (!threadId) return;
    setCopied(false);
  }, [threadId]);

  useEffect(() => {
    if (threadId) void loadGuestSharing(threadId);
  }, [threadId]);

  const open = Boolean(threadId && thread);
  const link = sharing?.link ?? null;

  const copy = async () => {
    if (!link) return;
    const ok = await copyGuestLink(link);
    if (ok) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } else {
      pushToast({
        variant: "info",
        title: t("guest.copyFallback.title"),
        description: t("guest.copyFallback.description"),
      });
    }
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeGuestShare())}>
      <DialogContent className="max-w-[520px]" aria-describedby="guest-share-description">
        <DialogHeader>
          <DialogTitle>{t("guest.title")}</DialogTitle>
          <DialogDescription id="guest-share-description">
            {t("guest.description")}
          </DialogDescription>
        </DialogHeader>

        {!thread || !threadId ? null : settingsEnabled === false ? (
          <div style={{ display: "grid", gap: 12 }}>
            <div className="q-guest-off">
              <ShieldIcon className="q-icon s16" strokeWidth={1.5} aria-hidden />
              <div>
                <div style={{ fontWeight: 500 }}>{t("guest.off.title")}</div>
                <p className="q-muted" style={{ margin: "2px 0 0" }}>
                  {t("guest.off.body")}
                </p>
              </div>
            </div>
            <DialogFooter>
              <button
                type="button"
                className="q-btn secondary"
                disabled={busy === "settings" || settingsSaving}
                onClick={async () => {
                  setBusy("settings");
                  const next = await setGuestLinksEnabled(true);
                  setBusy(null);
                  if (next) await loadGuestSharing(threadId);
                }}
              >
                {busy === "settings" || settingsSaving ? (
                  <LoaderCircleIcon className="q-icon s14 q-spin" aria-hidden />
                ) : null}
                {t("guest.off.turnOn")}
              </button>
              <button type="button" className="q-btn ghost" onClick={closeGuestShare}>
                {t("common:close")}
              </button>
            </DialogFooter>
          </div>
        ) : (
          <div style={{ display: "grid", gap: 14 }}>
            <div className="q-field" style={{ padding: 0, border: 0 }}>
              <div className="lab">
                <div className="t">{t("guest.thread")}</div>
                <div className="s">{thread.title}</div>
              </div>
            </div>

            {loading && !sharing ? (
              <div className="q-faint" aria-live="polite" style={{ fontSize: 12 }}>
                {t("guest.reading")}
              </div>
            ) : link ? (
              <>
                <div style={{ display: "grid", gap: 6 }}>
                  <span className="q-faint" style={{ fontSize: 12 }}>
                    {t("guest.link")}
                  </span>
                  {link.url ? (
                    <div style={{ display: "flex", gap: 8 }}>
                      <input
                        id="guest-link-url"
                        aria-label={t("guest.linkLabel")}
                        className="q-input q-mono"
                        style={{ fontSize: 11.5 }}
                        value={link.url}
                        readOnly
                        onFocus={(event) => event.currentTarget.select()}
                      />
                      <button
                        type="button"
                        className={cn("q-btn", copied ? "secondary" : "primary")}
                        onClick={() => void copy()}
                        aria-live="polite"
                      >
                        {copied ? (
                          <CheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                        ) : (
                          <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                        )}
                        {copied ? t("common:copied") : t("common:copy")}
                      </button>
                    </div>
                  ) : link.no_serving_listener ? (
                    <p className="q-danger" role="note" style={{ margin: 0, fontSize: 12.5 }}>
                      {t("guest.noListener")}
                    </p>
                  ) : (
                    <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                      {t("guest.noUrl")}
                    </p>
                  )}
                  <div className="q-faint" style={{ fontSize: 11.5 }}>
                    {expiryText(link.expires_at, now)}
                    {link.no_serving_listener ? null : (
                      <> · {link.bound ? t("guest.bound") : t("guest.unbound")}</>
                    )}
                  </div>
                  {link.url && isLoopbackLink(link.url) ? (
                    <p className="q-danger" role="note" style={{ margin: 0, fontSize: 12 }}>
                      {t("guest.loopback")}
                    </p>
                  ) : null}
                </div>
                <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                  {t("guest.sees")}
                </p>
                <DialogFooter>
                  <button
                    type="button"
                    className="q-btn ghost q-danger"
                    disabled={stopping || creating || busy !== null}
                    onClick={async () => {
                      setBusy("stop");
                      await stopGuestSharing(threadId);
                      setBusy(null);
                    }}
                  >
                    {stopping ? (
                      <LoaderCircleIcon className="q-icon s14 q-spin" aria-hidden />
                    ) : null}
                    {t("guest.stop")}
                  </button>
                  <button
                    type="button"
                    className="q-btn secondary"
                    disabled={creating || stopping || busy !== null}
                    onClick={async () => {
                      setBusy("create");
                      const created = await shareThreadWithGuest(threadId);
                      setBusy(null);
                      if (created) setCopied(false);
                    }}
                  >
                    {creating ? (
                      <LoaderCircleIcon className="q-icon s14 q-spin" aria-hidden />
                    ) : (
                      <LinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                    )}
                    {t("guest.newLink")}
                  </button>
                </DialogFooter>
              </>
            ) : (
              <>
                <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                  {t("guest.anyone")}
                </p>
                <DialogFooter>
                  <button
                    type="button"
                    className="q-btn primary"
                    disabled={creating || busy !== null}
                    onClick={async () => {
                      setBusy("create");
                      const created = await shareThreadWithGuest(threadId);
                      setBusy(null);
                      if (created) setCopied(false);
                    }}
                  >
                    {creating ? (
                      <LoaderCircleIcon className="q-icon s14 q-spin" aria-hidden />
                    ) : (
                      <LinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                    )}
                    {t("guest.create")}
                  </button>
                  <button type="button" className="q-btn ghost" onClick={closeGuestShare}>
                    {t("common:cancel")}
                  </button>
                </DialogFooter>
              </>
            )}

            {error ? (
              <p className="q-danger" role="alert" style={{ margin: 0, fontSize: 12 }}>
                {guestErrorText(error)}
              </p>
            ) : null}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
