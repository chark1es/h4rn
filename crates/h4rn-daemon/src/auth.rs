//! Local access token authentication.
//!
//! The daemon is reachable from a browser, so the token has to travel in a
//! place a browser can set: the `Sec-WebSocket-Protocol` header, or the query
//! string for clients that cannot set headers. A 256-bit random token is stored
//! in the data directory with `0600` permissions and compared in constant time.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use h4rn_protocol::{ProtoError, ProtoErrorCode};
use subtle::ConstantTimeEq;

/// File name of the access token inside the data directory.
const TOKEN_FILE: &str = "access_token";

/// Number of random bytes in a token. 32 bytes = 256 bits, hex encoded.
const TOKEN_BYTES: usize = 32;

/// Owns the on-disk local access token.
///
/// Verification re-reads the file so `h4rnd token --rotate` takes effect on a
/// running daemon at the next WebSocket upgrade, without a restart. Upgrades are
/// rare, so one small read is cheaper than keeping two sources of truth.
#[derive(Clone, Debug)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// Creates a token store rooted at `data_dir`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join(TOKEN_FILE),
        }
    }

    /// Path of the token file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a token has been provisioned.
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Reads the token if it exists.
    pub fn load(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(value) => {
                let value = value.trim().to_owned();
                if value.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(value))
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads the token, creating one on first use.
    pub fn load_or_create(&self) -> std::io::Result<String> {
        if let Some(token) = self.load()? {
            return Ok(token);
        }
        crate::config::create_private_dir(self.path.parent().unwrap_or(Path::new(".")))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let token = generate_token();

        // `create_new` makes concurrent first boots race safely: the loser
        // reads the winner's token instead of overwriting it.
        match open_private_new(&self.path) {
            Ok(mut file) => {
                writeln!(file, "{token}")?;
                file.sync_all()?;
                Ok(token)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => self
                .load()?
                .ok_or_else(|| std::io::Error::other("access token file exists but is empty")),
            Err(error) => Err(error),
        }
    }

    /// Replaces the token with a fresh 256-bit value and returns it.
    ///
    /// The write is atomic on Unix: a private temporary file is renamed over
    /// the old one, so a crash never leaves a half-written token.
    pub fn rotate(&self) -> std::io::Result<String> {
        crate::config::create_private_dir(self.path.parent().unwrap_or(Path::new(".")))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let token = generate_token();
        let temp = self.path.with_extension("tmp");
        {
            let mut file = open_private_new(&temp)?;
            writeln!(file, "{token}")?;
            file.sync_all()?;
        }
        #[cfg(windows)]
        {
            // Windows cannot rename over an existing file; the window between
            // remove and rename is acceptable for a local access token.
            if self.path.exists() {
                std::fs::remove_file(&self.path)?;
            }
        }
        std::fs::rename(&temp, &self.path)?;
        Ok(token)
    }

    /// Constant-time verification of a candidate token.
    pub fn verify(&self, candidate: &str) -> std::io::Result<bool> {
        let Some(expected) = self.load()? else {
            return Ok(false);
        };
        let candidate = candidate.trim();
        if candidate.len() != expected.len() {
            return Ok(false);
        }
        Ok(bool::from(candidate.as_bytes().ct_eq(expected.as_bytes())))
    }

    /// Token path for diagnostics.
    pub fn display_path(&self) -> String {
        self.path.display().to_string()
    }
}

/// Generates a 256-bit token as lowercase hex.
pub fn generate_token() -> String {
    hex::encode(rand::random::<[u8; TOKEN_BYTES]>())
}

/// The outcome of inspecting an upgrade request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthGrant {
    /// The offered subprotocol the server must echo for a browser to accept the
    /// socket. `None` when the client sent a bare token or used `?token=`.
    pub protocol: Option<String>,
}

