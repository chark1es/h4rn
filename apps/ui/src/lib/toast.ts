import { toast } from "sonner";

export type ToastVariant = "error" | "info" | "success";

export type ToastId = string | number;

export type ToastAction = {
  label: string;
  onClick: () => void;
};

export type ToastInput = {
  title: string;
  description?: string;
  variant?: ToastVariant;
  ttlMs?: number;
  /** A follow-up the person can take from the toast, like Undo. */
  action?: ToastAction;
};

const SHOW = { error: toast.error, info: toast.info, success: toast.success } as const;

/** The one way the app raises a notification. Words are already translated. */
export function pushToast(input: ToastInput): ToastId {
  const { action } = input;
  return SHOW[input.variant ?? "info"](input.title, {
    ...(input.description ? { description: input.description } : {}),
    ...(input.ttlMs === undefined ? {} : { duration: input.ttlMs }),
    ...(action ? { action: { label: action.label, onClick: () => action.onClick() } } : {}),
  });
}

export function dismissToast(id: ToastId): void {
  toast.dismiss(id);
}

type Outcome<T> = string | ((value: T) => string);

/**
 * One toast for an async operation: loading while it runs, then success or
 * error in place. Resolves or rejects with the operation's own result.
 */
export function promiseToast<T>(
  operation: Promise<T> | (() => Promise<T>),
  words: { loading: string; success: Outcome<T>; error: Outcome<unknown> },
): Promise<T> {
  return toast.promise(operation, words).unwrap();
}
