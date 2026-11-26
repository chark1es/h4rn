import "i18next";

import type enCommands from "./locales/en/commands.json";
import type enCommon from "./locales/en/common.json";
import type enComposer from "./locales/en/composer.json";
import type enIssues from "./locales/en/issues.json";
import type enPairing from "./locales/en/pairing.json";
import type enPullRequests from "./locales/en/pullRequests.json";
import type enSettings from "./locales/en/settings.json";
import type enSidebar from "./locales/en/sidebar.json";
import type enThread from "./locales/en/thread.json";
import type enUsage from "./locales/en/usage.json";

// Message keys are checked against the English catalogue: a missing or
// misspelled key is a type error.
declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "common";
    resources: {
      common: typeof enCommon;
      commands: typeof enCommands;
      composer: typeof enComposer;
      issues: typeof enIssues;
      pairing: typeof enPairing;
      pullRequests: typeof enPullRequests;
      settings: typeof enSettings;
      sidebar: typeof enSidebar;
      thread: typeof enThread;
      usage: typeof enUsage;
    };
    returnNull: false;
  }
}
