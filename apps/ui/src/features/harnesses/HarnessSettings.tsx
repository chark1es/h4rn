import { RefreshCwIcon } from "lucide-react";

import type { HarnessKind, HarnessVersionInfo } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { useApp } from "@/lib/store/appStore";

import {
  loadHarnessVersions,
  updateHarness,
  useHarnesses,
  useHarnessVersions,
} from "./harnessStore";

/** One sentence naming where the row stands; the version numbers sit beside it. */
function statusText(row: HarnessVersionInfo): string {
  if (row.installed === null) return "Not installed";
  if (row.update_available) return "Update available";
  if (row.latest === null) return "Latest version unknown";
  return "Up to date";
}

function HarnessRow({ row }: { row: HarnessVersionInfo }) {
  const kind: HarnessKind = row.harness;
  const updating = useHarnesses((state) => state.updating[kind] === true);
  const output = useHarnesses((state) => state.output[kind]);
  const result = useHarnesses((state) => state.results[kind]);
  const runnable =
    row.method.kind === "self_update" || row.method.kind === "npm" || row.method.kind === "bun";
  const showOutput = (output?.length ?? 0) > 0 || result !== undefined;

  return (
    <div className="q-harness-row">
      <div className="q-field">
        <div className="lab" style={{ width: 200 }}>
          <div className="t">{HARNESS_LABELS[kind]}</div>
          <div className="s">{statusText(row)}</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">
            {row.installed ?? "—"}
            {row.update_available && row.latest ? ` → ${row.latest}` : ""}
          </span>
          <span style={{ flex: 1 }} />
          {row.update_available && runnable ? (
            <button
              type="button"
              className="q-btn secondary"
              disabled={updating}
              aria-label={`Update ${HARNESS_LABELS[kind]}`}
              onClick={() => void updateHarness(kind)}
            >
              {updating ? "Updating…" : "Update"}
            </button>
          ) : null}
        </div>
      </div>
      {row.update_available && row.method.kind === "manual" ? (
        <p className="q-harness-manual">
          h4rn won't change this install. Run <span className="q-mono">{row.method.command}</span>{" "}
          in a terminal.
        </p>
      ) : null}
      {showOutput ? (
        <div className="q-harness-out" role="status" aria-live="polite">
          {(output?.length ?? 0) > 0 ? <pre className="q-mono">{output?.join("\n")}</pre> : null}
          {result ? (
            <p className={result.ok ? "res" : "res bad"}>
              {result.ok ? "" : "Failed · "}
              {result.message}
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/** Settings → Harnesses: installed versus latest, with an Update action. */
export function HarnessSettings() {
  useHarnessVersions();
  const rows = useHarnesses((state) => state.rows);
  const loaded = useHarnesses((state) => state.loaded);
  const loading = useHarnesses((state) => state.loading);
  const error = useHarnesses((state) => state.error);
  const connection = useApp((state) => state.connection.status);

  return (
    <div className="q-form">
      <h1>Harnesses</h1>
      <p className="lead">
        The agent CLIs h4rn runs on this host. Latest versions come from the npm registry and are
        checked every few hours.
      </p>
      <div className="q-group">
        <div className="q-group-head">
          <h2>CLI versions</h2>
          <button
            type="button"
            className="q-btn ghost"
            disabled={loading || connection !== "online"}
            onClick={() => void loadHarnessVersions()}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Check again
          </button>
        </div>
        {!loaded ? (
          <div className="q-skel" style={{ height: 120 }} />
        ) : error ? (
          <p className="q-muted">{error.message}</p>
        ) : (
          rows.map((row) => <HarnessRow key={row.harness} row={row} />)
        )}
      </div>
    </div>
  );
}
