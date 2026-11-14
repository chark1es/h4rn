import { CheckIcon, ChevronDownIcon } from "lucide-react";
import type { ReactNode } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

export type ChipOption<T extends string> = {
  value: T;
  label: string;
  /** Muted second line, for example a path or a hint. */
  hint?: string;
  icon?: ReactNode;
};

/**
 * The prototype's picker: a 24 px chip (radius 6, label, 12 px chevron) that
 * opens a menu. It replaces the native `<select>`, which draws OS chrome the
 * Quiet controls do not use.
 */
export function ChipSelect<T extends string>({
  value,
  options,
  onChange,
  label,
  id,
  placeholder = "Choose…",
  disabled,
  className,
  width,
  block,
}: {
  value: T | "";
  options: ChipOption<T>[];
  onChange: (value: T) => void;
  /** Accessible name; also the group label announced for the menu. */
  label: string;
  id?: string;
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  /** Fixed chip width in px; the chip otherwise hugs its label. */
  width?: number;
  /** Fill the field like an input (28 px) instead of hugging the label. */
  block?: boolean;
}) {
  const current = options.find((option) => option.value === value);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          id={id}
          className={`q-chip q-chip-select${block ? " block" : ""}${className ? ` ${className}` : ""}`}
          style={width ? { width, maxWidth: "100%" } : undefined}
          aria-label={`${label}: ${current?.label ?? placeholder}`}
          aria-haspopup="menu"
          disabled={disabled || options.length === 0}
        >
          {current?.icon}
          <span className="q-chip-label">{current?.label ?? placeholder}</span>
          <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-[160px] max-w-[320px]">
        {options.map((option) => (
          <DropdownMenuItem key={option.value} onSelect={() => onChange(option.value)}>
            {option.icon}
            <span className="min-w-0 flex-1">
              <span className="block truncate">{option.label}</span>
              {option.hint ? (
                <span className="block truncate text-[11.5px] text-fg-3">{option.hint}</span>
              ) : null}
            </span>
            {option.value === value ? (
              <CheckIcon className="q-icon s14 text-fg-3" strokeWidth={1.5} aria-hidden />
            ) : null}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
