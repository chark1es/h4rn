import { setDefaultPermissionMode, useThreadSettings } from "@/features/threads/settings";
import type { RuntimeMode } from "@/gen";
import { MODE_CHOICES } from "@/lib/threadControls";

/**
 * Settings → Threads → New threads: the permission mode a new thread starts
 * on, and the mode a thread must differ from to show its settings next to the
 * model.
 */
export function ThreadSettingsForm() {
  const settings = useThreadSettings();
  return (
    <div className="q-form">
      <h1>Threads</h1>
      <p className="lead">
        Defaults for threads started from this device. A thread keeps its own settings once it
        exists; changing them here never moves an existing thread.
      </p>
      <div className="q-group">
        <h2>New threads</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Permissions</div>
            <div className="s">
              The mode a new thread starts in. Change it mid-thread from the composer.
            </div>
          </div>
          <div className="ctl">
            <div className="q-seg" role="radiogroup" aria-label="Default permissions">
              {MODE_CHOICES.map((choice) => (
                // biome-ignore lint/a11y/useSemanticElements: a segmented radio group, not a form radio
                <button
                  key={choice.value}
                  type="button"
                  role="radio"
                  aria-checked={settings.defaultPermissionMode === choice.value}
                  onClick={() => setDefaultPermissionMode(choice.value as RuntimeMode)}
                >
                  {choice.label}
                </button>
              ))}
            </div>
          </div>
        </div>
        <p className="s q-muted">
          Plan is offered only by harnesses with a real plan mode; a harness that cannot honour the
          mode refuses it when the thread starts.
        </p>
      </div>
    </div>
  );
}
