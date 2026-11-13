import "@fontsource-variable/archivo";
import "@fontsource-variable/spline-sans-mono";
import "./styles/globals.css";
// Mobile-only pairing/deep-link/safe-area adapter; inert everywhere else.
import "./lib/platform/mobile";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { TooltipProvider } from "./components/ui/tooltip";

const container = document.getElementById("root");

if (!container) {
  throw new Error("Root element #root is missing from index.html");
}

createRoot(container).render(
  <StrictMode>
    <TooltipProvider>
      <App />
    </TooltipProvider>
  </StrictMode>,
);
