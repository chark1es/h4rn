import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./styles/globals.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { TooltipProvider } from "./components/ui/tooltip";
// Mobile-only pairing/deep-link/safe-area adapter; inert everywhere else. It
// also primes the platform secret store before the first synchronous config
// read, so no credential has to live in localStorage.
import { preparePlatform } from "./lib/platform/mobile";

const container = document.getElementById("root");

if (!container) {
  throw new Error("Root element #root is missing from index.html");
}

void preparePlatform().finally(() => {
  createRoot(container).render(
    <StrictMode>
      <TooltipProvider>
        <App />
      </TooltipProvider>
    </StrictMode>,
  );
});
