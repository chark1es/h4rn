import { type ToastT, toast } from "sonner";

export type ShownToast = {
  id: string | number;
  title: string;
  description?: string;
  variant?: string;
};

/** Every toast raised in this test file, oldest first, as the person saw it. */
export function shownToasts(): ShownToast[] {
  return toast
    .getHistory()
    .filter((entry): entry is ToastT => "title" in entry || "type" in entry)
    .map((entry) => ({
      id: entry.id,
      title: String(entry.title ?? ""),
      ...(typeof entry.description === "string" ? { description: entry.description } : {}),
      ...(entry.type ? { variant: entry.type } : {}),
    }));
}

export function lastToast(): ShownToast | undefined {
  return shownToasts().at(-1);
}
