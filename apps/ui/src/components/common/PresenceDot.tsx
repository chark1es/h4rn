import { cn } from "@/lib/utils";

export function PresenceDot({ online, className }: { online: boolean; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-block size-1.5 shrink-0 rounded-full",
        online ? "bg-standby" : "bg-ink-faint/60",
        className,
      )}
    />
  );
}
