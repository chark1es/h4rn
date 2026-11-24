//! Guest-link requests from the owner (ADR-044).
//!
//! The guest itself never sends a `ClientRequest`: it has no device identity.
//! These are the owner's controls, and every one is scoped by the thread it
//! names. The base URL is chosen here so the link is honest about where it
//! points: the Connect relay when this machine is linked, otherwise the
//! daemon's own network address.

use std::sync::Arc;

use h4rn_protocol::{ClientRequest, GuestSettings, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::{Connection, record_audit};

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GetGuestSharing { .. }
            | ClientRequest::ShareThreadWithGuest { .. }
            | ClientRequest::StopGuestSharing { .. }
            | ClientRequest::SendGuestRequest { .. }
            | ClientRequest::DismissGuestRequest { .. }
            | ClientRequest::GetGuestSettings
            | ClientRequest::SetGuestSettings { .. }
    )
}

/// Handles one owner-side guest request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GetGuestSharing { thread_id } => {
            // With no relay and no loopback listener there is no base to name:
            // an existing link still shows its expiry, but its URL is null.
            let base = share_base(conn).unwrap_or_default();
            let visible = may_read_link_url(conn);
            let sharing = conn
                .state
                .guest
                .sharing(&thread_id, &base, visible)
                .await
                .map_err(internal)?;
            // The URL is the bearer credential, so reading it is an audited
            // action, not just a metadata read.
            if visible && sharing.link.as_ref().is_some_and(|link| link.url.is_some()) {
                conn.audit(
                    "guest.link_viewed",
                    Some("thread"),
                    Some(thread_id.as_ref()),
                    serde_json::json!({"expires_at": sharing.link.as_ref().map(|link| link.expires_at)}),
                );
            }
            Ok(ResponseBody::GuestSharing { sharing })
        }
        ClientRequest::ShareThreadWithGuest { thread_id } => {
            let Some(base) = share_base(conn) else {
                return Err(ProtoError::new(
                    h4rn_protocol::ProtoErrorCode::InvalidRequest,
                    "This machine has no address a guest link can open on. Add a loopback listener, or link this machine to Connect, then share again.",
                    "Start the daemon on a loopback address (or add a loopback --extra-bind), or link this machine to Connect and try again",
                ));
            };
            let created = conn
                .state
                .guest
                .create(&thread_id, &base)
                .await
                .map_err(|error| match error {
                    crate::guest::GuestError::Invalid(message) => ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::InvalidRequest,
                        message,
                        "Turn on Settings → Labs → Guest links, then try again",
                    ),
                    crate::guest::GuestError::NotFound(message) => ProtoError::new(
                        h4rn_protocol::ProtoErrorCode::NotFound,
                        message,
                        "Refresh the thread list and try again",
                    ),
                    other => internal(other),
                })?;
            conn.audit(
                "guest.link_created",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({"expires_at": created.link.expires_at}),
            );
            Ok(ResponseBody::GuestSharing {
                sharing: h4rn_protocol::GuestSharing {
                    thread_id,
                    links_enabled: true,
                    link: Some(created.link),
                },
            })
        }
        ClientRequest::StopGuestSharing { thread_id } => {
            conn.state
                .guest
                .stop_sharing(&thread_id)
                .await
                .map_err(internal)?;
            conn.audit(
                "guest.link_stopped",
                Some("thread"),
                Some(thread_id.as_ref()),
                serde_json::json!({}),
            );
            Ok(ResponseBody::Ack)
        }
        ClientRequest::SendGuestRequest {
            thread_id,
            request_id,
        } => {
            // Send runs in its own task: a connection that drops while the turn
            // is being started must not cancel the send half-way. A cancelled
            // claim would leave a `sending` row, its files and its quota until
            // the next daemon start. The audit line is written inside that task
            // with the origin captured here, so an owner who disconnects after
            // the claim still leaves a record of it.
            let guest = Arc::clone(&conn.state.guest);
            let state = conn.state.clone();
            let origin = conn.origin.clone();
            let send_thread = thread_id.clone();
            let send_request = request_id.clone();
            tokio::spawn(async move {
                let result = guest.send_request(&send_thread, &send_request).await;
                if result.is_ok() {
                    record_audit(
                        &state,
                        &origin,
                        "guest.request_sent",
                        Some("thread"),
                        Some(send_thread.as_ref()),
                        serde_json::json!({"request_id": send_request}),
                    );
                }
                result
            })
            .await
            .map_err(join_failed)?
            .map_err(guest_error)?;
            // The turn's own events carry the accepted message; this answer
            // only says the request was consumed.
            Ok(ResponseBody::Ack)
        }
        ClientRequest::DismissGuestRequest {
            thread_id,
            request_id,
        } => {
            // Dismiss runs in its own task for the same reason as Send: a
            // dropped connection must not cancel the claim and leave the row
            // and its files behind. Its audit line is written there too.
            let guest = Arc::clone(&conn.state.guest);
            let state = conn.state.clone();
            let origin = conn.origin.clone();
            let dismiss_thread = thread_id.clone();
            let dismiss_request = request_id.clone();
            tokio::spawn(async move {
                let result = guest
                    .dismiss_request(&dismiss_thread, &dismiss_request)
                    .await;
                if result.is_ok() {
                    record_audit(
                        &state,
                        &origin,
                        "guest.request_dismissed",
                        Some("thread"),
                        Some(dismiss_thread.as_ref()),
                        serde_json::json!({"request_id": dismiss_request}),
                    );
                }
                result
            })
            .await
            .map_err(join_failed)?
            .map_err(guest_error)?;
            Ok(ResponseBody::Ack)
        }
        ClientRequest::GetGuestSettings => {
            let enabled = conn.state.guest.enabled().await.map_err(internal)?;
            Ok(ResponseBody::GuestSettings {
                settings: GuestSettings { enabled },
            })
        }
        ClientRequest::SetGuestSettings { enabled } => {
            let settings = conn
                .state
                .guest
                .set_enabled(enabled)
                .await
                .map_err(internal)?;
            conn.audit(
                "guest.setting_changed",
                Some("setting"),
                Some("guest_links"),
                serde_json::json!({"enabled": enabled}),
            );
            Ok(ResponseBody::GuestSettings { settings })
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder.
///
/// Guest requests arrive through the thread's journal like every other event,
/// so a connection has nothing extra to follow.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}

/// Where a guest link points.
///
/// A linked machine advertises the relay address, so the guest reaches it the
/// same way a paired device does. Without Connect a direct link is loopback
/// only: the daemon has no TLS listener, and a plaintext LAN URL would put the
/// bearer token on the network. The share dialog says it works only here. A
/// daemon with no listener that serves direct guest routes returns `None`, so
/// the owner is told to add a loopback listener or use Connect instead of
/// receiving a link that cannot answer.
fn share_base(conn: &Connection) -> Option<String> {
    if let Some(remote) = &conn.state.remote
        && let Some(client) = remote.client()
        && client.is_logged_in()
        && let Some(base) = client.public_base_url()
        && let Some(machine) = client.machine_id()
    {
        return Some(format!("{}/guest/{machine}", base.trim_end_matches('/')));
    }
    conn.state.guest.local_base()
}

/// Whether this session may read a guest link's bearer URL.
///
/// The URL is the credential itself. A view-only device may learn that a link
/// exists and when it expires, but only the machine's owner (loopback) or an
/// admin device may copy it.
fn may_read_link_url(conn: &Connection) -> bool {
    let scopes = conn
        .origin
        .device_key
        .as_ref()
        .and_then(|key| conn.state.devices.scopes_for_key(key));
    url_visible_to(conn.origin.is_remote(), scopes.as_deref())
}

/// Whether a session may see a link's bearer URL.
///
/// Loopback is the machine's owner. A remote device needs the admin scope: the
/// URL is the credential, and a view device may not copy it.
fn url_visible_to(is_remote: bool, scopes: Option<&[h4rn_protocol::PairScope]>) -> bool {
    !is_remote || scopes.is_some_and(|scopes| scopes.contains(&h4rn_protocol::PairScope::Admin))
}

fn internal(error: crate::guest::GuestError) -> ProtoError {
    ProtoError::new(
        h4rn_protocol::ProtoErrorCode::Internal,
        error.to_string(),
        "Fix the daemon data directory, then try again",
    )
}

fn guest_error(error: crate::guest::GuestError) -> ProtoError {
    match error {
        crate::guest::GuestError::NotFound(message) => ProtoError::new(
            h4rn_protocol::ProtoErrorCode::NotFound,
            message,
            "Refresh the thread; the request may have been sent or dismissed already",
        ),
        crate::guest::GuestError::Invalid(message) => ProtoError::new(
            h4rn_protocol::ProtoErrorCode::InvalidRequest,
            message,
            "Check the thread and try again",
        ),
        other => internal(other),
    }
}

/// A guest writer task that panicked or was cancelled before its answer.
fn join_failed(error: tokio::task::JoinError) -> ProtoError {
    ProtoError::new(
        h4rn_protocol::ProtoErrorCode::Internal,
        error.to_string(),
        "Check Settings → Labs → Guest links and try again",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::PairScope;

    #[test]
    fn only_an_admin_device_may_read_a_guest_url() {
        assert!(url_visible_to(false, None), "the owner is on this machine");
        assert!(url_visible_to(
            true,
            Some(&[PairScope::View, PairScope::Admin])
        ));
        assert!(!url_visible_to(
            true,
            Some(&[PairScope::View, PairScope::Approve])
        ));
        assert!(!url_visible_to(true, None));
    }

    /// Which owner action a dropped handler must still finish.
    #[derive(Clone, Copy)]
    enum OwnerAction {
        Send,
        Dismiss,
    }

    impl OwnerAction {
        fn request(self, thread_id: &h4rn_protocol::ThreadId, request_id: &str) -> ClientRequest {
            match self {
                Self::Send => ClientRequest::SendGuestRequest {
                    thread_id: thread_id.clone(),
                    request_id: request_id.to_owned(),
                },
                Self::Dismiss => ClientRequest::DismissGuestRequest {
                    thread_id: thread_id.clone(),
                    request_id: request_id.to_owned(),
                },
            }
        }

        fn final_status(self) -> &'static str {
            match self {
                Self::Send => "sent",
                Self::Dismiss => "dismissed",
            }
        }

        fn audit_action(self) -> &'static str {
            match self {
                Self::Send => "\"action\":\"guest.request_sent\"",
                Self::Dismiss => "\"action\":\"guest.request_dismissed\"",
            }
        }
    }

    /// A handler future dropped after its claim still finishes the owner's
    /// action: the detached task owns the row, its files and its audit line.
    ///
    /// Inlining the claim removes the detachment, so dropping the future
    /// leaves the row in `sending` (Send) or the files on disk (Dismiss) and
    /// the assertions below fail. Removing the audit call inside the detached
    /// task fails the audit check for either action.
    async fn a_dropped_owner_action_still_finishes(action: OwnerAction) {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, AtomicU64};

        use h4rn_protocol::{
            Event, GuestAttachmentInput, GuestLinkState, GuestMessageInput, GuestSnapshot,
            HarnessKind, HostId, RuntimeMode,
        };
        use tokio::sync::{Notify, mpsc};

        use crate::config::Config;
        use crate::daemon::DaemonBuilder;
        use crate::guest::{GuestHttpInput, Method};
        use crate::remote::{RemoteAudit, SessionOrigin};
        use crate::server::{Connection, Outbound};
        use crate::threads::NewThread;
        use base64::Engine as _;

        let data = tempfile::tempdir().expect("daemon dir");
        let audit_dir = tempfile::tempdir().expect("audit dir");
        let config = Config {
            data_dir: data.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().expect("bind"),
            extra_bind: None,
            log_level: "off".to_owned(),
            tailscale: crate::remote::TailscaleMode::Off,
            network_access: None,
            network_port: 0,
        };
        let daemon = DaemonBuilder::new(config)
            .with_local_host(HostId::new("host_audit_test"), "audit test")
            .without_title_agents()
            .with_catalog(Arc::new(crate::harnesses::StaticHarnessCatalog::with(
                Arc::new(h4rn_harness::FakeHarness::new()),
            )))
            .build()
            .await
            .expect("build daemon");
        let bound = daemon.bind().await.expect("bind daemon");
        let mut state = bound.app_state().clone();
        // A signed-in daemon owns a remote audit trail; this test installs a
        // local-only one so a remote session's line can be read from disk.
        state.remote = Some(Arc::new(RemoteAudit::local(audit_dir.path())));

        let thread = state
            .manager
            .create_thread(NewThread {
                thread_id: None,
                parent_thread_id: None,
                subagent_id: None,
                host: HostId::new("host_audit_test"),
                harness: HarnessKind::ClaudeCode,
                agent_id: None,
                workspace_path: data.path().display().to_string(),
                model: "fake-model".into(),
                prompt: "first".into(),
                attachments: Vec::new(),
                runtime_mode: RuntimeMode::ApprovalRequired,
                reasoning_effort: None,
                fast: false,
                worktree: None,
            })
            .await
            .expect("create thread");
        state.guest.set_enabled(true).await.expect("enable guests");
        let created = state
            .guest
            .create(&thread.thread_id, "http://127.0.0.1:47800/guest")
            .await
            .expect("create link");
        let token = created.token;

        // Load the page, bind this browser, and submit one waiting message
        // with a file, so Dismiss has something to delete and Send has
        // something to move.
        let page = state
            .guest
            .handle(GuestHttpInput {
                method: Method::Get,
                path: format!("/guest/{token}"),
                public_prefix: None,
                cookie: None,
                content_type: None,
                origin: None,
                host: None,
                fetch_site: None,
                body: Vec::new(),
            })
            .await;
        let cookie = page
            .set_cookie
            .expect("binding cookie")
            .strip_prefix("h4rn_guest=")
            .and_then(|rest| rest.split(';').next())
            .expect("cookie value")
            .to_owned();
        let state_read = state
            .guest
            .handle(GuestHttpInput {
                method: Method::Get,
                path: format!("/guest/{token}/state"),
                public_prefix: None,
                cookie: Some(cookie.clone()),
                content_type: None,
                origin: None,
                host: None,
                fetch_site: None,
                body: Vec::new(),
            })
            .await;
        let snapshot: GuestSnapshot =
            serde_json::from_slice(&state_read.body).expect("guest snapshot");
        assert_eq!(snapshot.state, GuestLinkState::Active);
        let submitted = state
            .guest
            .handle(GuestHttpInput {
                method: Method::Post,
                path: format!("/guest/{token}/messages"),
                public_prefix: None,
                cookie: Some(cookie.clone()),
                content_type: Some("application/json".into()),
                origin: None,
                host: None,
                fetch_site: None,
                body: serde_json::to_vec(&GuestMessageInput {
                    text: "audit me".into(),
                    attachments: vec![GuestAttachmentInput {
                        name: "note.txt".into(),
                        mime: "text/plain".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(b"audit me too"),
                    }],
                })
                .expect("message json"),
            })
            .await;
        assert_eq!(submitted.status, 202);
        let snapshot: GuestSnapshot = serde_json::from_slice(
            &state
                .guest
                .handle(GuestHttpInput {
                    method: Method::Get,
                    path: format!("/guest/{token}/state"),
                    public_prefix: None,
                    cookie: Some(cookie),
                    content_type: None,
                    origin: None,
                    host: None,
                    fetch_site: None,
                    body: Vec::new(),
                })
                .await
                .body,
        )
        .expect("guest snapshot");
        let request_id = snapshot
            .events
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                Event::GuestRequested { request, .. } => Some(request.request_id.clone()),
                _ => None,
            })
            .expect("waiting request");
        let files = data.path().join("guest").join(&request_id);
        assert!(files.exists(), "the waiting request has no files");

        let (tx, _rx) = mpsc::channel::<Outbound>(8);
        let connection = Connection {
            state: state.clone(),
            hello_done: true,
            topics: Arc::new(std::sync::RwLock::new(vec![h4rn_protocol::Topic::All])),
            ready: Arc::new(AtomicBool::new(true)),
            last_seq: Arc::new(AtomicU64::new(0)),
            device: None,
            viewer_id: state.terminals.next_viewer_id(),
            tx,
            origin: SessionOrigin::relay(Some("dev_remote_owner".into()), None),
            watches: HashMap::new(),
            overflowed: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        };

        // Drop the handler while the claim is in flight, the way a closed
        // connection tears its request down.
        let gate = state.guest.pause_next_claim();
        {
            let handler = handle(&connection, action.request(&thread.thread_id, &request_id));
            tokio::pin!(handler);
            tokio::select! {
                _ = gate.arrived() => {}
                result = &mut handler => panic!("the action finished before the gate: {result:?}"),
            }
            // Dropping the future is what a torn-down connection does.
        }
        drop(connection);
        gate.release();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM guest_requests WHERE request_id = ?")
                    .bind(&request_id)
                    .fetch_optional(state.guest.store_pool())
                    .await
                    .expect("read request row");
            let audit = std::fs::read_to_string(audit_dir.path().join("remote-audit.jsonl"))
                .unwrap_or_default();
            if status.as_deref() == Some(action.final_status())
                && !files.exists()
                && audit.contains(action.audit_action())
            {
                assert!(
                    audit.contains("dev_remote_owner"),
                    "the audit line lost the authenticated origin: {audit}"
                );
                assert!(
                    audit.contains(thread.thread_id.as_str()),
                    "the audit line lost the thread: {audit}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a dropped connection left the {:?} unfinished (row {status:?}, files {})",
                std::mem::discriminant(&action),
                files.exists()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A gated Send whose handler future is dropped after the claim still
    /// finishes, because the claim runs inside a detached task.
    #[tokio::test]
    async fn a_dropped_guest_send_still_finishes_its_row_files_and_audit_line() {
        a_dropped_owner_action_still_finishes(OwnerAction::Send).await;
    }

    /// A gated Dismiss whose handler future is dropped after the claim still
    /// finishes, for the same reason.
    #[tokio::test]
    async fn a_dropped_guest_dismiss_still_finishes_its_row_files_and_audit_line() {
        a_dropped_owner_action_still_finishes(OwnerAction::Dismiss).await;
    }

    /// The real share request on a daemon with no listener a guest could
    /// reach says what to do, and creates no link.
    ///
    /// The owner's socket is loopback-only, so a LAN-only daemon cannot be
    /// driven over a WebSocket in a test; this calls the same handler the
    /// socket calls, with the state a LAN-only `bind()` really produces.
    #[tokio::test]
    async fn sharing_with_no_serving_listener_names_the_remedy() {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, AtomicU64};

        use h4rn_protocol::{HarnessKind, HostId, RuntimeMode};
        use tokio::sync::{Notify, mpsc};

        use crate::config::Config;
        use crate::daemon::DaemonBuilder;
        use crate::remote::SessionOrigin;
        use crate::server::{Connection, Outbound};
        use crate::threads::NewThread;

        let Some(lan) = crate::remote::local_ipv4() else {
            eprintln!(
                "no non-loopback address on this machine; the LAN-only case cannot be exercised"
            );
            return;
        };
        let data = tempfile::tempdir().expect("daemon dir");
        let config = Config {
            data_dir: data.path().to_path_buf(),
            bind: std::net::SocketAddr::new(lan, 0),
            extra_bind: None,
            log_level: "off".to_owned(),
            tailscale: crate::remote::TailscaleMode::Off,
            network_access: None,
            network_port: 0,
        };
        let daemon = DaemonBuilder::new(config)
            .with_local_host(HostId::new("host_no_listener"), "no listener")
            .without_title_agents()
            .with_catalog(Arc::new(crate::harnesses::StaticHarnessCatalog::with(
                Arc::new(h4rn_harness::FakeHarness::new()),
            )))
            .build()
            .await
            .expect("build daemon");
        let bound = daemon.bind().await.expect("bind daemon");
        let state = bound.app_state().clone();
        assert_eq!(
            state.guest.local_base(),
            None,
            "a LAN-only daemon named a direct link"
        );
        let thread = state
            .manager
            .create_thread(NewThread {
                thread_id: None,
                parent_thread_id: None,
                subagent_id: None,
                host: HostId::new("host_no_listener"),
                harness: HarnessKind::ClaudeCode,
                agent_id: None,
                workspace_path: data.path().display().to_string(),
                model: "fake-model".into(),
                prompt: "first".into(),
                attachments: Vec::new(),
                runtime_mode: RuntimeMode::ApprovalRequired,
                reasoning_effort: None,
                fast: false,
                worktree: None,
            })
            .await
            .expect("create thread");
        state.guest.set_enabled(true).await.expect("enable guests");

        let (tx, _rx) = mpsc::channel::<Outbound>(8);
        let connection = Connection {
            state: state.clone(),
            hello_done: true,
            topics: Arc::new(std::sync::RwLock::new(vec![h4rn_protocol::Topic::All])),
            ready: Arc::new(AtomicBool::new(true)),
            last_seq: Arc::new(AtomicU64::new(0)),
            device: None,
            viewer_id: state.terminals.next_viewer_id(),
            tx,
            origin: SessionOrigin::local(),
            watches: HashMap::new(),
            overflowed: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        };

        let error = handle(
            &connection,
            ClientRequest::ShareThreadWithGuest {
                thread_id: thread.thread_id.clone(),
            },
        )
        .await
        .expect_err("a daemon with nowhere to point handed out a link");
        assert_eq!(error.code, h4rn_protocol::ProtoErrorCode::InvalidRequest);
        // These are the words the share dialog shows, so the remedies have to
        // be in the message itself.
        for remedy in [
            "no address a guest link can open on",
            "Add a loopback listener",
            "link this machine to Connect",
            "then share again",
        ] {
            assert!(
                error.message.contains(remedy),
                "the refusal did not say {remedy:?}: {}",
                error.message
            );
        }
        let hint = error.hint.expect("the refusal carried a hint");
        assert!(
            hint.contains("loopback") && hint.contains("Connect"),
            "the hint did not name both remedies: {hint}"
        );

        // A refused share leaves no half-created link behind.
        let sharing = state
            .guest
            .sharing(&thread.thread_id, "", true)
            .await
            .expect("sharing");
        assert!(sharing.link.is_none(), "a refused share still made a link");
    }
}
