/**
 * The Scheduled tasks surface state.
 *
 * The daemon owns the timer, the journal and the webhook secret. This module
 * sends the `schedule_*` requests, keeps the last list and applies the
 * transient `ScheduleEvent` frames the runner emits after a run.
 */
import { useSyncExternalStore } from "react";
import type {
  ClientRequest,
  ProtoError,
  ResponseBody,
  ScheduleDraft,
  ScheduleEvent,
  ScheduleRun,
  ScheduleTask,
  ScheduleTaskId,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient } from "@/lib/store/appStore";

export type SchedulesState = {
  tasks: ScheduleTask[];
  loaded: boolean;
  loading: boolean;
  saving: boolean;
  error: ProtoError | null;
  saveError: string | null;
  /** Runs per task, loaded when a history is opened. */
  runs: Record<string, ScheduleRun[]>;
  runsLoading: string | null;
  runsError: string | null;
};

const initialState: SchedulesState = {
  tasks: [],
  loaded: false,
  loading: false,
  saving: false,
  error: null,
  saveError: null,
  runs: {},
  runsLoading: null,
  runsError: null,
};

let state: SchedulesState = initialState;
const listeners = new Set<() => void>();

function set(patch: Partial<SchedulesState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export function getSchedulesState(): SchedulesState {
  return state;
}

export function resetSchedulesStore(): void {
  state = initialState;
  for (const listener of listeners) listener();
}

export function useSchedules<T>(selector: (value: SchedulesState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

type ScheduleRequest = Extract<
  ClientRequest,
  {
    type:
      | "list_schedules"
      | "create_schedule"
      | "update_schedule"
      | "delete_schedule"
      | "set_schedule_enabled"
      | "run_schedule_now"
      | "list_schedule_runs"
      | "rotate_schedule_webhook";
  }
>;

/** Test seam: answer schedule requests without a socket. */
let sendImpl: ((request: ScheduleRequest) => Promise<ResponseBody>) | null = null;

export function setSchedulesRequesterForTests(
  next: ((request: ScheduleRequest) => Promise<ResponseBody>) | null,
): void {
  sendImpl = next;
}

async function send(request: ScheduleRequest): Promise<ResponseBody> {
  if (sendImpl) return sendImpl(request);
  return (await getClient().request(request as never)) as unknown as ResponseBody;
}

function describe(error: unknown): string {
  const proto = toProtoError(error);
  return proto.hint ? `${proto.message} ${proto.hint}` : proto.message;
}

/** Loads every task. A failed read keeps the rows already shown. */
export async function loadSchedules(): Promise<void> {
  set({ loading: true, error: null });
  try {
    const response = await send({ type: "list_schedules" });
    if (response.type !== "schedules") {
      set({ loading: false });
      return;
    }
    set({ tasks: response.tasks, loaded: true, loading: false, error: null });
  } catch (error) {
    set({ loading: false, loaded: true, error: toProtoError(error) });
  }
}

/** Creates a task and adds it to the list. */
export async function createSchedule(draft: ScheduleDraft): Promise<ScheduleTask | null> {
  set({ saving: true, saveError: null });
  try {
    const response = await send({ type: "create_schedule", task: draft });
    if (response.type !== "schedule_task") {
      set({ saving: false });
      return null;
    }
    upsert(response.task);
    set({ saving: false, loaded: true });
    return response.task;
  } catch (error) {
    set({ saving: false, saveError: describe(error) });
    return null;
  }
}

/** Saves an existing task's fields. */
export async function updateSchedule(
  taskId: ScheduleTaskId,
  draft: ScheduleDraft,
): Promise<ScheduleTask | null> {
  set({ saving: true, saveError: null });
  try {
    const response = await send({ type: "update_schedule", task_id: taskId, task: draft });
    if (response.type !== "schedule_task") {
      set({ saving: false });
      return null;
    }
    upsert(response.task);
    set({ saving: false });
    return response.task;
  } catch (error) {
    set({ saving: false, saveError: describe(error) });
    return null;
  }
}

/** Deletes a task and its history. */
export async function deleteSchedule(taskId: ScheduleTaskId): Promise<boolean> {
  set({ saveError: null });
  try {
    const response = await send({ type: "delete_schedule", task_id: taskId });
    if (response.type !== "ack") return false;
    set({ tasks: state.tasks.filter((task) => task.task_id !== taskId) });
    return true;
  } catch (error) {
    set({ saveError: describe(error) });
    return false;
  }
}

/** Enables or disables a task. */
export async function setScheduleEnabled(
  taskId: ScheduleTaskId,
  enabled: boolean,
): Promise<boolean> {
  set({ saveError: null });
  try {
    const response = await send({ type: "set_schedule_enabled", task_id: taskId, enabled });
    if (response.type !== "schedule_task") return false;
    upsert(response.task);
    return true;
  } catch (error) {
    set({ saveError: describe(error) });
    return false;
  }
}

/** Runs a clock task once. The response carries its fresh last run. */
export async function runScheduleNow(taskId: ScheduleTaskId): Promise<boolean> {
  set({ saveError: null });
  try {
    const response = await send({ type: "run_schedule_now", task_id: taskId });
    if (response.type !== "schedule_task") return false;
    upsert(response.task);
    return true;
  } catch (error) {
    set({ saveError: describe(error) });
    return false;
  }
}

/** Reads one task's bounded history, newest first. */
export async function loadScheduleRuns(taskId: ScheduleTaskId): Promise<void> {
  set({ runsLoading: taskId, runsError: null });
  try {
    const response = await send({ type: "list_schedule_runs", task_id: taskId });
    if (response.type !== "schedule_runs") {
      set({ runsLoading: null });
      return;
    }
    set({
      runs: { ...state.runs, [taskId]: response.runs },
      runsLoading: null,
    });
  } catch (error) {
    set({ runsLoading: null, runsError: describe(error) });
  }
}

/** Replaces a webhook task's path; every earlier URL stops working. */
export async function rotateScheduleWebhook(taskId: ScheduleTaskId): Promise<boolean> {
  set({ saveError: null });
  try {
    const response = await send({ type: "rotate_schedule_webhook", task_id: taskId });
    if (response.type !== "schedule_task") return false;
    upsert(response.task);
    return true;
  } catch (error) {
    set({ saveError: describe(error) });
    return false;
  }
}

/** Applies one live change from the runner. */
export function applyScheduleEvent(event: ScheduleEvent): void {
  if (event.kind === "removed") {
    set({ tasks: state.tasks.filter((task) => task.task_id !== event.task_id) });
    return;
  }
  upsert(event.task);
}

function upsert(task: ScheduleTask): void {
  const existing = state.tasks.findIndex((entry) => entry.task_id === task.task_id);
  const tasks =
    existing === -1
      ? [...state.tasks, task]
      : state.tasks.map((entry, index) => (index === existing ? task : entry));
  set({ tasks, loaded: true });
}

/** Keeps list order stable and oldest first, as the daemon returns it. */
export function sortedTasks(tasks: ScheduleTask[]): ScheduleTask[] {
  return [...tasks].sort((left, right) => left.created_at.localeCompare(right.created_at));
}
