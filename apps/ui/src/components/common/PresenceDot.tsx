import { cn } from "@/lib/utils";

export function PresenceDot({ online, className }: { online: boolean; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-block size-1.5 shrink-0 rounded-full",
        online ? "bg-ok" : "bg-fg-3/60",
        className,
      )}
    />
  );
}
