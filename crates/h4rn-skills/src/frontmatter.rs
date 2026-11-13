//! A deliberately small YAML frontmatter reader for `SKILL.md`.
//!
//! Claude Code, Codex, OpenCode and Antigravity all read a `SKILL.md` whose
//! frontmatter is a flat YAML mapping. h4rn only needs `name` and
//! `description`, so this module implements the subset those files use
//! (plain scalars, quoted scalars and `|`/`>` block scalars) instead of adding
//! a full YAML engine to the daemon. Unknown keys are preserved so a future
//! field can travel in [`Frontmatter::values`] without a parser change.

use std::collections::BTreeMap;

/// The metadata h4rn reads from a skill file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontmatter {
    /// `name`, when present.
    pub name: Option<String>,
    /// `description`, when present.
    pub description: Option<String>,
    /// Every top-level key, including the two above.
    pub values: BTreeMap<String, String>,
}

/// A skill file split into metadata and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSkill {
    /// Parsed frontmatter.
    pub frontmatter: Frontmatter,
    /// Markdown after the closing `---`.
    pub body: String,
}

/// Frontmatter parse failures.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrontmatterError {
    /// The opening fence was not closed.
    #[error("frontmatter starts with --- but has no closing ---")]
    Unterminated,
    /// A line was not `key: value`.
    #[error("line {line}: expected `key: value`")]
    Malformed {
        /// One-based line number inside the frontmatter.
        line: usize,
    },
}

/// Parses a `SKILL.md` document.
///
/// A file without frontmatter is valid: the registry falls back to the
/// directory name and an empty description.
pub fn parse(source: &str) -> Result<ParsedSkill, FrontmatterError> {
    let normalized = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = normalized.lines();
    let first = lines.next();
    if !matches!(first.map(str::trim), Some("---")) {
        return Ok(ParsedSkill {
            frontmatter: Frontmatter::default(),
            body: normalized.to_owned(),
        });
    }

    let mut front = Vec::new();
    let mut closed = false;
    let mut body_start = 0usize;
    for (index, line) in normalized.lines().enumerate() {
        if index == 0 {
            continue;
        }
        if line.trim() == "---" {
            closed = true;
            body_start = index + 1;
            break;
        }
        front.push(line);
    }
    if !closed {
        return Err(FrontmatterError::Unterminated);
    }

    let mut frontmatter = Frontmatter::default();
    let mut index = 0usize;
    while index < front.len() {
        let line = front[index];
        let trimmed = line.trim();
        index += 1;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A nested key belongs to a parent the registry does not read; skip it
        // rather than failing a real skill that uses `metadata:` or `allowed-tools`.
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            return Err(FrontmatterError::Malformed { line: index });
        };
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(FrontmatterError::Malformed { line: index });
        }
        let raw_value = raw_value.trim();
        let value =
            if raw_value == "|" || raw_value == ">" || raw_value == "|-" || raw_value == ">-" {
                let folded = raw_value.starts_with('>');
                let mut block = Vec::new();
                while index < front.len() {
                    let candidate = front[index];
                    if !candidate.is_empty()
                        && !candidate.starts_with(' ')
                        && !candidate.starts_with('\t')
                    {
                        break;
                    }
                    block.push(candidate.trim_start().to_owned());
                    index += 1;
                }
                if folded {
                    block.join(" ").trim().to_owned()
                } else {
                    block.join("\n").trim().to_owned()
                }
            } else {
                unquote(raw_value)
            };
        if key == "name" {
            frontmatter.name = Some(value.clone());
        } else if key == "description" {
            frontmatter.description = Some(value.clone());
        }
        frontmatter.values.insert(key, value);
    }

    let body = normalized
        .lines()
        .skip(body_start)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ParsedSkill { frontmatter, body })
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            let inner = &value[1..value.len() - 1];
            if first == b'"' {
                return inner
                    .replace("\\\"", "\"")
                    .replace("\\n", "\n")
                    .replace("\\t", "\t")
                    .replace("\\\\", "\\");
            }
            return inner.replace("''", "'");
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_and_description() {
        let source =
            "---\nname: leaving-pr-comments\ndescription: Leave review comments\n---\n\n# Body\n";
        let parsed = parse(source).unwrap();
        assert_eq!(
            parsed.frontmatter.name.as_deref(),
            Some("leaving-pr-comments")
        );
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("Leave review comments")
        );
        assert!(parsed.body.contains("# Body"));
    }

    #[test]
    fn accepts_quoted_and_block_descriptions() {
        let quoted = "---\nname: \"x\"\ndescription: 'a: b'\n---\n";
        let parsed = parse(quoted).unwrap();
        assert_eq!(parsed.frontmatter.name.as_deref(), Some("x"));
        assert_eq!(parsed.frontmatter.description.as_deref(), Some("a: b"));

        let block = "---\nname: x\ndescription: |\n  first\n  second\n---\n";
        let parsed = parse(block).unwrap();
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn uses_the_directory_name_when_there_is_no_frontmatter() {
        let parsed = parse("# just markdown\n").unwrap();
        assert!(parsed.frontmatter.name.is_none());
    }

    #[test]
    fn reports_an_unterminated_fence() {
        assert_eq!(
            parse("---\nname: x\n").unwrap_err(),
            FrontmatterError::Unterminated
        );
    }
}
