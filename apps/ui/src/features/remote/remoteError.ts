import type { ConnectError } from "@/features/remote/connect";
import { i18n } from "@/lib/i18n";

/**
 * Why the last remote action failed, as a kind rather than words.
 *
 * The store keeps the kind and the component words it with
 * {@link remoteErrorText}, so a language switch relabels an error that is
 * already on screen. `sent` is text the relay, the sign-in provider or the
 * shell wrote: it is shown as sent.
 */
export type RemoteError =
  | { kind: "sent"; text: string }
  | { kind: "noSignInWindow" }
  | { kind: "callbackState" }
  | { kind: "signInDenied" }
  | { kind: "signInExpired" }
  | { kind: "noNoiseKey"; params: { name: string } }
  | { kind: "notPairingLink" }
  | { kind: "pairingExpired" }
  | { kind: "signInToScan" }
  | { kind: "signInToType" }
  | { kind: "otherConnect"; params: { url: string } }
  | { kind: "notApproved"; params: { name: string } }
  | { kind: "codesDiffer" }
  | { kind: "connectStatus"; params: { status: number } };

/** The error in the active language; `sent` text is returned as is. */
export function remoteErrorText(error: RemoteError): string {
  switch (error.kind) {
    case "sent":
      return error.text;
    case "noSignInWindow":
      return i18n.t("settings:connect.errors.noSignInWindow");
    case "callbackState":
      return i18n.t("settings:connect.errors.callbackState");
    case "signInDenied":
      return i18n.t("settings:connect.errors.denied");
    case "signInExpired":
      return i18n.t("settings:connect.errors.expired");
    case "noNoiseKey":
      return i18n.t("sidebar:hosts.errors.noNoiseKey", {
        name: error.params.name,
        command: "h4rnd login",
      });
    case "notPairingLink":
      return i18n.t("pairing:errors.notPairingLink");
    case "pairingExpired":
      return i18n.t("pairing:errors.expired");
    case "signInToScan":
      return i18n.t("pairing:errors.signInToScan");
    case "signInToType":
      return i18n.t("pairing:errors.signInToType");
    case "otherConnect":
      return i18n.t("pairing:errors.otherConnect", { url: error.params.url });
    case "notApproved":
      return i18n.t("pairing:errors.notApproved", { name: error.params.name });
    case "codesDiffer":
      return i18n.t("pairing:errors.codesDiffer");
    case "connectStatus":
      // An HTTP status is a code, not an amount: no digit grouping.
      return i18n.t("settings:connect.errors.status", { status: String(error.params.status) });
  }
}

/** A failure the client raises itself; it carries its kind to the store. */
export class RemoteFailure extends Error {
  readonly remote: RemoteError;

  constructor(remote: RemoteError) {
    super(remoteErrorText(remote));
    this.name = "RemoteFailure";
    this.remote = remote;
  }
}

/** The kind of any failure: the client's own, or the text that came with it. */
export function toRemoteError(error: unknown): RemoteError {
  if (error instanceof RemoteFailure) return error.remote;
  const connect = error as ConnectError;
  return { kind: "sent", text: connect?.hint ?? connect?.message ?? String(error) };
}
