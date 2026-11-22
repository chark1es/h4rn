//! Path resolution and the service's typed failures.
//!
//! Every path that arrives from a client is relative and untrusted. The rules
//! here are the security boundary for IDE mode:
//!
//! 1. reject absolute paths, prefixes and `..` components before touching disk;
//! 2. join the remaining components onto the authorised root;
//! 3. canonicalise the existing part of the target (following symlinks);
//! 4. refuse anything whose canonical form is not inside the canonical root.
//!
//! Step 3 is what closes the symlink-escape hole: a link inside the workspace
//! that points at `/etc` canonicalises to `/etc` and fails step 4.

use std::path::{Component, Path, PathBuf};

use h4rn_transport::DEFAULT_MAX_FRAME_BYTES;
use thiserror::Error;

/// Largest directory listing the daemon returns in one response.
pub const MAX_ENTRIES: usize = 2_000;
/// Largest file the editor will open, in bytes.
pub const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Largest file the preview embeds as base64 in one response, in bytes.
///
/// Base64 inflates by a third, so 360 KiB becomes 480 KiB and the JSON
/// envelope still fits inside the secure transport's 512 KiB frame. A larger
/// asset comes back with `FsOmitted::TooLarge` instead of failing the
/// whole preview.
pub const MAX_INLINE_BYTES: u64 = 360 * 1024;
/// Bytes kept clear of the transport's hard frame limit for the JSON envelope
/// and any framing the transport adds around it.
pub const FRAME_HEADROOM_BYTES: usize = 8 * 1024;
/// Largest serialized filesystem response the daemon sends in one frame.
///
/// A frame over the transport's limit is refused with `FrameTooLarge`, which
/// closes the whole paired session, so the daemon measures the serialized
/// response (escaping included) and omits the payload instead.
pub const MAX_RESPONSE_BYTES: usize = DEFAULT_MAX_FRAME_BYTES - FRAME_HEADROOM_BYTES;
/// Largest content the editor will write, in bytes.
pub const MAX_WRITE_BYTES: usize = 8 * 1024 * 1024;

/// A filesystem-service failure, mapped to a protocol error at the server edge.
#[derive(Debug, Error)]
pub enum FsError {
    /// The path is outside the authorised workspace (or a symlink escapes it).
    #[error("{0}")]
    Forbidden(String),
    /// The path or workspace does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The operation collides with current state.
    #[error("{0}")]
    Conflict(String),
    /// The request is malformed.
    #[error("{0}")]
    Invalid(String),
    /// The file or content exceeds a limit.
    #[error("{0}")]
    TooLarge(String),
    /// The file is not editable text.
    #[error("{0}")]
    Binary(String),
    /// This build or host cannot serve the request.
    #[error("{0}")]
    Unsupported(String),
    /// An operating-system failure.
    #[error("{0}")]
    Io(String),
}

/// Whether the target must already exist or may be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveMode {
    /// Canonicalise the target itself; it must exist.
    Existing,
    /// Canonicalise the nearest existing ancestor and append the new components.
    Create,
}

/// Normalises a client-supplied relative path without touching disk.
///
/// The result is a relative path built only from normal components. `.` is
/// ignored, `..`, roots and Windows prefixes are rejected.
pub fn normalize_relative(rel: &str) -> Result<PathBuf, FsError> {
    if rel.contains('\0') {
        return Err(FsError::Invalid("path contains a NUL byte".into()));
    }
    // Accept either separator so a path copied from a Windows client still
    // means the same components. On Unix a literal backslash in a file name is
    // vanishingly rare and never worth an escape risk.
    let normalized = rel.replace('\\', "/");
    let mut path = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(FsError::Forbidden(
                    "path traversal (`..`) is not allowed".into(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(FsError::Forbidden(
                    "absolute paths are not allowed; use a workspace-relative path".into(),
                ));
            }
        }
    }
    Ok(path)
}

