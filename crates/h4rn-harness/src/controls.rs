//! What a harness can change on a thread, and when.
//!
//! A thread's permission mode, model, reasoning effort and fast tier are not
//! uniform across adapters: Claude Code has a plan mode and an effort flag, Codex
//! has per-turn reasoning effort and a fast service tier, OpenCode has neither,
//! and a configured ACP agent advertises its own list at session start. This
//! module is the one place that says which harness offers which control, so the
//! daemon and the UI never offer a switch a harness cannot honour.

use h4rn_protocol::{HarnessKind, RuntimeMode};

/// The effort values Claude Code's `--effort` flag accepts.
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// The reasoning efforts Codex models advertise today.
///
/// Codex publishes the exact list per model; these are the values every current
/// coding model accepts, so the picker never shows an empty list for a model it
/// has not catalogued.
pub const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

/// What one harness offers for a thread's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadControls {
    /// Permission modes this harness can run in, in display order.
    pub modes: Vec<RuntimeMode>,
    /// Reasoning-effort values in the harness's own vocabulary; empty when the
    /// harness has no real effort control.
    pub efforts: Vec<String>,
    /// Whether the harness has a real fast tier.
    pub fast: bool,
    /// Whether a live session can switch the permission mode in place.
    pub switch_mode: bool,
    /// Whether a live session can switch the model in place.
    pub switch_model: bool,
    /// Whether a live session can switch reasoning effort in place.
    pub switch_effort: bool,
    /// Whether a live session can switch its fast tier in place.
    pub switch_fast: bool,
}

impl ThreadControls {
    /// Whether `mode` is one this harness can run in.
    pub fn offers_mode(&self, mode: RuntimeMode) -> bool {
        self.modes.contains(&mode)
    }

    /// Whether `effort` is one this harness accepts.
    pub fn offers_effort(&self, effort: &str) -> bool {
        self.efforts.iter().any(|value| value == effort)
    }
}

fn efforts(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// The thread controls a built-in harness offers.
///
/// A configured ACP agent is the exception: its modes and efforts are
/// advertised by the agent at `session/new`, so this returns the h4rn modes it
/// may map and leaves the final check to the session (see
/// [`crate::SessionControls`]).
pub fn thread_controls(kind: HarnessKind) -> ThreadControls {
    match kind {
        HarnessKind::ClaudeCode => ThreadControls {
            modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Plan,
                RuntimeMode::FullAccess,
            ],
            efforts: efforts(CLAUDE_EFFORTS),
            fast: false,
            switch_mode: true,
            switch_model: true,
            switch_effort: false,
            switch_fast: false,
        },
        HarnessKind::Codex => ThreadControls {
            modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::FullAccess,
            ],
            efforts: efforts(CODEX_EFFORTS),
            fast: true,
            switch_mode: true,
            switch_model: true,
            switch_effort: true,
            switch_fast: true,
        },
        HarnessKind::Opencode => ThreadControls {
            modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::FullAccess,
            ],
            efforts: Vec::new(),
            fast: false,
            switch_mode: true,
            switch_model: true,
            switch_effort: false,
            switch_fast: false,
        },
        // Antigravity's stream-json fallback has a real plan mode; its ACP
        // transport only offers plan when the agent advertises it, which the
        // session checks before ignoring the request.
        HarnessKind::Antigravity => ThreadControls {
            modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Plan,
                RuntimeMode::FullAccess,
            ],
            efforts: Vec::new(),
            fast: false,
            switch_mode: true,
            switch_model: true,
            switch_effort: false,
            switch_fast: false,
        },
        // A configured ACP agent names its own modes. Plan is included so a
        // client can ask for it; the session refuses it when the agent does not
        // advertise a plan mode.
        HarnessKind::Acp => ThreadControls {
            modes: vec![
                RuntimeMode::ApprovalRequired,
                RuntimeMode::AutoAcceptEdits,
                RuntimeMode::Plan,
                RuntimeMode::FullAccess,
            ],
            efforts: Vec::new(),
            fast: false,
            switch_mode: true,
            switch_model: true,
            switch_effort: true,
            switch_fast: false,
        },
    }
}

/// What one live session can switch, as the agent itself advertised it.
///
/// An adapter returns this when its catalogue only exists at runtime (a
/// configured ACP agent, for example). Values here override
/// [`thread_controls`] for that session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionControls {
    /// Permission modes the session advertised, mapped to h4rn's vocabulary.
    pub modes: Vec<RuntimeMode>,
    /// Model ids the session advertised, when it named a catalogue.
    pub models: Option<Vec<String>>,
    /// Model ids that mean "the agent's own default" to this adapter.
    ///
    /// A request for one of these is valid even when the advertised catalogue
    /// does not list it: the alias names the agent's own choice, not a model.
    /// Empty for an adapter without such an alias.
    pub default_models: Vec<String>,
    /// Reasoning-effort values the session advertised; empty when it has no
    /// real effort control.
    pub efforts: Vec<String>,
    /// Whether the session advertised a fast tier.
    pub fast: bool,
    /// Whether this session can switch its permission mode in place.
    pub switch_mode: bool,
    /// Whether this session can switch its model in place.
    pub switch_model: bool,
    /// Whether this session can switch its reasoning effort in place.
    pub switch_effort: bool,
    /// Whether this session can switch its fast tier in place.
    pub switch_fast: bool,
}

impl Default for SessionControls {
    /// A session that advertised nothing and can switch nothing: the safe
    /// default for an adapter that must restart to apply a setting.
    fn default() -> Self {
        Self {
            modes: Vec::new(),
            models: None,
            default_models: Vec::new(),
            efforts: Vec::new(),
            fast: false,
            switch_mode: false,
            switch_model: false,
            switch_effort: false,
            switch_fast: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_offered_only_where_the_harness_has_it() {
        assert!(thread_controls(HarnessKind::ClaudeCode).offers_mode(RuntimeMode::Plan));
        assert!(!thread_controls(HarnessKind::Codex).offers_mode(RuntimeMode::Plan));
        assert!(!thread_controls(HarnessKind::Opencode).offers_mode(RuntimeMode::Plan));
    }

    #[test]
    fn effort_and_fast_are_offered_only_where_real() {
        let claude = thread_controls(HarnessKind::ClaudeCode);
        assert!(claude.offers_effort("xhigh"));
        assert!(!claude.fast);
        assert!(
            !claude.switch_effort,
            "Claude Code restarts for an effort change"
        );
        let codex = thread_controls(HarnessKind::Codex);
        assert!(codex.offers_effort("ultra"));
        assert!(codex.fast);
        assert!(codex.switch_effort && codex.switch_fast);
        assert!(thread_controls(HarnessKind::Opencode).efforts.is_empty());
        assert!(!thread_controls(HarnessKind::Antigravity).fast);
    }
}
