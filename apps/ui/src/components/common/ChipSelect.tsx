import { type ReactNode, useRef } from "react";
import { useTranslation } from "react-i18next";

import { Select, SelectContent, SelectItem, SelectTrigger } from "@/components/ui/select";
import { cn } from "@/lib/utils";

export type ChipOption<T extends string> = {
  value: T;
  label: string;
  /** Muted second line, for example a path or a hint. */
  hint?: string;
  icon?: ReactNode;
  /** Shown but not selectable, for a path the daemon does not support yet. */
  disabled?: boolean;
};

/** A labelled Select over a short list of options, sized like an Input. */
export function ChipSelect<T extends string>({
  value,
  options,
  onChange,
  label,
  id,
  placeholder,
  disabled,
  className,
  block,
  size,
}: {
  value: T | "";
  options: ChipOption<T>[];
  /** Runs on every pick, including the option already chosen. */
  onChange: (value: T) => void;
  /** Names the control; the current choice follows it. */
  label: string;
  id?: string;
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  /** Fill the field instead of hugging the label. */
  block?: boolean;
  size?: "default" | "chip";
}) {
  const { t } = useTranslation("common");
  const changed = useRef(false);
  const withIcon = options.some((option) => option.icon);
  const current = options.find((option) => option.value === value);
  return (
    <Select<T>
      items={options.map((option) => ({
        value: option.value,
        label: option.label,
        ...(option.disabled ? { disabled: true } : {}),
      }))}
      value={value === "" ? null : value}
      placeholder={placeholder ?? t("choose")}
      disabled={disabled || options.length === 0}
      onValueChange={(next) => {
        if (next === null) return;
        changed.current = true;
        onChange(next);
      }}
      onOpenChange={(open, details) => {
        if (open) {
          changed.current = false;
          return;
        }
        // Choosing the current option changes no value, but callers still treat it as a pick.
        if (details.reason === "item-press" && !changed.current && value !== "") onChange(value);
      }}
    >
      <SelectTrigger
        id={id}
        aria-label={`${label}: ${current?.label ?? placeholder ?? t("choose")}`}
        size={size}
        withIcon={withIcon}
        className={cn(block ? "w-full" : "w-auto max-w-full", className)}
      />
      <SelectContent align="start" className="max-w-80">
        {options.map((option) => (
          <SelectItem<T>
            key={option.value}
            value={option.value}
            label={option.label}
            disabled={option.disabled}
            icon={option.icon}
            description={option.hint}
          >
            {option.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
