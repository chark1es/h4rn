import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./styles/globals.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { TooltipProvider } from "./components/ui/tooltip";
// UI translations: reads the saved language (or the system's) and loads its
// catalogue before the first render, so a non-English reader never sees an
// English flash. English is bundled; never rejects.
import { prepareI18n } from "./lib/i18n";
// Mobile-only pairing/deep-link/safe-area adapter; inert everywhere else. It
// also primes the platform secret store before the first synchronous config
// read, so no credential has to live in localStorage.
import { preparePlatform } from "./lib/platform/mobile";

const container = document.getElementById("root");

if (!container) {
  // i18n-ignore-next-line: index.html ships the root, and no catalogue is loaded yet
  throw new Error("Root element #root is missing from index.html");
}

const languageReady = prepareI18n();
void Promise.allSettled([preparePlatform(languageReady), languageReady]).finally(() => {
  createRoot(container).render(
    <StrictMode>
      <TooltipProvider>
        <App />
      </TooltipProvider>
    </StrictMode>,
  );
});
