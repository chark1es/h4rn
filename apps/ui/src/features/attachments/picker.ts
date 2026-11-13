/**
 * File picking adapter.
 *
 * The desktop shell can expose a native dialog-backed picker on
 * `window.__H4RN_DESKTOP__` (Tauri webviews cannot read a path from the web
 * side without a filesystem permission). When the bridge is absent — the web
 * build, mock mode, or a mobile webview — a hidden `input[type=file]` is the
 * fallback, which also gives phones their native photo picker.
 */

export type DesktopPickedFile = {
  name: string;
  mime: string;
  bytes: ArrayBuffer;
};

type DesktopBridge = {
  pickAttachments?: () => Promise<DesktopPickedFile[]>;
};

type PickerWindow = Window & {
  __TAURI_INTERNALS__?: unknown;
  __H4RN_DESKTOP__?: DesktopBridge;
};

function pickerWindow(): PickerWindow | null {
  return typeof window === "undefined" ? null : (window as PickerWindow);
}

/** True inside a Tauri webview (desktop or mobile shell). */
export function isTauri(): boolean {
  return Boolean(pickerWindow()?.__TAURI_INTERNALS__);
}

/** True when the desktop shell opted into the native dialog bridge. */
export function hasDesktopPicker(): boolean {
  return typeof pickerWindow()?.__H4RN_DESKTOP__?.pickAttachments === "function";
}

/**
 * Opens the native dialog when the shell provides it.
 *
 * Returns null when there is no bridge, so the caller can click the hidden
 * file input instead. A dismissed dialog returns an empty array.
 */
export async function pickWithDesktop(): Promise<File[] | null> {
  const pick = pickerWindow()?.__H4RN_DESKTOP__?.pickAttachments;
  if (!pick) return null;
  const picked = await pick();
  return picked.map(
    (file) => new File([file.bytes], file.name, { type: file.mime || "application/octet-stream" }),
  );
}

/** `accept` for the fallback input: images plus the text formats models read. */
export const FILE_ACCEPT = [
  "image/*",
  "application/pdf",
  "text/plain",
  "text/markdown",
  "text/csv",
  "application/json",
  ".log",
  ".diff",
  ".patch",
].join(",");
