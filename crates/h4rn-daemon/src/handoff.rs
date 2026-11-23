//! Bounded handoffs for threads that continue somewhere else.
//!
//! An imported thread has no live harness session. Its first new message
//! starts a fresh one, and the saved history must reach that agent without
//! crowding out the person's request. [`build`] selects the original request
//! plus the newest messages, in order, under a byte budget; the request itself
//! is appended afterwards and is never shortened.
//!
//! The builder takes only roles and text, so provider switching and forking
//! can reuse it later with their own history.

use h4rn_protocol::MessageRole;

/// Default budget for one handoff, in UTF-8 bytes.
///
/// A byte is charged conservatively as one token, so 16,000 leaves most of a
/// model's window for instructions, tools and the work that follows.
pub const DEFAULT_HANDOFF_BYTES: usize = 16_000;

/// Smallest budget a setting may ask for.
pub const MIN_HANDOFF_BYTES: usize = 1_024;

/// Largest budget a setting may ask for.
pub const MAX_HANDOFF_BYTES: usize = 64_000;

/// The ceiling imported history has on its own, independent of the handoff
/// setting.
pub const IMPORTED_HISTORY_BYTES: usize = 64_000;

/// Most messages one handoff carries.
pub const MAX_HANDOFF_MESSAGES: usize = 200;

/// Environment variable that overrides [`DEFAULT_HANDOFF_BYTES`].
pub const ENV_HANDOFF_BYTES: &str = "H4RN_HANDOFF_BYTES";

/// One saved message offered to the builder.
#[derive(Clone, Copy, Debug)]
pub struct HandoffMessage<'a> {
    /// Who authored it.
    pub role: MessageRole,
    /// Its text.
    pub text: &'a str,
}

/// Reads the configured handoff budget, clamped to the documented range.
pub fn budget_from_env() -> usize {
    std::env::var(ENV_HANDOFF_BYTES)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(clamp_budget)
        .unwrap_or(DEFAULT_HANDOFF_BYTES)
}

/// Clamps a budget to [`MIN_HANDOFF_BYTES`]..=[`MAX_HANDOFF_BYTES`].
pub fn clamp_budget(bytes: usize) -> usize {
    bytes.clamp(MIN_HANDOFF_BYTES, MAX_HANDOFF_BYTES)
}

/// Selects the history block of a handoff, or `None` when there is no history.
///
/// The first user request is always offered first; the rest of the budget goes
/// to the newest messages, kept in their original order. The block ends with a
/// line that introduces the person's new request, which the caller appends
/// verbatim.
pub fn build(messages: &[HandoffMessage<'_>], budget: usize) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let budget = clamp_budget(budget).min(IMPORTED_HISTORY_BYTES);
    let mut indices: Vec<usize> = Vec::new();
    let mut used = HEADER.len();
    let first_user = messages
        .iter()
        .position(|message| message.role == MessageRole::User);
    if let Some(first) = first_user {
        let cost = cost(messages[first]);
        if cost <= budget {
            indices.push(first);
            used += cost;
        }
    }
    for index in (0..messages.len()).rev() {
        if indices.len() >= MAX_HANDOFF_MESSAGES {
            break;
        }
        if Some(index) == first_user {
            continue;
        }
        let cost = cost(messages[index]);
        if used + cost > budget {
            continue;
        }
        used += cost;
        indices.push(index);
    }
    if indices.is_empty() {
        return None;
    }
    indices.sort_unstable();
    indices.dedup();
    let mut block = String::from(HEADER);
    for index in indices {
        let message = messages[index];
        let role = match message.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
        };
        block.push_str(role);
        block.push_str(":\n");
        block.push_str(message.text.trim_end());
        block.push_str("\n\n");
    }
    block.push_str(REQUEST_LEAD);
    Some(block)
}

/// Appends the person's new request to a history block.
///
/// The request is never shortened or rewritten: it is the caller's exact text.
pub fn with_request(block: &str, request: &str) -> String {
    format!("{block}{request}")
}

const HEADER: &str = "Earlier conversation history, imported from another harness. Older messages may be omitted.\n\n";

const REQUEST_LEAD: &str = "My next message:\n";

fn cost(message: HandoffMessage<'_>) -> usize {
    // Role label, separator and text, approximating the serialised block.
    message.text.len() + 16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: MessageRole, text: &str) -> HandoffMessage<'_> {
        HandoffMessage { role, text }
    }

    #[test]
    fn a_short_conversation_is_included_intact() {
        let history = [
            message(MessageRole::User, "Add a health check"),
            message(MessageRole::Assistant, "Added /healthz"),
        ];
        let block = build(&history, DEFAULT_HANDOFF_BYTES).expect("block");
        assert!(block.contains("User:\nAdd a health check"));
        assert!(block.contains("Assistant:\nAdded /healthz"));
        assert!(block.ends_with(REQUEST_LEAD));
        assert_eq!(
            with_request(&block, "now add metrics"),
            format!("{block}now add metrics")
        );
    }

    #[test]
    fn the_first_request_survives_and_the_newest_messages_fill_the_budget() {
        let mut history = vec![message(MessageRole::User, "the original request")];
        let texts = (0..50)
            .map(|index| format!("message {index} {}", "x".repeat(200)))
            .collect::<Vec<_>>();
        for text in &texts {
            history.push(message(MessageRole::Assistant, text));
        }
        let block = build(&history, MIN_HANDOFF_BYTES).expect("block");
        assert!(block.contains("the original request"));
        assert!(block.len() <= MIN_HANDOFF_BYTES + REQUEST_LEAD.len());
        // The newest message wins a place over older ones.
        assert!(block.contains("message 49"));
        assert!(!block.contains("message 0 "));
    }

    #[test]
    fn the_budget_is_clamped_and_empty_history_builds_nothing() {
        assert_eq!(clamp_budget(1), MIN_HANDOFF_BYTES);
        assert_eq!(clamp_budget(usize::MAX), MAX_HANDOFF_BYTES);
        assert!(build(&[], DEFAULT_HANDOFF_BYTES).is_none());
    }

    #[test]
    fn roles_are_labelled_and_messages_keep_their_order() {
        let history = [
            message(MessageRole::User, "first"),
            message(MessageRole::Assistant, "second"),
            message(MessageRole::User, "third"),
        ];
        let block = build(&history, DEFAULT_HANDOFF_BYTES).expect("block");
        let first = block.find("first").expect("first");
        let second = block.find("second").expect("second");
        let third = block.find("third").expect("third");
        assert!(first < second && second < third);
    }
}