/// Authenticates a WebSocket upgrade.
///
/// Accepts either `?token=<token>` or a token in `Sec-WebSocket-Protocol`, in
/// any of these shapes:
///
/// - `Sec-WebSocket-Protocol: bearer.<token>`
/// - `Sec-WebSocket-Protocol: bearer, <token>`
/// - `Sec-WebSocket-Protocol: <token>`
///
/// Returns a [`ProtoError`] suitable for a `401` body, never a message that
/// contains the expected token.
pub fn authorize(
    store: &TokenStore,
    query_token: Option<&str>,
    protocol_header: Option<&str>,
) -> Result<AuthGrant, ProtoError> {
    let mut selected_protocol = None;

    let candidate = query_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let (token, protocol) = token_from_protocols(protocol_header?)?;
            selected_protocol = protocol;
            Some(token)
        });

    let Some(candidate) = candidate else {
        return Err(missing_token_error());
    };

    match store.verify(&candidate) {
        Ok(true) => Ok(AuthGrant {
            protocol: selected_protocol,
        }),
        Ok(false) => Err(ProtoError::new(
            ProtoErrorCode::Unauthorized,
            "the local access token is not valid",
            "Run `h4rnd token` on the host to read the current token, then reconnect",
        )),
        Err(error) => {
            tracing::warn!(%error, "could not read the access token file");
            Err(ProtoError::new(
                ProtoErrorCode::Unauthorized,
                "the daemon could not read its access token",
                "Check that the data directory is readable, then run `h4rnd doctor`",
            ))
        }
    }
}

fn missing_token_error() -> ProtoError {
    ProtoError::new(
        ProtoErrorCode::Unauthorized,
        "a local access token is required",
        "Pass `?token=<token>` or `Sec-WebSocket-Protocol: bearer.<token>`; run `h4rnd token` to read it",
    )
}

/// Extracts a token from a `Sec-WebSocket-Protocol` header and the protocol to
/// echo back, if any.
fn token_from_protocols(header: &str) -> Option<(String, Option<String>)> {
    let items: Vec<&str> = header
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect();

    for (index, item) in items.iter().enumerate() {
        let lower = item.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("bearer.") {
            // Recover the original casing of the secret from `item`.
            let token = &item[item.len() - rest.len()..];
            return Some((token.to_owned(), Some((*item).to_owned())));
        }
        if let Some(rest) = lower.strip_prefix("bearer ") {
            let token = &item[item.len() - rest.len()..];
            return Some((token.to_owned(), Some("bearer".to_owned())));
        }
        if lower == "bearer" {
            if let Some(token) = items.get(index + 1) {
                return Some(((*token).to_owned(), Some("bearer".to_owned())));
            }
        }
    }

    // A bare token is accepted for local CLI clients and tests.
    if items.len() == 1 {
        return Some((items[0].to_owned(), None));
    }
    None
}

#[cfg(unix)]
fn open_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_of_lowercase_hex() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(token, token.to_ascii_lowercase());
        assert_ne!(token, generate_token());
    }

    #[test]
    fn creates_once_and_verifies_in_constant_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        assert!(!store.exists());

        let token = store.load_or_create().unwrap();
        assert_eq!(store.load_or_create().unwrap(), token);
        assert!(store.verify(&token).unwrap());
        assert!(!store.verify("not-the-token").unwrap());
        assert!(!store.verify("").unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the token must not be group/world readable"
            );
        }
    }

    #[test]
    fn rotate_replaces_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let first = store.load_or_create().unwrap();
        let second = store.rotate().unwrap();
        assert_ne!(first, second);
        assert!(store.verify(&second).unwrap());
        assert!(!store.verify(&first).unwrap());
    }

    #[test]
    fn protocol_header_shapes_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let token = store.load_or_create().unwrap();

        for header in [
            format!("bearer.{token}"),
            format!("bearer, {token}"),
            format!("Bearer.{token}"),
            token.clone(),
        ] {
            let grant = authorize(&store, None, Some(&header)).unwrap();
            assert!(grant.protocol.is_some() || header == token);
        }

        assert!(authorize(&store, Some(&token), None).is_ok());
        assert!(authorize(&store, Some("wrong"), None).is_err());
        assert!(authorize(&store, None, None).is_err());
        assert!(authorize(&store, None, Some("bearer.wrong")).is_err());
    }

    #[test]
    fn authorization_errors_do_not_leak_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let token = store.load_or_create().unwrap();
        let error = authorize(&store, Some("wrong"), None).unwrap_err();
        assert_eq!(error.code, ProtoErrorCode::Unauthorized);
        assert!(!error.message.contains(&token));
        assert!(!error.hint.unwrap_or_default().contains(&token));
    }
}
