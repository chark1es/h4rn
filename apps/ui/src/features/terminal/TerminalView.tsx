import { CircleAlertIcon, RefreshCwIcon, WifiOffIcon } from "lucide-react";
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
  onDimensions,
}: {
  client: H4rnClient;
  threadId: string | null;
  host: string;
  cwd: string;
  active: boolean;
  onTitle?: (title: string) => void;
  onController?: (controller: TerminalController | null) => void;
  onDimensions?: (dimensions: { cols: number; rows: number }) => void;
}) {
  const surfaceRef = useRef<HTMLDivElement>(null);
  const controllerRef = useRef<TerminalController | null>(null);
  const titleRef = useRef(onTitle);
  titleRef.current = onTitle;
  const controllerCallbackRef = useRef(onController);
  controllerCallbackRef.current = onController;
  const dimensionsRef = useRef(onDimensions);
  dimensionsRef.current = onDimensions;

  const [status, setStatus] = useState<TerminalStatus>({ phase: "opening" });
  const [dimensions, setDimensions] = useState({ cols: 0, rows: 0 });
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
      onDimensions: (next) => {
        setDimensions(next);
        dimensionsRef.current?.(next);
      },
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
          <div
            className="pointer-events-none absolute inset-0 flex items-center justify-center text-[12px] text-[var(--fg-3)]"
            style={{ background: "color-mix(in oklch, var(--bg-sunk) 85%, transparent)" }}
          >
            Opening shell…
          </div>
        ) : null}

        {showError || showOffline ? (
          <div
            className="absolute inset-0 flex flex-col items-center justify-center gap-2 px-6 text-center"
            style={{ background: "var(--bg-sunk)", color: "var(--fg)" }}
          >
            {showOffline ? (
              <WifiOffIcon className="size-5 text-[var(--fg-3)]" aria-hidden />
            ) : (
              <CircleAlertIcon className="size-5 text-[var(--danger)]" aria-hidden />
            )}
            <p className="text-[13px] font-medium text-[var(--fg)]">
              {showOffline ? "The daemon is offline." : status.message}
            </p>
            <p className="max-w-[46ch] text-[12px] text-[var(--fg-2)]">
              {status.hint ??
                (showOffline
                  ? "Reconnect and the terminal will open on its own."
                  : "Close this tab and open a new terminal.")}
            </p>
            <button
              type="button"
              className="mt-1 inline-flex h-7 items-center gap-1.5 rounded-md border border-[var(--line-2)] px-2.5 text-[12px] font-medium text-[var(--fg)] transition-colors hover:bg-[var(--hover)]"
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

      {status.phase === "attached" ? (
        <div className="q-term-note">
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>
            Sized to this phone. Desktop viewers see the same {dimensions.cols || "—"} columns.
          </span>
        </div>
      ) : null}

      <div
        className={cn("q-keybar sm:hidden", status.phase !== "attached" && "opacity-60")}
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
        <KeyButton label="↑" ariaLabel="Up arrow" onPress={() => arrow("ArrowUp")} />
        <KeyButton label="↓" ariaLabel="Down arrow" onPress={() => arrow("ArrowDown")} />
        <KeyButton label="←" ariaLabel="Left arrow" onPress={() => arrow("ArrowLeft")} />
        <KeyButton label="→" ariaLabel="Right arrow" onPress={() => arrow("ArrowRight")} />
        <KeyButton label="Paste" onPress={() => void paste()} />
      </div>

      {status.phase === "exited" ? (
        <div className="flex h-8 shrink-0 items-center justify-center border-t border-[var(--line)] bg-[var(--bg-sunk)] text-[12px] text-[var(--fg-3)]">
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
      className={cn("q-key", active && "on")}
      onClick={onPress}
      {...rest}
    >
      {label}
    </button>
  );
}
