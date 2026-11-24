//! Moving a thread to another harness between turns.
//!
//! A switch is only offered when nothing is in flight: no running turn, no
//! queued or parked prompt, no armed rate-limit resume and no waiting approval.
//! That is what makes the rules below safe. The old session is shut down, the
//! old resume id is cleared, and the next turn starts a fresh session on the
//! new harness with a bounded handoff of the journaled history.
//!
//! This module owns the decisions a switch makes about settings and the plain
//! words for each refusal, so `threads.rs` only keeps the small hook that
//! applies them to a record.

use h4rn_protocol::{DroppedSetting, DroppedSettingKind, HarnessKind, RuntimeMode};

/// What the person asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRequest {
    /// Harness the next turns run on.
    pub harness: HarnessKind,
    /// Configured ACP agent instance when `harness` is [`HarnessKind::Acp`].
    pub agent_id: Option<String>,
    /// Model slug to start the new harness with.
    pub model: String,
}

/// The thread's settings before the switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentSettings {
    /// Permission autonomy.
    pub runtime_mode: RuntimeMode,
    /// Reasoning effort in the old harness's vocabulary.
    pub reasoning_effort: Option<String>,
    /// Whether the old harness was asked for its fast tier.
    pub fast: bool,
}

/// What the thread should look like after the switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchPlan {
    /// New harness.
    pub harness: HarnessKind,
    /// Configured ACP instance, or `None` for a built-in harness.
    pub agent_id: Option<String>,
    /// New model slug.
    pub model: String,
    /// Permission mode carried over, or the safe default when the new harness
    /// has no such mode.
    pub runtime_mode: RuntimeMode,
    /// Effort carried over, or the new harness's default.
    pub reasoning_effort: Option<String>,
    /// Fast tier carried over, when the new harness has one.
    pub fast: bool,
    /// Settings that had to be dropped, with the values they held.
    pub dropped: Vec<DroppedSetting>,
}

/// What a completed switch changed, for the request's answer.
#[derive(Clone, Debug)]
pub struct SwitchOutcome {
    /// The thread after the switch.
    pub summary: h4rn_protocol::ThreadSummary,
    /// Settings that had to be dropped.
    pub dropped: Vec<DroppedSetting>,
}

/// Why a switch request cannot be planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchPlanError {
    /// The request is malformed.
    Invalid(String),
    /// This build has no adapter for the target harness.
    Unsupported(String),
}

/// Ensures a switch is only applied when no work is in flight.
///
/// The order is the order the person would meet the reasons in: a running turn
/// first, then anything waiting behind it, then a pause, then a raised hand.
pub fn refusal(
    turn_running: bool,
    queued: usize,
    parked: bool,
    rate_limited: bool,
    approvals: usize,
    session_starting: bool,
) -> Option<(&'static str, &'static str)> {
    if turn_running {
        return Some((
            "this thread is still working",
            "Wait for the turn to finish, then switch harness.",
        ));
    }
    if queued > 0 {
        return Some((
            "a message is waiting behind the turn",
            "Let the queued message run, or stop the turn, then switch harness.",
        ));
    }
    if parked {
        return Some((
            "a prompt is parked",
            "Send or discard the parked prompt before switching harness.",
        ));
    }
    if rate_limited {
        return Some((
            "this thread is paused on a usage limit",
            "Resume it or cancel the resume, then switch harness.",
        ));
    }
    if approvals > 0 {
        return Some((
            "this thread is waiting for an approval",
            "Answer or dismiss it before switching harness.",
        ));
    }
    if session_starting {
        return Some((
            "this thread's harness session is starting",
            "Wait a moment, then switch harness.",
        ));
    }
    None
}

/// Plans a switch, carrying over every setting the target harness offers.
///
/// A setting the target does not offer is dropped to the target's safe default
/// and reported in [`SwitchPlan::dropped`]. The target's own model is required:
/// model names rarely mean anything across harnesses, so guessing one would
/// start a provider on a model it does not have.
pub fn plan(
    from: HarnessKind,
    request: &SwitchRequest,
    current: &CurrentSettings,
) -> Result<SwitchPlan, SwitchPlanError> {
    if from == request.harness {
        return Err(SwitchPlanError::Invalid(format!(
            "this thread already runs on {}",
            harness_label(from)
        )));
    }
    let model = request.model.trim().to_owned();
    if model.is_empty() {
        return Err(SwitchPlanError::Invalid(
            "choose a model for the new harness".into(),
        ));
    }
    let agent_id = match request.harness {
        HarnessKind::Acp => {
            let agent_id = request
                .agent_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            if agent_id.is_none() {
                return Err(SwitchPlanError::Invalid(
                    "choose a configured ACP agent to switch to".into(),
                ));
            }
            agent_id
        }
        _ => None,
    };

    let controls = h4rn_harness::thread_controls(request.harness);
    let mut dropped = Vec::new();

    // ACP agents advertise their real catalogue only at session start, so the
    // static controls accept every h4rn mode; the session refuses one it
    // cannot map instead of silently changing the person's choice.
    let runtime_mode =
        if request.harness == HarnessKind::Acp || controls.offers_mode(current.runtime_mode) {
            current.runtime_mode
        } else {
            dropped.push(DroppedSetting {
                setting: DroppedSettingKind::RuntimeMode,
                value: mode_slug(current.runtime_mode),
            });
            RuntimeMode::ApprovalRequired
        };

    let reasoning_effort = match current.reasoning_effort.as_deref() {
        Some(effort) if request.harness != HarnessKind::Acp && !controls.offers_effort(effort) => {
            dropped.push(DroppedSetting {
                setting: DroppedSettingKind::ReasoningEffort,
                value: effort.to_owned(),
            });
            None
        }
        other => other.map(str::to_owned),
    };

    let fast = if current.fast && !controls.fast {
        dropped.push(DroppedSetting {
            setting: DroppedSettingKind::Fast,
            value: "on".into(),
        });
        false
    } else {
        current.fast
    };

    Ok(SwitchPlan {
        harness: request.harness,
        agent_id,
        model,
        runtime_mode,
        reasoning_effort,
        fast,
        dropped,
    })
}

