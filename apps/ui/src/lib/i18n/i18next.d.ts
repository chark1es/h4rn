import "i18next";

import type enCommands from "./locales/en/commands.json";
import type enCommon from "./locales/en/common.json";
import type enSettings from "./locales/en/settings.json";
import type enSidebar from "./locales/en/sidebar.json";

// Message keys are checked against the English catalogue: a missing or
// misspelled key is a type error.
declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "common";
    resources: {
      common: typeof enCommon;
      commands: typeof enCommands;
      settings: typeof enSettings;
      sidebar: typeof enSidebar;
    };
    returnNull: false;
  }
}
