import { CircleAlertIcon, LoaderCircleIcon, RefreshCwIcon, WifiOffIcon } from "lucide-react";
import { type ButtonHTMLAttributes, useCallback, useEffect, useRef, useState } from "react";

import type { H4rnClient } from "@/lib/client/client";
import { cn } from "@/lib/utils";

import { TerminalController, type TerminalStatus } from "./terminal-controller";

const CTRL_ARROWS: Record<string, string> = {
  ArrowUp: "\u001b[1;5A",
  ArrowDown: "\u001b[1;5B",
  ArrowRight: "\u001b[1;5C",
  ArrowLeft: "\u001b[1;5D",
};

const ARROWS: Record<string, string> = {
  ArrowUp: "\u001b[A",
  ArrowDown: "\u001b[B",
  ArrowRight: "\u001b[C",
  ArrowLeft: "\u001b[D",
};

export function TerminalView({
  client,
  threadId,
  host,
  cwd,
  active,
  onTitle,
  onController,
}: {
  client: H4rnClient;
  threadId: string | null;
  host: string;
  cwd: string;
  active: boolean;
  onTitle?: (title: string) => void;
  onController?: (controller: TerminalController | null) => void;
}) {
  const surfaceRef = useRef<HTMLDivElement>(null);
  const controllerRef = useRef<TerminalController | null>(null);
  const titleRef = useRef(onTitle);
  titleRef.current = onTitle;
  const controllerCallbackRef = useRef(onController);
  controllerCallbackRef.current = onController;

  const [status, setStatus] = useState<TerminalStatus>({ phase: "opening" });
  const statusRef = useRef(status);
  statusRef.current = status;
  const [attempt, setAttempt] = useState(0);
  const [ctrlArmed, setCtrlArmed] = useState(false);

  // `attempt` is a retry counter: bumping it tears the controller down and
  // starts a fresh xterm/session, which is the only reason it is a dependency.
  // biome-ignore lint/correctness/useExhaustiveDependencies: attempt remounts the controller
  useEffect(() => {
    const element = surfaceRef.current;
    if (!element) return;
    const controller = new TerminalController();
    controllerRef.current = controller;
    controllerCallbackRef.current?.(controller);
    const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
    void controller.start({
      client,
      threadId,
      host,
      cwd,
      element,
      reducedMotion,
      onStatus: setStatus,
      onTitle: (title) => titleRef.current?.(title),
    });
    return () => {
      controllerCallbackRef.current?.(null);
      controller.dispose();
      controllerRef.current = null;
    };
  }, [client, threadId, host, cwd, attempt]);

  useEffect(() => {
    if (!active) return;
    const frame = window.requestAnimationFrame(() => controllerRef.current?.fit());
    return () => window.cancelAnimationFrame(frame);
  }, [active]);

  useEffect(() => {
    const off = client.onStatus((snapshot) => {
      if (snapshot.status === "online" && statusRef.current.phase === "offline") {
        setAttempt((value) => value + 1);
      }
    });
    return off;
  }, [client]);

  const sendKey = useCallback((sequence: string) => {
    controllerRef.current?.send(sequence);
  }, []);

  const arrow = (key: string) => {
    const sequence = ctrlArmed ? CTRL_ARROWS[key] : ARROWS[key];
    if (!sequence) return;
    sendKey(sequence);
    if (ctrlArmed) setCtrlArmed(false);
  };

  const paste = async () => {
    try {
      const text = await navigator.clipboard?.readText();
      if (text) controllerRef.current?.send(text);
    } catch {
      // Clipboard access can be denied on a non-secure origin; the user can
      // still paste straight into xterm on desktop.
    }
  };

  const showOpening = status.phase === "opening";
  const showError = status.phase === "error";
  const showOffline = status.phase === "offline";

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="relative min-h-0 flex-1">
        <div
          ref={surfaceRef}
          className="terminal-surface absolute inset-0"
          role="application"
          aria-label="Terminal session"
        />

        {showOpening ? (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center gap-2 bg-stage/80 text-micro text-stage-ink-muted">
            <LoaderCircleIcon className="size-3.5 animate-spin" aria-hidden />
            Opening shell…
          </div>
        ) : null}

        {showError || showOffline ? (
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 bg-stage px-6 text-center">
            {showOffline ? (
              <WifiOffIcon className="size-5 text-stage-ink-muted" aria-hidden />
            ) : (
              <CircleAlertIcon className="size-5 text-strike" aria-hidden />
            )}
            <p className="text-compact font-medium text-stage-ink">
              {showOffline ? "The daemon is offline." : status.message}
            </p>
            <p className="max-w-[46ch] text-micro text-stage-ink-muted">
              {status.hint ??
                (showOffline
                  ? "Reconnect and the terminal will open on its own."
                  : "Close this tab and open a new terminal.")}
            </p>
            <button
              type="button"
              className="mt-1 inline-flex h-7 items-center gap-1.5 rounded-sm border border-white/20 px-2.5 text-micro font-medium text-stage-ink transition-colors hover:bg-white/10"
              onClick={() => {
                if (showOffline) client.reconnectNow();
                setAttempt((value) => value + 1);
              }}
            >
              <RefreshCwIcon className="size-3" aria-hidden />
              {showOffline ? "Reconnect" : "Try again"}
            </button>
          </div>
        ) : null}
      </div>

      <div
        className={cn(
          "flex h-12 shrink-0 items-center gap-0.5 overflow-x-auto border-t border-white/10 bg-stage px-2 sm:hidden",
          status.phase !== "attached" && "opacity-60",
        )}
        role="toolbar"
        aria-label="Terminal keys"
      >
        <KeyButton label="Esc" onPress={() => sendKey("\u001b")} />
        <KeyButton label="Tab" onPress={() => sendKey("\t")} />
        <KeyButton
          label="Ctrl"
          active={ctrlArmed}
          aria-pressed={ctrlArmed}
          onPress={() => setCtrlArmed((value) => !value)}
        />
        <span className="h-5 w-px bg-white/10" aria-hidden />
        <KeyButton label="←" ariaLabel="Left arrow" onPress={() => arrow("ArrowLeft")} />
        <KeyButton label="↑" ariaLabel="Up arrow" onPress={() => arrow("ArrowUp")} />
        <KeyButton label="↓" ariaLabel="Down arrow" onPress={() => arrow("ArrowDown")} />
        <KeyButton label="→" ariaLabel="Right arrow" onPress={() => arrow("ArrowRight")} />
        <KeyButton label="Paste" onPress={() => void paste()} />
      </div>

      {status.phase === "exited" ? (
        <div className="flex h-8 shrink-0 items-center justify-center border-t border-white/10 bg-stage text-micro text-stage-ink-muted">
          {status.message ?? "Process exited."}
        </div>
      ) : null}
    </div>
  );
}

function KeyButton({
  label,
  ariaLabel,
  active,
  onPress,
  ...rest
}: {
  label: string;
  ariaLabel?: string;
  active?: boolean;
  onPress: () => void;
} & Pick<ButtonHTMLAttributes<HTMLButtonElement>, "aria-pressed">) {
  return (
    <button
      type="button"
      aria-label={ariaLabel ?? label}
      className={cn(
        "inline-flex h-11 min-w-11 shrink-0 items-center justify-center rounded-sm border border-white/15 px-2 text-micro text-stage-ink transition-colors active:bg-white/20",
        active && "border-cue/60 bg-cue/20 text-cue",
      )}
      onClick={onPress}
      {...rest}
    >
      {label}
    </button>
  );
}
