import { useEffect, useState } from "react";

/** Delay a query; an empty query settles immediately so closing resets at once. */
export function useDebouncedValue(value: string, delay: number): string {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    if (value === "") {
      setSettled("");
      return;
    }
    const timer = setTimeout(() => setSettled(value), delay);
    return () => clearTimeout(timer);
  }, [value, delay]);
  return value === "" ? "" : settled;
}
