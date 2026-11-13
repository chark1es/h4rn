import { useEffect } from "react";

import { AppShell } from "@/components/shell/AppShell";
import { useKeyboardShortcuts } from "@/hooks/use-keyboard-shortcuts";
import { startConnection } from "@/lib/store/appStore";

export function App() {
  useKeyboardShortcuts();

  useEffect(() => {
    startConnection();
  }, []);

  return <AppShell />;
}
