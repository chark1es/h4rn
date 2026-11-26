import { TriangleAlertIcon } from "lucide-react";
import type { CSSProperties } from "react";
import { useTranslation } from "react-i18next";

import { Button } from "@/components/ui/button";
import type { McpAction } from "@/features/mcp/store";
import type { SkillsAction } from "@/features/skills/store";
import type { ProtoError } from "@/gen";
import { errorWords } from "@/lib/client/errors";

/**
 * Why the MCP or Skills list could not load, with a Retry.
 *
 * The store keeps the error as it came and the request that failed as a kind,
 * so both are worded here: a client-written error and the request's name
 * follow a language switch, and a daemon's own text is shown as sent.
 */
export function LibraryErrorBanner({
  error,
  action,
  onRetry,
  style,
}: {
  error: ProtoError | null;
  action: McpAction | SkillsAction | null;
  onRetry: () => void;
  style?: CSSProperties;
}) {
  const { t } = useTranslation(["library", "common"]);
  if (!error) return null;
  const words = errorWords(error);
  return (
    <div className="q-banner err" style={style}>
      <span className="ic">
        <TriangleAlertIcon className="q-icon s14" aria-hidden />
      </span>
      <span className="grow">
        <b>
          {action
            ? t("errors.withAction", {
                message: words.message,
                action: t(`errors.actions.${action}`),
              })
            : words.message}
        </b>{" "}
        {words.hint ?? t("errors.noAnswer")}
      </span>
      <Button variant="ghost" size="sm" onClick={onRetry}>
        {t("common:retry")}
      </Button>
    </div>
  );
}
