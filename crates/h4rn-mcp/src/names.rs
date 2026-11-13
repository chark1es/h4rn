//! Tool namespacing and collision handling.
//!
//! A model sees one flat tool namespace. The hub must therefore make every
//! tool name from every upstream server and every built-in provider unique, and
//! it must be able to reverse the name without asking a model to disambiguate.
//!
//! The scheme is `server__tool`, e.g. `github__create_issue` or
//! `h4rn__spawn_subagent`. Double underscore is the separator because MCP tool
//! names are conventionally `[A-Za-z0-9_-]`, so a single underscore is common
//! and a double one is not. Server names are validated to not contain `__`,
//! which makes the split unambiguous.
//!
//! Collisions are still possible across namespaces (two configured servers
//! cannot share a name, but a built-in provider can claim a name the user also
//! configured). [`allocate`] appends `_2`, `_3`, … deterministically so the
//! same configuration always produces the same names.

use std::collections::BTreeSet;

/// Separator between a server namespace and a tool name.
pub const SEPARATOR: &str = "__";

/// Why a server or tool name was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The name was empty or whitespace-only.
    #[error("the name is empty")]
    Empty,
    /// The name contains the namespace separator, which would make
    /// `server__tool` ambiguous.
    #[error("the name must not contain \"__\"")]
    ContainsSeparator,
    /// The name contains a character outside `[a-z0-9_-]` (after lowercasing)
    /// or starts with a separator.
    #[error("the name must match [a-z0-9][a-z0-9_-]*")]
    Invalid,
    /// The name is the reserved built-in namespace.
    #[error("\"{0}\" is reserved for h4rn's own tools")]
    Reserved(String),
}

/// Validates a server name.
///
/// Server names are case-insensitive: the hub stores the lowercased form, so
/// `GitHub` and `github` cannot become two servers whose tools look identical.
pub fn validate_server_name(name: &str) -> Result<(), NameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed.contains(SEPARATOR) {
        return Err(NameError::ContainsSeparator);
    }
    if trimmed.eq_ignore_ascii_case(super::HUB_SERVER_NAME) {
        return Err(NameError::Reserved(trimmed.to_owned()));
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return Err(NameError::Empty);
    };
    if !first.is_ascii_alphanumeric() {
        return Err(NameError::Invalid);
    }
    if !chars
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(NameError::Invalid);
    }
    Ok(())
}

/// Normalises a server name for storage and comparison.
pub fn normalise_server_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Builds the model-visible name for a tool.
pub fn qualified(server: &str, tool: &str) -> String {
    format!("{server}{SEPARATOR}{tool}")
}

/// Splits a qualified name back into `(server, tool)`.
///
/// The first separator wins, which is why server names may not contain it.
pub fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    let (server, tool) = qualified.split_once(SEPARATOR)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Allocates a unique tool name, appending `_2`, `_3`, … when `base` is taken.
///
/// The caller holds the set of names already allocated in this listing, so the
/// result is deterministic for a deterministic iteration order.
pub fn allocate(base: &str, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    for suffix in 2u32.. {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("the suffix space is larger than any tool list")
}

/// Validates a tool name as reported by an upstream server.
///
/// MCP does not constrain tool names beyond "a string", but a name with a
/// separator or control character would break the flat namespace. Rejected
/// tools are skipped rather than renamed, because renaming would make the
/// model's call and the server's expectation diverge.
pub fn tool_name_is_addressable(tool: &str) -> bool {
    !tool.is_empty()
        && !tool.contains(SEPARATOR)
        && tool
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(qualified("github", "create_issue"), "github__create_issue");
        assert_eq!(
            split_qualified("github__create_issue"),
            Some(("github", "create_issue"))
        );
        assert_eq!(split_qualified("plain"), None);
        assert_eq!(split_qualified("__tool"), None);
    }

    #[test]
    fn server_names_reject_the_separator_and_the_builtin_namespace() {
        assert_eq!(validate_server_name(""), Err(NameError::Empty));
        assert_eq!(
            validate_server_name("my__server"),
            Err(NameError::ContainsSeparator)
        );
        assert_eq!(
            validate_server_name("h4rn"),
            Err(NameError::Reserved("h4rn".into()))
        );
        assert_eq!(validate_server_name("-bad"), Err(NameError::Invalid));
        assert_eq!(validate_server_name("bad space"), Err(NameError::Invalid));
        assert!(validate_server_name("github-2").is_ok());
        assert!(validate_server_name("MyServer").is_ok());
    }

    #[test]
    fn collision_handling_is_deterministic() {
        let mut used = BTreeSet::new();
        assert_eq!(allocate("h4rn__echo", &mut used), "h4rn__echo");
        assert_eq!(allocate("h4rn__echo", &mut used), "h4rn__echo_2");
        assert_eq!(allocate("h4rn__echo", &mut used), "h4rn__echo_3");
        assert_eq!(allocate("other", &mut used), "other");
    }

    #[test]
    fn unaddressable_tool_names_are_skipped() {
        assert!(tool_name_is_addressable("read_file"));
        assert!(!tool_name_is_addressable("weird__name"));
        assert!(!tool_name_is_addressable("two words"));
        assert!(!tool_name_is_addressable(""));
    }
}
