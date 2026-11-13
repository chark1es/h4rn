import { Blobatar } from "@blobatar/react";

import { cn } from "@/lib/utils";

type EntityKind = "machine" | "harness" | "subagent" | "thread" | "user";

const BACKGROUND: Record<EntityKind, "squircle" | "circle" | false> = {
  // The prototype draws the creature itself, never a plate. A background tile
  // turns into a white chip in dark mode and reads as a badge, not an identity.
  machine: false,
  harness: false,
  subagent: false,
  thread: false,
  user: false,
};

type EntityAvatarProps = {
  seed: string;
  kind: EntityKind;
  label: string;
  size?: number;
  className?: string;
  /** True when a text label sits next to the avatar. */
  decorative?: boolean;
};

/** A deterministic identity face. The face answers who; the orb answers what. */
export function EntityAvatar({
  seed,
  kind,
  label,
  size = 20,
  className,
  decorative = true,
}: EntityAvatarProps) {
  return (
    <Blobatar
      name={seed}
      size={size}
      background={BACKGROUND[kind]}
      title={label}
      className={cn("shrink-0", className)}
      aria-hidden={decorative ? true : undefined}
      aria-label={decorative ? undefined : label}
    />
  );
}
