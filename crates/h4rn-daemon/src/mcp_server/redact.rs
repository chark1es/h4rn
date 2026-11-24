//! One redactor for everything an outside agent may read.
//!
//! Two shapes leave the daemon as text: tool output (thread titles, messages)
//! and the listing forms of the same records. Both go through [`Redactor`], so
//! a credential is removed once, by one rule, and both the text and the
//! structured form carry the same redacted value.
//!
//! Redaction happens before truncation. A title of the first prompt is already
//! cut to 80 characters when the thread is created, so a token that straddles
//! that boundary left a fragment of its own prefix (`ghp`, `github_pa`) that no
//! whole-prefix rule matches. [`Redactor::title`] removes that tail as well, and
//! [`Redactor::text_to`] redacts the whole value before it cuts anything.

use std::sync::RwLock;

use crate::auth::TokenStore;

/// Prefixes of every credential h4rn itself mints.
const H4RN_TOKEN_PREFIXES: [&str; 5] = [
    "h4rn_at_",
    "h4rn_rt_",
    "h4rn_code_",
    "h4rn_auth_",
    "h4rn_mcp_",
];

/// Removes credential-shaped values from text an outside agent may read.
///
/// Two layers: the daemon's own secret value (its access token), and the shapes
/// h4rn and forge clients use, so a token pasted into a thread never comes back
/// out of a tool.
#[derive(Default)]
pub struct Redactor {
    owner_token: RwLock<Option<TokenStore>>,
}

impl Redactor {
    /// A redactor with no owner token yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names the store that holds the owner's access token.
    ///
    /// The token is read from the file on every use, so `h4rnd token --rotate`
    /// never leaves a new value unredacted.
    pub fn set_owner_token(&self, tokens: &TokenStore) {
        *self
            .owner_token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tokens.clone());
    }

    /// Removes every credential-shaped value from `value`.
    pub fn redact(&self, value: &str) -> String {
        let mut output = value.to_owned();
        let owner = self
            .owner_token
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|store| store.load().ok().flatten());
        if let Some(token) = owner.filter(|token| !token.is_empty())
            && output.contains(token.as_str())
        {
            output = output.replace(token.as_str(), "[redacted]");
        }
        let output = redact_h4rn_tokens(&output);
        h4rn_forge::redact(&output)
    }

    /// A title, redacted and with a fragment the 80-character cut left behind.
    pub fn title(&self, title: &str) -> String {
        trim_partial_token_tail(&self.redact(title))
    }

    /// `value` redacted before it is cut to `max_chars` characters.
    pub fn text_to(&self, value: &str, max_chars: usize) -> String {
        let redacted = self.redact(value);
        redacted.chars().take(max_chars).collect()
    }
}

/// Replaces every h4rn-shaped credential in `value`.
fn redact_h4rn_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let next = H4RN_TOKEN_PREFIXES
            .iter()
            .filter_map(|prefix| rest.find(prefix).map(|index| (index, *prefix)))
            .min_by_key(|(index, _)| *index);
        let Some((index, prefix)) = next else {
            output.push_str(rest);
            return output;
        };
        output.push_str(&rest[..index]);
        output.push_str("[redacted]");
        let after = &rest[index + prefix.len()..];
        rest = &after[token_run(after)..];
    }
}

/// Length of the credential characters at the start of `value`.
fn token_run(value: &str) -> usize {
    value
        .char_indices()
        .find(|(_, ch)| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '~' | '+' | '/' | '='))
        })
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

/// Removes a fragment of a token prefix that a truncation left at the end.
///
/// A title cut at 80 characters can end in `ghp` or `github_pa`: the whole
/// prefix is gone, so [`h4rn_forge::redact`] has nothing to match, but the
/// fragment is still part of a credential. The longest proper prefix of any
/// known token prefix (at least two characters) at the very end is replaced.
fn trim_partial_token_tail(value: &str) -> String {
    let mut best = 0;
    for prefix in H4RN_TOKEN_PREFIXES
        .iter()
        .chain(h4rn_forge::token_prefixes().iter())
    {
        for cut in (2..prefix.len()).rev() {
            if cut > best && value.ends_with(&prefix[..cut]) {
                best = cut;
                break;
            }
        }
    }
    if best == 0 {
        return value.to_owned();
    }
    let keep = value.len() - best;
    format!("{}[redacted]", &value[..keep])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_at_the_cut_is_removed_with_its_fragment() {
        let redactor = Redactor::new();
        let token = format!("{}{}", "ghp", "_").to_owned() + &"x".repeat(36);
        // The title is the first 80 characters of the prompt, exactly like a
        // thread's stored title.
        let prompt = format!("{} within {}", "a".repeat(69), token);
        let title: String = prompt.chars().take(80).collect();
        assert!(title.ends_with("ghp"), "{title}");
        let redacted = redactor.title(&title);
        assert!(!redacted.contains("ghp"), "{redacted}");
        assert!(redacted.ends_with("[redacted]"), "{redacted}");
    }

    #[test]
    fn a_whole_token_is_removed() {
        let redactor = Redactor::new();
        let token = format!("{}{}", "ghp", "_").to_owned() + &"x".repeat(36);
        let redacted = redactor.redact(&format!("for {token} here"));
        assert_eq!(redacted, "for [redacted] here");
    }

    #[test]
    fn a_short_text_is_untouched_by_the_cut() {
        let redactor = Redactor::new();
        assert_eq!(redactor.text_to("hello", 80), "hello");
    }
}
