import { describe, expect, it } from "vitest";

import { agentAlertsAvailabilityText, agentAlertsSwitchEnabled } from "./agentAlerts";

describe("the machine's phone-alert switch", () => {
  it("can always be turned off, even without a Connect sign-in here", () => {
    expect(agentAlertsSwitchEnabled(true, false, false)).toBe(true);
  });

  it("can only be turned on when alerts have somewhere to go", () => {
    expect(agentAlertsSwitchEnabled(false, false, true)).toBe(true);
    expect(agentAlertsSwitchEnabled(false, false, false)).toBe(false);
  });

  it("ignores a click while loading or saving", () => {
    expect(agentAlertsSwitchEnabled(null, false, true)).toBe(false);
    expect(agentAlertsSwitchEnabled(true, true, true)).toBe(false);
  });

  it("promises delivery only when the daemon is paired with Connect", () => {
    expect(agentAlertsAvailabilityText(true)).toContain("Each alert carries");
    expect(agentAlertsAvailabilityText(false)).toBe(
      "Pair this machine with Connect to send alerts to a phone.",
    );
  });
});
