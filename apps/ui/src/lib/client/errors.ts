import type { ProtoError, ProtoErrorCode } from "@/gen";

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

export function toProtoError(error: unknown): ProtoError {
  if (error instanceof H4rnRequestError) return error.proto;
  if (error instanceof Error) {
    return {
      code: "internal",
      message: error.message,
      hint: "Try again. If this keeps happening, open the connection details in the status bar.",
    };
  }
  return {
    code: "internal",
    message: "The daemon returned something the client could not read.",
    hint: "Try again. If it persists, update h4rn.",
  };
}
