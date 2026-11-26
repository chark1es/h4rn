import type { ProtoError, ProtoErrorCode } from "@/gen";
import { i18n } from "@/lib/i18n";
import type enShell from "@/lib/i18n/locales/en/shell.json";

/** A typed protocol failure carried by a response frame. */
export class H4rnRequestError extends Error {
  readonly proto: ProtoError;
  readonly requestId: number | null;

  constructor(proto: ProtoError, requestId: number | null = null) {
    super(proto.message);
    this.name = "H4rnRequestError";
    this.proto = proto;
    this.requestId = requestId;
  }
}

export function protoError(
  code: ProtoErrorCode,
  message: string,
  hint: string | null = null,
): ProtoError {
  return { code, message, hint };
}

/** An error the client writes itself, named in `shell:errors.<kind>`. */
export type ClientErrorKind = keyof typeof enShell.errors;

/**
 * Which client-written error this is, kept beside the words.
 *
 * `message` is text that came with the failure (a socket's close reason, a
 * transport's own error): it is shown as sent, and only the hint is the
 * client's.
 */
export type ClientErrorSource = {
  kind: ClientErrorKind;
  params?: Record<string, string | number>;
  message?: string | undefined;
};

/**
 * A protocol error the client raised, rather than the daemon.
 *
 * It travels through every store like a daemon error, so `message` and `hint`
 * are worded in the language active when it was raised; a component that shows
 * one reads it through {@link errorWords}, which words the kind again in the
 * active language.
 */
export type ClientProtoError = ProtoError & { client: ClientErrorSource };

function clientWords(source: ClientErrorSource): { message: string; hint: string } {
  const params = source.params ?? {};
  return {
    message: source.message || i18n.t(`shell:errors.${source.kind}.message`, params),
    hint: i18n.t(`shell:errors.${source.kind}.hint`, params),
  };
}

/** Builds a client-written error from its kind. */
export function clientProto(
  code: ProtoErrorCode,
  kind: ClientErrorKind,
  options: { params?: Record<string, string | number>; message?: string | undefined } = {},
): ClientProtoError {
  const client: ClientErrorSource = { kind, ...options };
  return { code, ...clientWords(client), client };
}

/**
 * The words to show for any protocol error, in the active language.
 *
 * A daemon error is shown as sent. A client-written one is worded again from
 * its kind, so an error already on screen follows a language switch.
 */
export function errorWords(error: ProtoError): { message: string; hint: string | null } {
  const client = (error as Partial<ClientProtoError>).client;
  if (!client) return { message: error.message, hint: error.hint };
  return clientWords(client);
}

/** The kind behind a client-written error, or null for a daemon error. */
export function clientSource(error: ProtoError): ClientErrorSource | null {
  return (error as Partial<ClientProtoError>).client ?? null;
}

export function toProtoError(error: unknown): ProtoError {
  if (error instanceof H4rnRequestError) return error.proto;
  if (error instanceof Error) {
    return clientProto("internal", "unexpected", { message: error.message });
  }
  return clientProto("internal", "unreadable");
}