/// The protocol spelling of a mode, for a dropped-setting value.
fn mode_slug(mode: RuntimeMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Plain name of a harness, for refusal text.
pub fn harness_label(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
        HarnessKind::Opencode => "OpenCode",
        HarnessKind::Antigravity => "Antigravity",
        HarnessKind::Acp => "the ACP agent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(harness: HarnessKind, model: &str) -> SwitchRequest {
        SwitchRequest {
            harness,
            agent_id: None,
            model: model.into(),
        }
    }

    fn current(
        runtime_mode: RuntimeMode,
        reasoning_effort: Option<&str>,
        fast: bool,
    ) -> CurrentSettings {
        CurrentSettings {
            runtime_mode,
            reasoning_effort: reasoning_effort.map(str::to_owned),
            fast,
        }
    }

    #[test]
    fn plan_keeps_settings_the_new_harness_offers() {
        let plan = plan(
            HarnessKind::ClaudeCode,
            &request(HarnessKind::Codex, "gpt-5"),
            &current(RuntimeMode::AutoAcceptEdits, Some("high"), true),
        )
        .expect("plan");
        assert_eq!(plan.harness, HarnessKind::Codex);
        assert_eq!(plan.model, "gpt-5");
        assert_eq!(plan.runtime_mode, RuntimeMode::AutoAcceptEdits);
        assert_eq!(plan.reasoning_effort.as_deref(), Some("high"));
        assert!(plan.fast);
        assert!(plan.dropped.is_empty(), "{:?}", plan.dropped);
    }

    #[test]
    fn plan_drops_a_plan_mode_the_new_harness_cannot_run() {
        let plan = plan(
            HarnessKind::ClaudeCode,
            &request(HarnessKind::Codex, "gpt-5"),
            &current(RuntimeMode::Plan, None, false),
        )
        .expect("plan");
        assert_eq!(plan.runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(
            plan.dropped,
            vec![DroppedSetting {
                setting: DroppedSettingKind::RuntimeMode,
                value: "plan".into(),
            }]
        );
    }

    #[test]
    fn plan_drops_an_effort_and_fast_tier_the_new_harness_has_no_control_for() {
        let plan = plan(
            HarnessKind::Codex,
            &request(HarnessKind::Opencode, "claude-sonnet-5"),
            &current(RuntimeMode::FullAccess, Some("ultra"), true),
        )
        .expect("plan");
        assert_eq!(plan.reasoning_effort, None);
        assert!(!plan.fast);
        assert_eq!(
            plan.dropped,
            vec![
                DroppedSetting {
                    setting: DroppedSettingKind::ReasoningEffort,
                    value: "ultra".into(),
                },
                DroppedSetting {
                    setting: DroppedSettingKind::Fast,
                    value: "on".into(),
                },
            ]
        );
    }

    #[test]
    fn plan_refuses_the_same_harness_an_empty_model_and_a_missing_agent() {
        assert!(matches!(
            plan(
                HarnessKind::Codex,
                &request(HarnessKind::Codex, "gpt-5"),
                &current(RuntimeMode::ApprovalRequired, None, false),
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
        assert!(matches!(
            plan(
                HarnessKind::ClaudeCode,
                &request(HarnessKind::Codex, "  "),
                &current(RuntimeMode::ApprovalRequired, None, false),
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
        assert!(matches!(
            plan(
                HarnessKind::ClaudeCode,
                &request(HarnessKind::Acp, "demo"),
                &current(RuntimeMode::ApprovalRequired, None, false),
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
    }

    #[test]
    fn every_refusal_has_its_own_plain_explanation() {
        let reasons = [
            refusal(true, 0, false, false, 0, false),
            refusal(false, 2, false, false, 0, false),
            refusal(false, 0, true, false, 0, false),
            refusal(false, 0, false, true, 0, false),
            refusal(false, 0, false, false, 1, false),
            refusal(false, 0, false, false, 0, true),
        ];
        let messages = reasons
            .iter()
            .map(|reason| reason.expect("a refusal").0)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(messages.len(), reasons.len(), "each case reads differently");
        assert!(refusal(false, 0, false, false, 0, false).is_none());
    }
}
