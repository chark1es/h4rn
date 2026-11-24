import { CheckIcon, CopyIcon, LinkIcon, LoaderCircleIcon, ShieldIcon } from "lucide-react";
import { useEffect, useState } from "react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useNow } from "@/hooks/use-now";
import { absoluteDateTime } from "@/lib/format";
import { pushToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import {
  closeGuestShare,
  copyGuestLink,
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

function expiryText(expiresAt: string, now: number): string {
  const left = new Date(expiresAt).getTime() - now;
  if (!Number.isFinite(left) || left <= 0) return "Expired";
  const minutes = Math.round(left / 60_000);
  if (minutes < 60) return `Expires in ${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  if (hours < 24) return rest ? `Expires in ${hours} h ${rest} min` : `Expires in ${hours} h`;
  return `Expires ${absoluteDateTime(expiresAt)}`;
}

export function GuestShareDialog() {
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
        title: "Copy the link from the field above.",
        description: "This browser would not let h4rn write to the clipboard.",
      });
    }
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeGuestShare())}>
      <DialogContent className="max-w-[520px]" aria-describedby="guest-share-description">
        <DialogHeader>
          <DialogTitle>Share with guest</DialogTitle>
          <DialogDescription id="guest-share-description">
            One thread, one browser, no account. The link expires after 24 hours, and every message
            the guest writes waits for you.
          </DialogDescription>
        </DialogHeader>

        {!thread || !threadId ? null : settingsEnabled === false ? (
          <div style={{ display: "grid", gap: 12 }}>
            <div className="q-guest-off">
              <ShieldIcon className="q-icon s16" strokeWidth={1.5} aria-hidden />
              <div>
                <div style={{ fontWeight: 500 }}>Guest links are off.</div>
                <p className="q-muted" style={{ margin: "2px 0 0" }}>
                  Turn on Settings → Labs → Guest links for this machine. While it is off, every
                  existing link stops working.
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
                Turn on guest links
              </button>
              <button type="button" className="q-btn ghost" onClick={closeGuestShare}>
                Close
              </button>
            </DialogFooter>
          </div>
        ) : (
          <div style={{ display: "grid", gap: 14 }}>
            <div className="q-field" style={{ padding: 0, border: 0 }}>
              <div className="lab">
                <div className="t">Thread</div>
                <div className="s">{thread.title}</div>
              </div>
            </div>

            {loading && !sharing ? (
              <div className="q-faint" aria-live="polite" style={{ fontSize: 12 }}>
                Reading the current link…
              </div>
            ) : link ? (
              <>
                <div style={{ display: "grid", gap: 6 }}>
                  <span className="q-faint" style={{ fontSize: 12 }}>
                    Link
                  </span>
                  {link.url ? (
                    <div style={{ display: "flex", gap: 8 }}>
                      <input
                        id="guest-link-url"
                        aria-label="Guest link"
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
                        {copied ? "Copied" : "Copy"}
                      </button>
                    </div>
                  ) : link.no_serving_listener ? (
                    <p className="q-danger" role="note" style={{ margin: 0, fontSize: 12.5 }}>
                      This machine has no address a guest link can open on. Add a loopback listener,
                      or link this machine to Connect, then share this thread again. Until then the
                      link cannot be opened.
                    </p>
                  ) : (
                    <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                      The link is still valid for the guest, but this device cannot show its URL.
                      Only an admin device can copy it; after a daemon restart, create a new link to
                      copy it again. The old one stops working.
                    </p>
                  )}
                  <div className="q-faint" style={{ fontSize: 11.5 }}>
                    {expiryText(link.expires_at, now)} ·{" "}
                    {link.bound ? "Bound to the first browser" : "First browser that opens it"}
                  </div>
                  {link.url && isLoopbackLink(link.url) ? (
                    <p className="q-danger" role="note" style={{ margin: 0, fontSize: 12 }}>
                      This link only works on this machine. Turn on Connect and share again to send
                      a link to a teammate on another machine. A direct link is never sent over a
                      plain network connection.
                    </p>
                  ) : null}
                </div>
                <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                  The guest sees this thread&rsquo;s messages, the agent&rsquo;s replies and its
                  tool output, including anything the agent printed. They cannot see diffs, other
                  threads, files or terminals, and they cannot answer the agent for you.
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
                    Stop sharing
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
                    New link
                  </button>
                </DialogFooter>
              </>
            ) : (
              <>
                <p className="q-muted" style={{ margin: 0, fontSize: 12.5 }}>
                  Anyone with the link can follow this thread and suggest messages. You choose which
                  ones run.
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
                    Create link
                  </button>
                  <button type="button" className="q-btn ghost" onClick={closeGuestShare}>
                    Cancel
                  </button>
                </DialogFooter>
              </>
            )}

            {error ? (
              <p className="q-danger" role="alert" style={{ margin: 0, fontSize: 12 }}>
                {error}
              </p>
            ) : null}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
