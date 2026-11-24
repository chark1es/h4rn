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
    /// Configured ACP instance the thread runs on, when it is an ACP thread.
    ///
    /// A switch from one configured agent to another keeps the same harness
    /// kind, so the plan must tell "the same thread" from "another agent".
    pub agent_id: Option<String>,
}

/// What the target harness really offers, when that is only known at runtime.
///
/// A built-in harness's controls are declared in
/// [`h4rn_harness::thread_controls`]. A configured ACP agent advertises its own
/// catalogue at session start, so a switch resolves it first and passes it here;
/// a value the agent did not advertise is dropped and reported instead of being
/// handed to a session that will refuse to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetCapabilities {
    /// Permission modes the target offers.
    pub modes: Vec<RuntimeMode>,
    /// Reasoning-effort values the target offers.
    pub efforts: Vec<String>,
    /// Whether the target has a fast tier.
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
///
/// `target` is the target harness's real catalogue when the daemon resolved one
/// (a configured ACP agent advertises its modes and efforts at session start).
/// Without it the declared controls in `h4rn_harness::thread_controls` stand in.
pub fn plan(
    from: HarnessKind,
    request: &SwitchRequest,
    current: &CurrentSettings,
    target: Option<&TargetCapabilities>,
) -> Result<SwitchPlan, SwitchPlanError> {
    if from == request.harness {
        let same_agent =
            from != HarnessKind::Acp || current.agent_id.as_deref() == request.agent_id.as_deref();
        if same_agent {
            return Err(SwitchPlanError::Invalid(format!(
                "this thread already runs on {}",
                harness_label(from)
            )));
        }
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

    let declared = h4rn_harness::thread_controls(request.harness);
    // The target's own catalogue wins when the switch resolved one. A harness
    // with no runtime catalogue (or a switch that could not resolve it) keeps
    // the declared controls.
    let modes = target
        .map(|caps| caps.modes.clone())
        .unwrap_or(declared.modes);
    let efforts = target
        .map(|caps| caps.efforts.clone())
        .unwrap_or(declared.efforts);
    let fast_supported = target.map(|caps| caps.fast).unwrap_or(declared.fast);
    let mut dropped = Vec::new();

    let runtime_mode = if modes.contains(&current.runtime_mode) {
        current.runtime_mode
    } else if modes.is_empty() || modes.contains(&RuntimeMode::ApprovalRequired) {
        dropped.push(DroppedSetting {
            setting: DroppedSettingKind::RuntimeMode,
            value: mode_slug(current.runtime_mode),
        });
        // Approval required asks for no mode change at all, so an agent with
        // no catalogue (or one that offers `default`) can run there. An agent
        // that named its modes but not that one would refuse the session at
        // startup, so the switch must not claim success and fail later.
        RuntimeMode::ApprovalRequired
    } else {
        return Err(SwitchPlanError::Unsupported(format!(
            "{} offers {}, none of which can take this thread's `{}` mode; \
             switch it to a mode the agent lists, or to another harness",
            harness_label(request.harness),
            modes
                .iter()
                .map(|mode| format!("`{}`", mode_slug(*mode)))
                .collect::<Vec<_>>()
                .join(", "),
            mode_slug(current.runtime_mode),
        )));
    };

    let reasoning_effort = match current.reasoning_effort.as_deref() {
        Some(effort) if !efforts.iter().any(|offered| offered == effort) => {
            dropped.push(DroppedSetting {
                setting: DroppedSettingKind::ReasoningEffort,
                value: effort.to_owned(),
            });
            None
        }
        other => other.map(str::to_owned),
    };

    let fast = if current.fast && !fast_supported {
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

    fn acp_request(agent_id: &str, model: &str) -> SwitchRequest {
        SwitchRequest {
            harness: HarnessKind::Acp,
            agent_id: Some(agent_id.into()),
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
            agent_id: None,
        }
    }

    fn acp_current(agent_id: &str) -> CurrentSettings {
        CurrentSettings {
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            agent_id: Some(agent_id.into()),
        }
    }

    #[test]
    fn plan_keeps_settings_the_new_harness_offers() {
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &request(HarnessKind::Codex, "gpt-5"),
            &current(RuntimeMode::AutoAcceptEdits, Some("high"), true),
            None,
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
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &request(HarnessKind::Codex, "gpt-5"),
            &current(RuntimeMode::Plan, None, false),
            None,
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
        let plan = super::plan(
            HarnessKind::Codex,
            &request(HarnessKind::Opencode, "claude-sonnet-5"),
            &current(RuntimeMode::FullAccess, Some("ultra"), true),
            None,
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

    /// An ACP agent that offers only `accept-edits` has no safe replacement
    /// for Plan: startup would refuse `default`, so the switch stops before
    /// committing and names what the agent does offer.
    #[test]
    fn plan_refuses_when_the_target_acp_agent_has_no_safe_mode() {
        let capabilities = TargetCapabilities {
            modes: vec![RuntimeMode::AutoAcceptEdits],
            efforts: Vec::new(),
            fast: false,
        };
        let error = super::plan(
            HarnessKind::ClaudeCode,
            &acp_request("demo", "deepseek-chat"),
            &current(RuntimeMode::Plan, None, false),
            Some(&capabilities),
        )
        .expect_err("there is no mode the agent can start in");
        assert!(
            matches!(&error, SwitchPlanError::Unsupported(message) if message.contains("`plan`") && message.contains("`auto_accept_edits`")),
            "{error:?}"
        );
    }

    /// An ACP agent names its own modes, so a mode it never advertised is
    /// dropped and reported instead of failing the first Send.
    #[test]
    fn plan_drops_a_mode_the_target_acp_agent_does_not_advertise() {
        let capabilities = TargetCapabilities {
            modes: vec![RuntimeMode::ApprovalRequired, RuntimeMode::FullAccess],
            efforts: Vec::new(),
            fast: false,
        };
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &acp_request("demo", "deepseek-chat"),
            &current(RuntimeMode::Plan, None, false),
            Some(&capabilities),
        )
        .expect("plan");
        assert_eq!(plan.harness, HarnessKind::Acp);
        assert_eq!(plan.agent_id.as_deref(), Some("demo"));
        assert_eq!(plan.runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(
            plan.dropped,
            vec![DroppedSetting {
                setting: DroppedSettingKind::RuntimeMode,
                value: "plan".into(),
            }]
        );

        // A mode the agent did name is carried over unchanged.
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &acp_request("demo", "deepseek-chat"),
            &current(RuntimeMode::FullAccess, None, false),
            Some(&capabilities),
        )
        .expect("plan");
        assert_eq!(plan.runtime_mode, RuntimeMode::FullAccess);
        assert!(plan.dropped.is_empty(), "{:?}", plan.dropped);
    }

    /// An effort the target agent never advertised would fail at session
    /// start, so it returns to the agent's default and is reported.
    #[test]
    fn plan_drops_an_effort_the_target_acp_agent_does_not_advertise() {
        let capabilities = TargetCapabilities {
            modes: vec![RuntimeMode::ApprovalRequired],
            efforts: vec!["low".into(), "high".into()],
            fast: false,
        };
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &acp_request("demo", "deepseek-chat"),
            &current(RuntimeMode::ApprovalRequired, Some("max"), false),
            Some(&capabilities),
        )
        .expect("plan");
        assert_eq!(plan.reasoning_effort, None);
        assert_eq!(
            plan.dropped,
            vec![DroppedSetting {
                setting: DroppedSettingKind::ReasoningEffort,
                value: "max".into(),
            }]
        );

        // The agent's own effort is kept.
        let plan = super::plan(
            HarnessKind::ClaudeCode,
            &acp_request("demo", "deepseek-chat"),
            &current(RuntimeMode::ApprovalRequired, Some("high"), false),
            Some(&capabilities),
        )
        .expect("plan");
        assert_eq!(plan.reasoning_effort.as_deref(), Some("high"));
        assert!(plan.dropped.is_empty(), "{:?}", plan.dropped);
    }

    #[test]
    fn plan_moves_between_two_configured_acp_agents_and_refuses_the_same_one() {
        let plan = super::plan(
            HarnessKind::Acp,
            &acp_request("other", "deepseek-chat"),
            &acp_current("demo"),
            None,
        )
        .expect("another agent on the same harness is a real switch");
        assert_eq!(plan.harness, HarnessKind::Acp);
        assert_eq!(plan.agent_id.as_deref(), Some("other"));

        assert!(matches!(
            super::plan(
                HarnessKind::Acp,
                &acp_request("demo", "deepseek-chat"),
                &acp_current("demo"),
                None,
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
    }

    #[test]
    fn plan_refuses_the_same_harness_an_empty_model_and_a_missing_agent() {
        assert!(matches!(
            super::plan(
                HarnessKind::Codex,
                &request(HarnessKind::Codex, "gpt-5"),
                &current(RuntimeMode::ApprovalRequired, None, false),
                None,
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
        assert!(matches!(
            super::plan(
                HarnessKind::ClaudeCode,
                &request(HarnessKind::Codex, "  "),
                &current(RuntimeMode::ApprovalRequired, None, false),
                None,
            ),
            Err(SwitchPlanError::Invalid(_))
        ));
        assert!(matches!(
            super::plan(
                HarnessKind::ClaudeCode,
                &request(HarnessKind::Acp, "demo"),
                &current(RuntimeMode::ApprovalRequired, None, false),
                None,
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
