//! Per-service request handlers and transient-event forwarders.
//!
//! The core request match in [`crate::server`] owns protocol plumbing (hello,
//! subscriptions, terminals, filesystem, blobs). Everything a feature adds on
//! top lives in a module here and is registered exactly once in
//! [`declare_services`]. Adding metering means a new `handlers/<name>.rs`
//! with `matches`, `handle` and `spawn`, plus one line in the list below.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
use tokio::task::JoinHandle;

use crate::server::Connection;

pub(super) mod forge;
pub(super) mod mcp;
pub(super) mod proxy;
pub(super) mod skills;

/// Generates the service enum, request classification, dispatch and forwarder
/// spawning from one registration list.
macro_rules! declare_services {
    ($($variant:ident => $module:ident),* $(,)?) => {
        /// A feature group with its own requests and transient event stream.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(super) enum Service {
            $($variant),*
        }

        impl Service {
            /// Every registered service, in registration order.
            pub(super) const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// The service that owns this request, if any.
            pub(super) fn of(request: &ClientRequest) -> Option<Self> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|service| service.matches(request))
            }

            fn matches(self, request: &ClientRequest) -> bool {
                match self {
                    $(Self::$variant => $module::matches(request),)*
                }
            }

            /// Handles one request owned by this service.
            pub(super) async fn handle(
                self,
                conn: &Connection,
                request: ClientRequest,
            ) -> Result<ResponseBody, ProtoError> {
                match self {
                    $(Self::$variant => $module::handle(conn, request).await,)*
                }
            }

            /// Spawns this service's forwarder for one connection.
            fn spawn(self, conn: &Connection) -> JoinHandle<()> {
                match self {
                    $(Self::$variant => $module::spawn(conn),)*
                }
            }
        }
    };
}

declare_services! {
    Proxy => proxy,
    Forge => forge,
    Skills => skills,
    Mcp => mcp,
}

/// Spawns every registered service's forwarder for one connection.
pub(super) fn spawn_forwarders(conn: &Connection) -> Vec<JoinHandle<()>> {
    Service::ALL
        .iter()
        .copied()
        .map(|service| service.spawn(conn))
        .collect()
}
