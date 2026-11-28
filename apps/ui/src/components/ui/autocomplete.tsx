import {
  AutocompleteContent,
  AutocompleteInput,
  AutocompleteItem,
  AutocompleteList,
  Autocomplete as AutocompletePrimitive,
  AutocompleteTrigger,
} from "@ui-sdk/react/autocomplete";
import { ChevronDownIcon } from "lucide-react";
import type * as React from "react";

import { fieldClasses } from "@/components/ui/input";
import { itemRow, popupSurface } from "@/components/ui/menu";
import { cn } from "@/lib/utils";

type SuggestInputProps = {
  value: string;
  onValueChange: (value: string) => void;
  suggestions: readonly string[];
  /** The field's accessible name, unless a label points at `id`. */
  "aria-label"?: string | undefined;
  /** Names the button that shows every suggestion. */
  showLabel: string;
  id?: string | undefined;
  placeholder?: string | undefined;
  disabled?: boolean | undefined;
  /** Sets the text and suggestions in Geist Mono, for slugs like model ids. */
  mono?: boolean | undefined;
  className?: string | undefined;
  inputClassName?: string | undefined;
  onKeyDown?: React.KeyboardEventHandler<HTMLInputElement> | undefined;
};

/**
 * A text field with suggestions, like a datalist: any text is valid. While the
 * text is one of the suggestions, opening it lists them all.
 */
function SuggestInput({
  value,
  onValueChange,
  suggestions,
  showLabel,
  id,
  placeholder,
  disabled,
  mono,
  className,
  inputClassName,
  onKeyDown,
  ...aria
}: SuggestInputProps) {
  const exact = suggestions.includes(value.trim());
  return (
    <AutocompletePrimitive
      value={value}
      onValueChange={(next) => onValueChange(next)}
      filter={(item, query) =>
        exact || !query ? true : item.label.toLowerCase().includes(query.trim().toLowerCase())
      }
      disabled={disabled ?? false}
    >
      <div className={cn("relative min-w-0", className)}>
        <AutocompleteInput
          id={id}
          aria-label={aria["aria-label"]}
          placeholder={placeholder}
          spellCheck={false}
          autoComplete="off"
          onKeyDown={onKeyDown}
          className={cn(
            fieldClasses,
            "h-7 ps-2.5 pe-7 phone:h-11 phone:ps-3 phone:pe-10",
            mono && "font-mono placeholder:font-sans",
            inputClassName,
          )}
        />
        {/* The chevron sits where a Select trigger's does: inside the border,
            then the field's own end padding. */}
        <AutocompleteTrigger
          aria-label={showLabel}
          className="absolute inset-y-0 end-px inline-flex w-7 items-center justify-end rounded-e-md pe-2.5 text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring phone:w-10 phone:pe-3"
        >
          <ChevronDownIcon className="size-3.5" aria-hidden="true" />
        </AutocompleteTrigger>
      </div>
      <AutocompleteContent
        sideOffset={4}
        collisionPadding={8}
        className={cn(popupSurface, "min-w-(--anchor-width)")}
      >
        <AutocompleteList>
          {suggestions.map((suggestion) => (
            <AutocompleteItem
              key={suggestion}
              value={suggestion}
              // Text starts where the field's own text does (popup border and
              // padding plus this row's).
              className={cn(itemRow, "px-1.5 phone:px-2", mono && "font-mono")}
            >
              <span className="min-w-0 flex-1 truncate">{suggestion}</span>
            </AutocompleteItem>
          ))}
        </AutocompleteList>
      </AutocompleteContent>
    </AutocompletePrimitive>
  );
}

export { SuggestInput };
