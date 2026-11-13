import { useEffect } from "react";

import { AppShell } from "@/components/shell/AppShell";
import { startUsageWatch } from "@/features/usage/usageStore";
import { useKeyboardShortcuts } from "@/hooks/use-keyboard-shortcuts";
import { startConnection } from "@/lib/store/appStore";

export function App() {
  useKeyboardShortcuts();

  useEffect(() => {
    startConnection();
    startUsageWatch();
  }, []);

  return <AppShell />;
}
