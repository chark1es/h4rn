/**
 * Opening the phone panel sheet from places that do not own its state.
 *
 * On a phone the right panel is a sheet owned by `ThreadPane`; the thread
 * header opens it directly, and the composer's `/` menu asks for it through
 * this event so a command does not reach for the desktop panel tab.
 */
export type PhonePanelTab = "changes" | "subagents";

const PHONE_PANEL_EVENT = "h4rn:phone-panel";

export function openPhonePanel(tab: PhonePanelTab): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent<PhonePanelTab>(PHONE_PANEL_EVENT, { detail: tab }));
}

export function onPhonePanel(handler: (tab: PhonePanelTab) => void): () => void {
  if (typeof window === "undefined") return () => undefined;
  const listener = (event: Event) => {
    const tab = (event as CustomEvent<PhonePanelTab>).detail;
    if (tab === "changes" || tab === "subagents") handler(tab);
  };
  window.addEventListener(PHONE_PANEL_EVENT, listener);
  return () => window.removeEventListener(PHONE_PANEL_EVENT, listener);
}
