//! Skill failures and their protocol mapping.

use h4rn_protocol::{ProtoError, ProtoErrorCode};

use crate::secret::SecretFinding;

/// Everything that can go wrong while reading, storing or installing a skill.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// Filesystem failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Registry database failure.
    #[error("skills database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Manifest or metadata JSON failure.
    #[error("skills serialisation error: {0}")]
    Serialisation(#[from] serde_json::Error),
    /// A `SKILL.md` or bundle path is not valid.
    #[error("{path}: {reason}")]
    InvalidSkill {
        /// Path involved.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// A source could not be read or pinned.
    #[error("{0}")]
    InvalidSource(String),
    /// The skill or version is not in the registry.
    #[error("skill {0:?} is not in the registry")]
    NotFound(String),
    /// A user-owned directory already occupies the install path.
    #[error("{path} is not managed by h4rn")]
    UserOwned {
        /// Directory that would have been replaced.
        path: String,
    },
    /// Two different versions claim the same name.
    #[error("{0}")]
    Conflict(String),
    /// A bundle exceeded a hard limit.
    #[error("{0}")]
    TooLarge(String),
    /// The secret scanner refused a bundle.
    #[error("the secret scanner found {} possible secret(s)", .0.len())]
    Secrets(Vec<SecretFinding>),
    /// Connect was not available or the daemon is signed out.
    #[error("{0}")]
    Sync(String),
    /// A git command failed.
    #[error("git: {0}")]
    Git(String),
}

impl SkillError {
    /// Maps a failure to the wire error with a user-facing hint.
    pub fn into_proto(self) -> ProtoError {
        let hint = match &self {
            Self::UserOwned { path } => Some(format!(
                "Rename or remove {path} by hand; h4rn never overwrites a skill it did not install"
            )),
            Self::Secrets(findings) => Some(secret_hint(findings)),
            Self::NotFound(_) => Some("Refresh the skill list and try again".to_owned()),
            Self::Conflict(_) => {
                Some("Open the conflict and choose which version to keep".to_owned())
            }
            Self::Sync(_) => Some(
                "Sign the daemon in to connect, or retry when the relay is reachable".to_owned(),
            ),
            Self::TooLarge(_) => {
                Some("Remove large assets from the skill directory and try again".to_owned())
            }
            Self::InvalidSource(_) | Self::InvalidSkill { .. } | Self::Git(_) => {
                Some("Check the source path or commit and try again".to_owned())
            }
            Self::Io(_) | Self::Database(_) | Self::Serialisation(_) => {
                Some("Check the daemon log for details".to_owned())
            }
        };
        let code = match self {
            Self::NotFound(_) => ProtoErrorCode::NotFound,
            Self::UserOwned { .. } | Self::Conflict(_) => ProtoErrorCode::Conflict,
            Self::Secrets(_) => ProtoErrorCode::Forbidden,
            Self::InvalidSkill { .. } | Self::InvalidSource(_) | Self::TooLarge(_) => {
                ProtoErrorCode::InvalidRequest
            }
            Self::Sync(_) => ProtoErrorCode::Internal,
            Self::Io(_) | Self::Database(_) | Self::Serialisation(_) | Self::Git(_) => {
                ProtoErrorCode::Internal
            }
        };
        ProtoError {
            code,
            message: self.to_string(),
            hint,
        }
    }
}

fn secret_hint(findings: &[SecretFinding]) -> String {
    let first = findings.first().map(|finding| {
        format!(
            "{}:{} looks like a {}",
            finding.path,
            finding.line,
            finding.kind.as_str()
        )
    });
    match first {
        Some(detail) => format!(
            "Remove the secret ({detail}) and rotate it; skills are synced between machines"
        ),
        None => "Remove secrets from the skill before syncing".to_owned(),
    }
}
