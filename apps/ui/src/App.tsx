import { useEffect } from "react";

import { AppShell } from "@/components/shell/AppShell";
import { startDevicePush } from "@/features/notifications/devicePush";
import { startNotifications } from "@/features/notifications/runtime";
import { initializeRemote } from "@/features/remote/remoteStore";
import { initializeSubagents } from "@/features/subagents/subagentStore";
import { startUsageWatch } from "@/features/usage/usageStore";
import { useKeyboardShortcuts } from "@/hooks/use-keyboard-shortcuts";
import { startConnection } from "@/lib/store/appStore";

export function App() {
  useKeyboardShortcuts();

  useEffect(() => {
    startConnection();
    startNotifications();
    startDevicePush();
    startUsageWatch();
    initializeRemote();
    initializeSubagents();
  }, []);

  return <AppShell />;
}
