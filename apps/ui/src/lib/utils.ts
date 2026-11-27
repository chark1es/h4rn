import { type ClassValue, clsx } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

const twMerge = extendTailwindMerge({
  extend: {
    theme: {
      shadow: ["popup", "modal"],
      ease: ["enter", "exit"],
    },
    classGroups: {
      duration: [
        { duration: ["micro", "popup", "modal", "micro-exit", "popup-exit", "modal-exit"] },
      ],
    },
  },
});

/** Merge conditional class names, with Tailwind conflict resolution. */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
