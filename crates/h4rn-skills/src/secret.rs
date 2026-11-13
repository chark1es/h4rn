//! A conservative secret scanner that runs before a skill is uploaded.
//!
//! Sync is the moment a skill leaves the machine, so the scanner runs over the
//! exact bytes that would be pushed. It is intentionally biased toward false
//! positives: a refused sync with a clear hint is cheap, a leaked provider key
//! is not. Binary files are skipped unless they contain a PEM private-key
//! header.

use crate::model::SkillBundle;

/// What kind of secret was found. Deliberately coarse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    /// A PEM-encoded private key.
    PrivateKey,
    /// An AWS access key id.
    AwsAccessKey,
    /// A GitHub token.
    GitHubToken,
    /// A Slack token.
    SlackToken,
    /// A Stripe live key.
    StripeKey,
    /// An OpenAI or Anthropic API key.
    ProviderKey,
    /// A JSON Web Token.
    Jwt,
    /// A generic `api_key = ...` style assignment.
    CredentialAssignment,
}

impl SecretKind {
    /// Stable spelling used in hints and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrivateKey => "private key",
            Self::AwsAccessKey => "AWS access key",
            Self::GitHubToken => "GitHub token",
            Self::SlackToken => "Slack token",
            Self::StripeKey => "Stripe key",
            Self::ProviderKey => "provider API key",
            Self::Jwt => "JSON Web Token",
            Self::CredentialAssignment => "credential assignment",
        }
    }
}

/// One suspected secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretFinding {
    /// Bundle-relative file path.
    pub path: String,
    /// One-based line number.
    pub line: usize,
    /// What it looks like.
    pub kind: SecretKind,
}

/// The scanner's hard cap; a bundle with more is refused anyway.
const MAX_FINDINGS: usize = 20;

/// Scans every text-like file in a bundle.
pub fn scan_bundle(bundle: &SkillBundle) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    for (path, bytes) in &bundle.files {
        if findings.len() >= MAX_FINDINGS {
            break;
        }
        scan_file(path, bytes, &mut findings);
    }
    findings
}

/// Scans one file.
pub fn scan_file(path: &str, bytes: &[u8], findings: &mut Vec<SecretFinding>) {
    if bytes.contains(&0) && !looks_like_pem(bytes) {
        return;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        if looks_like_pem(bytes) {
            findings.push(SecretFinding {
                path: path.to_owned(),
                line: 1,
                kind: SecretKind::PrivateKey,
            });
        }
        return;
    };
    for (index, line) in text.lines().enumerate() {
        if findings.len() >= MAX_FINDINGS {
            return;
        }
        if let Some(kind) = classify(line) {
            findings.push(SecretFinding {
                path: path.to_owned(),
                line: index + 1,
                kind,
            });
        }
    }
}

fn looks_like_pem(bytes: &[u8]) -> bool {
    let haystack = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    haystack.contains("-----BEGIN") && haystack.contains("PRIVATE KEY-----")
}

fn classify(line: &str) -> Option<SecretKind> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("-----BEGIN") && trimmed.contains("PRIVATE KEY-----") {
        return Some(SecretKind::PrivateKey);
    }
    if let Some(token) = find_token(trimmed, "AKIA", 16)
        && token
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Some(SecretKind::AwsAccessKey);
    }
    for prefix in ["ghp_", "gho_", "ghs_", "ghr_", "github_pat_"] {
        if find_token(trimmed, prefix, 20).is_some() {
            return Some(SecretKind::GitHubToken);
        }
    }
    for prefix in ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"] {
        if trimmed.contains(prefix) {
            return Some(SecretKind::SlackToken);
        }
    }
    if trimmed.contains("sk_live_") || trimmed.contains("rk_live_") {
        return Some(SecretKind::StripeKey);
    }
    if trimmed.contains("sk-ant-")
        || find_token(trimmed, "sk-proj-", 20).is_some()
        || find_token(trimmed, "sk-", 32).is_some()
    {
        return Some(SecretKind::ProviderKey);
    }
    if let Some(index) = trimmed.find("eyJ") {
        let candidate = &trimmed[index..];
        let dots = candidate.matches('.').count();
        if dots >= 2 && candidate.len() >= 60 {
            return Some(SecretKind::Jwt);
        }
    }
    if looks_like_assignment(trimmed) {
        return Some(SecretKind::CredentialAssignment);
    }
    None
}

fn find_token<'a>(line: &'a str, prefix: &str, min_tail: usize) -> Option<&'a str> {
    let start = line.find(prefix)?;
    let tail = &line[start + prefix.len()..];
    let len = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .count();
    (len >= min_tail).then_some(&line[start..])
}

fn looks_like_assignment(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let key = [
        "api_key",
        "apikey",
        "secret_key",
        "client_secret",
        "access_token",
        "password",
    ];
    let Some(name) = key.iter().find(|name| lower.contains(**name)) else {
        return false;
    };
    let after_name = &lower[lower.find(name).unwrap_or(0) + name.len()..];
    let Some(index) = after_name.find(['=', ':']) else {
        return false;
    };
    let value = after_name[index + 1..]
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim();
    value.len() >= 16
        && !value.starts_with('<')
        && !value.contains("${")
        && !value.contains("{{")
        && !value.eq_ignore_ascii_case("redacted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SKILL_FILE, read_bundle};
    use std::collections::BTreeMap;

    fn bundle(extra: &[(&str, &str)]) -> SkillBundle {
        let mut files = BTreeMap::new();
        files.insert(
            SKILL_FILE.to_owned(),
            b"---\nname: demo\ndescription: Demo\n---\n".to_vec(),
        );
        for (path, content) in extra {
            files.insert((*path).to_owned(), content.as_bytes().to_vec());
        }
        SkillBundle {
            manifest: h4rn_protocol::SkillManifest {
                name: "demo".into(),
                description: "Demo".into(),
                total_bytes: 0,
                files: Vec::new(),
            },
            files,
        }
    }

    #[test]
    fn finds_common_credentials() {
        let findings = scan_bundle(&bundle(&[
            ("a.md", "token: ghp_abcdefghijklmnopqrstuvwxyz0123456789\n"),
            ("b.sh", "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n"),
            ("c.txt", "password = \"hunter2hunter2hunter2\"\n"),
        ]));
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].path, "a.md");
        assert_eq!(findings[0].line, 1);
    }

    #[test]
    fn ignores_placeholders_and_prose() {
        let findings = scan_bundle(&bundle(&[(
            "a.md",
            "Use ${API_KEY} from your environment.\nSet api_key = <your key>.\nThe password is rotated by the service.\n",
        )]));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn finds_private_keys_in_binary_ish_files() {
        let mut files = BTreeMap::new();
        files.insert(
            "key.pem".to_owned(),
            b"\x00\x01-----BEGIN PRIVATE KEY-----\n...".to_vec(),
        );
        files.insert(
            SKILL_FILE.to_owned(),
            b"---\nname: demo\ndescription: Demo\n---\n".to_vec(),
        );
        let bundle = SkillBundle {
            manifest: h4rn_protocol::SkillManifest {
                name: "demo".into(),
                description: "Demo".into(),
                total_bytes: 0,
                files: Vec::new(),
            },
            files,
        };
        let findings = scan_bundle(&bundle);
        assert_eq!(findings[0].kind, SecretKind::PrivateKey);
    }

    #[test]
    fn a_clean_skill_is_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n\nUse the API reference.\n",
        )
        .unwrap();
        let bundle = read_bundle(&root).unwrap();
        assert!(scan_bundle(&bundle).is_empty());
    }
}