/// Resolves a relative path inside `root` and proves it stays inside.
///
/// `root` must already be canonical (the service canonicalises roots when they
/// are registered).
pub fn resolve_inside(root: &Path, rel: &str, mode: ResolveMode) -> Result<PathBuf, FsError> {
    let relative = normalize_relative(rel)?;
    let joined = root.join(&relative);

    if mode == ResolveMode::Existing {
        let canonical = joined
            .canonicalize()
            .map_err(|error| FsError::NotFound(format!("{rel:?}: {error}")))?;
        ensure_inside(root, &canonical)?;
        return Ok(canonical);
    }

    if joined.exists() {
        let canonical = joined
            .canonicalize()
            .map_err(|error| FsError::NotFound(format!("{rel:?}: {error}")))?;
        ensure_inside(root, &canonical)?;
        return Ok(canonical);
    }

    // Walk up to the nearest existing ancestor, canonicalise it, then append
    // the components that do not exist yet. Any symlink in the existing part is
    // resolved here, which is exactly where an escape would hide.
    let mut ancestor = joined.clone();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        // `exists()` follows links, so a dangling symlink reads as "missing".
        // Creating through it would write wherever it points, so refuse.
        if ancestor
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(FsError::Forbidden(
                "path is a dangling symlink; refusing to create through it".into(),
            ));
        }
        let Some(name) = ancestor.file_name().map(|name| name.to_owned()) else {
            return Err(FsError::Forbidden(
                "path has no existing ancestor inside the workspace".into(),
            ));
        };
        missing.push(name);
        if !ancestor.pop() {
            return Err(FsError::Forbidden(
                "path has no existing ancestor inside the workspace".into(),
            ));
        }
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|error| FsError::NotFound(error.to_string()))?;
    ensure_inside(root, &canonical_ancestor)?;
    let mut resolved = canonical_ancestor;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    ensure_inside(root, &resolved)?;
    Ok(resolved)
}

/// Resolves a path for a diff read.
///
/// A diff is allowed to target something that no longer exists: a deletion is
/// exactly what it must show. So this resolves like [`ResolveMode::Create`],
/// walking to the nearest existing ancestor and appending missing components.
///
/// The one difference is the leaf: a diff must not follow a link that is
/// dangling or points outside the workspace. Reading through such a link either
/// fails or exposes a file the workspace boundary forbids. Those leaves resolve
/// to `None`, so the diff reports the path with `after: None` (the same way a
/// snapshot records a link it could not read) instead of erroring. A link that
/// resolves inside the workspace is followed exactly as a read would follow it.
///
/// Writes and creates must keep using [`ResolveMode::Create`], which refuses
/// dangling and escaping links outright.
pub fn resolve_for_diff(root: &Path, rel: &str) -> Result<Option<PathBuf>, FsError> {
    let relative = normalize_relative(rel)?;
    let joined = root.join(&relative);
    let leaf_is_symlink = joined
        .symlink_metadata()
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    if leaf_is_symlink {
        return match joined.canonicalize() {
            Ok(canonical) if ensure_inside(root, &canonical).is_ok() => Ok(Some(canonical)),
            // Dangling, looping or escaping: do not follow it, and do not read
            // through it. The path itself stays workspace-relative.
            _ => Ok(None),
        };
    }
    resolve_inside(root, rel, ResolveMode::Create).map(Some)
}

fn ensure_inside(root: &Path, candidate: &Path) -> Result<(), FsError> {
    if candidate == root || candidate.starts_with(root) {
        Ok(())
    } else {
        Err(FsError::Forbidden(format!(
            "{} is outside the workspace",
            candidate.display()
        )))
    }
}

/// Renders a path as a workspace-relative string with `/` separators.
pub fn relative_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        assert!(normalize_relative("../etc/passwd").is_err());
        assert!(normalize_relative("src/../../etc").is_err());
        assert!(normalize_relative("/etc/passwd").is_err());
        #[cfg(windows)]
        assert!(normalize_relative("C:\\Windows").is_err());
        assert_eq!(
            normalize_relative("./src//main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(normalize_relative("").unwrap(), PathBuf::new());
    }

    #[test]
    fn relative_of_uses_forward_slashes() {
        let root = Path::new("/repo");
        assert_eq!(relative_of(root, &root.join("a").join("b")), "a/b");
    }
}
