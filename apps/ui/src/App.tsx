import { useEffect } from "react";

import { AppShell } from "@/components/shell/AppShell";
import { initializeRemote } from "@/features/remote/remoteStore";
import { initializeSubagents } from "@/features/subagents/subagentStore";
import { startUsageWatch } from "@/features/usage/usageStore";
import { useKeyboardShortcuts } from "@/hooks/use-keyboard-shortcuts";
import { startConnection } from "@/lib/store/appStore";

export function App() {
  useKeyboardShortcuts();

  useEffect(() => {
    startConnection();
    startUsageWatch();
    initializeRemote();
    initializeSubagents();
  }, []);

  return <AppShell />;
}
